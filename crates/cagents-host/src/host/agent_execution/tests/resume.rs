//! Resuming a stopped conversation without a window: `cagents resume`, and
//! an agent's next prompt after a reboot.
use super::*;
use crate::{
    agent_execution::directory::Scope as Sessions,
    agent_execution::resume::{Resumed, Scope, StoppedSessions},
    host::agent_execution::resume::{AfterBoot, Pass},
    model::{BootId, CardKind},
};

/// Whether this test of this module ran in its own child process, which the
/// test then leaves to report.
fn in_child(test: &str) -> bool {
    isolated(&format!("host::agent_execution::tests::resume::{test}")).is_some()
}

/// The fixture's agent, run once and stopped with its daemon, then a new
/// daemon on `boot` over the same registry, as after a restart, and the
/// conversation its window saved.
fn stopped_agent(boot: BootId) -> (SharedHost, Accepted, String) {
    run_and_stop(boot, true)
}

/// The same, with the conversation saved in a pane when `shown`, or else in
/// the agent's own record, as for an agent no window ever showed.
fn run_and_stop(boot: BootId, shown: bool) -> (SharedHost, Accepted, String) {
    let (host, request, path) = fixture();
    let accepted = submit(&host, &Authority::default(), request).unwrap();
    assert_eq!(settled(&host, accepted.run.id).state, State::Succeeded);
    let conversation = if shown {
        show_in_a_channel(&host, &path, &accepted)
    } else {
        let (chat, confirmed) = confirmed_conversation(&host, &path, &accepted);
        let saved = HostedSession::Chat(session_of(&host, accepted.agent.id));
        registry(&host)
            .unwrap()
            .lock()
            .unwrap()
            .bind_agent(accepted.agent.id, Some(&saved), HostedSession::Chat(chat))
            .unwrap();
        confirmed
    };
    crate::host::finish_chat_shutdown(&host).unwrap();
    drop(host);
    (daemon_on(boot), accepted, conversation)
}

/// A new daemon on `boot` over the fixture's registry.
fn daemon_on(boot: BootId) -> SharedHost {
    let path = std::path::PathBuf::from(std::env::var_os("CAGENTS_EXECUTION_FIXTURE").unwrap())
        .join("registry.sqlite3");
    let mut state = HostState::new(HostSessionProof {
        host_session_id: HostSessionId::new("restarted-resume-fixture"),
        boot_id: boot,
        host_incarnation: HostIncarnation::new("restarted-resume"),
    });
    state.install_model(Registry::at(&path).unwrap());
    Arc::new(crate::host::state::HostLock::new(state))
}

/// Shows the agent's conversation in a channel's pane, saved with the
/// conversation its provider confirmed, as the window showing it saves it.
fn show_in_a_channel(host: &SharedHost, path: &Path, accepted: &Accepted) -> String {
    let (chat, confirmed) = confirmed_conversation(host, path, accepted);
    let project = path.parent().unwrap().join("project");
    pane_in_a_new_channel(
        host,
        &project,
        crate::model::CardId::from_uuid(accepted.agent.id.as_uuid()),
        chat,
    );
    confirmed
}

/// The agent's chat with the conversation its provider confirmed, whose
/// history Claude saved, and that conversation.
fn confirmed_conversation(
    host: &SharedHost,
    path: &Path,
    accepted: &Accepted,
) -> (chat::Session, String) {
    let root = path.parent().unwrap();
    let project = root.join("project");
    let mut chat = session_of(host, accepted.agent.id);
    let confirmed = host.lock().unwrap().chats[&chat.id]
        .handle
        .confirmed_session_id()
        .expect("the fixture provider confirmed its conversation");
    chat.launch.session_id = Some(confirmed.clone());
    // The history Claude saved, which a resume reads before it starts.
    let sanitized: String = project
        .to_str()
        .unwrap()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let history = root.join("home/.claude/projects").join(sanitized);
    std::fs::create_dir_all(&history).unwrap();
    std::fs::write(
        history.join(format!("{confirmed}.jsonl")),
        format!(
            "{}\n",
            serde_json::json!({"type":"user","uuid":"u1","sessionId":confirmed,
                "message":{"role":"user","content":"Read the owned fixture"}})
        ),
    )
    .unwrap();
    (chat, confirmed)
}

/// Shows `chat` in pane `card` of a new channel of `project`.
fn pane_in_a_new_channel(
    host: &SharedHost,
    project: &Path,
    card: crate::model::CardId,
    chat: chat::Session,
) {
    use crate::{agent_execution::Placement, model::ChannelId, workspace_model::views::Pane};
    let channel = ChannelId::new();
    let registry = registry(host).unwrap();
    let mut model = registry.lock().unwrap();
    model
        .apply(&ModelEdit::CreateChannel {
            id: channel,
            repository: project.to_path_buf(),
            title: "Recovery".into(),
            title_source: Default::default(),
            checkout: None,
        })
        .unwrap();
    model
        .apply(&ModelEdit::SeedView {
            scope: Placement::Channel(channel),
            closed: vec![],
            panes: vec![Pane {
                id: card,
                title: "Agent".into(),
                user_title: None,
                pending_terminal_start: false,
                width_fraction: 1.0,
                kind: CardKind::Chat(chat),
                scope: Some(project.to_path_buf()),
            }],
        })
        .unwrap();
}

fn session_of(host: &SharedHost, agent: AgentId) -> chat::Session {
    match registry(host)
        .unwrap()
        .lock()
        .unwrap()
        .agent(agent)
        .unwrap()
        .session
    {
        Some(HostedSession::Chat(session)) => session,
        other => panic!("the agent has no conversation: {other:?}"),
    }
}

fn stopped_in(host: &SharedHost, scope: Scope) -> StoppedSessions {
    match process(host, Authority::default(), Operation::Stopped(scope)).unwrap() {
        Reply::Stopped(stopped) => stopped,
        other => panic!("{other:?}"),
    }
}

fn resume(host: &SharedHost, handle: String) -> Result<Resumed, String> {
    process(host, Authority::default(), Operation::Resume(handle)).map(|reply| match reply {
        Reply::Resumed(resumed) => resumed,
        other => panic!("{other:?}"),
    })
}

/// The chat each pane in the workspace shows.
fn panes(host: &SharedHost) -> Vec<chat::Session> {
    registry(host)
        .unwrap()
        .lock()
        .unwrap()
        .model()
        .unwrap()
        .views
        .into_iter()
        .flat_map(|view| view.panes)
        .filter_map(|pane| match pane.kind {
            CardKind::Chat(session) => Some(session),
            _ => None,
        })
        .collect()
}

/// After a restart on the same boot, the person resumes the stopped agent by
/// name. It reopens in place on this daemon's proof, keeps its agent, and its
/// pane shows the running process, so a window that opens later does not
/// start it a second time. Resuming again starts nothing.
#[test]
fn the_person_resumes_a_stopped_agent_in_place_and_its_pane_and_agent_follow() {
    if in_child("the_person_resumes_a_stopped_agent_in_place_and_its_pane_and_agent_follow") {
        return;
    }
    let (host, accepted, conversation) = stopped_agent(crate::daemon::read_boot_id().unwrap());
    let before = session_of(&host, accepted.agent.id);
    let found = stopped_in(&host, Scope::Sessions(Sessions::All));
    assert_eq!(found.in_app_only, 0, "{found:?}");
    let listed = found.sessions;
    assert_eq!(
        listed.iter().map(|stopped| stopped.id).collect::<Vec<_>>(),
        [before.id],
        "{listed:?}"
    );
    assert_eq!(listed[0].agent, Some(accepted.agent.id));

    // A prompt cannot tell whether the earlier process still writes the
    // conversation, so it sends nothing and names the command.
    let refused = submit(
        &host,
        &Authority::default(),
        Submit {
            name: None,
            destination: Destination::Existing(accepted.agent.id),
            prompt: "Before resuming".into(),
            idempotency_key: "before-resume".into(),
        },
    )
    .unwrap();
    let refused = settled(&host, refused.run.id);
    assert_eq!(refused.state, State::Failed, "{refused:?}");
    let reason = refused.result.diagnostic.unwrap_or_default();
    assert!(
        reason.contains(&format!("cagents resume {}", before.id)),
        "{reason}"
    );
    assert_eq!(
        stopped_in(&host, Scope::Sessions(Sessions::All))
            .sessions
            .len(),
        1
    );

    // Too short a handle names nothing.
    assert!(resume(&host, before.id.to_string()[..3].into()).is_err());
    let resumed = resume(&host, accepted.agent.id.to_string()[..8].into()).unwrap();
    assert_eq!(resumed.stopped, before.id);
    assert_eq!(
        resumed.session, before.id,
        "a conversation reopens in place"
    );
    assert!(
        !resumed.already_running && resumed.note.is_none(),
        "{resumed:?}"
    );
    assert_eq!(resumed.agent, Some(accepted.agent.id));
    let proof = host.lock().unwrap().proof().clone();
    let after = session_of(&host, accepted.agent.id);
    assert_eq!(
        after.proof.as_ref(),
        Some(&proof),
        "the agent follows the new process"
    );
    assert_eq!(after.launch.session_id, Some(conversation.clone()));
    assert!(
        host.lock()
            .unwrap()
            .chats
            .get(&before.id)
            .is_some_and(|record| !record.handle.is_completed())
    );
    let pane = panes(&host)
        .into_iter()
        .find(|pane| pane.id == before.id)
        .unwrap();
    assert_eq!(
        pane.proof.as_ref(),
        Some(&proof),
        "the pane names the process that runs"
    );
    assert_eq!(pane.launch.session_id, Some(conversation.clone()));
    assert!(
        stopped_in(&host, Scope::Sessions(Sessions::All))
            .sessions
            .is_empty()
    );

    let again = resume(&host, before.id.to_string()).unwrap();
    assert!(again.already_running, "{again:?}");

    // The agent takes a prompt again.
    let next = submit(
        &host,
        &Authority::default(),
        Submit {
            name: None,
            destination: Destination::Existing(accepted.agent.id),
            prompt: "Carry on".into(),
            idempotency_key: "after-resume".into(),
        },
    )
    .unwrap();
    assert_eq!(settled(&host, next.run.id).state, State::Succeeded);
}

/// A prompt for an agent whose conversation an earlier boot ran reopens it by
/// itself, whichever provider runs it: that boot's processes are all gone.
#[test]
fn a_prompt_reopens_a_conversation_an_earlier_boot_ran() {
    if in_child("a_prompt_reopens_a_conversation_an_earlier_boot_ran") {
        return;
    }
    let (host, accepted, conversation) = stopped_agent(BootId::new("a-later-boot"));
    let before = session_of(&host, accepted.agent.id);
    let next = submit(
        &host,
        &Authority::default(),
        Submit {
            name: None,
            destination: Destination::Existing(accepted.agent.id),
            prompt: "Carry on after the reboot".into(),
            idempotency_key: "after-reboot".into(),
        },
    )
    .unwrap();
    let run = settled(&host, next.run.id);
    assert_eq!(run.state, State::Succeeded, "{run:?}");
    let after = session_of(&host, accepted.agent.id);
    assert_eq!(after.id, before.id);
    assert_eq!(after.launch.session_id, Some(conversation.clone()));
    assert_eq!(after.proof.as_ref(), Some(host.lock().unwrap().proof()));
}

/// Resuming names exactly one stopped conversation, and says what to do when
/// it cannot.
#[test]
fn resume_refuses_what_it_cannot_name_exactly() {
    if in_child("resume_refuses_what_it_cannot_name_exactly") {
        return;
    }
    let (host, _, _) = stopped_agent(crate::daemon::read_boot_id().unwrap());
    let error = resume(&host, "ffffffff".into()).unwrap_err();
    assert!(error.contains("No session or agent matches"), "{error}");
    let error = resume(&host, "ab".into()).unwrap_err();
    assert!(error.contains("at least 4 characters"), "{error}");
}

/// A successor of the agent's conversation that a pane prepared but never
/// started: prepared again on resume, as the pane does, rather than started
/// in place.
fn unstarted_successor(of: &chat::Session) -> chat::Session {
    let mut successor = of.clone();
    successor.id = crate::model::TerminalId::new();
    successor.proof = None;
    successor.terminal = None;
    successor.native_continuity = chat::NativeContinuity::ReconnectOnly;
    assert!(chat::is_unstarted_resume_successor(&successor));
    successor
}

/// An agent named by its ID resumes its own session even when its lineage
/// holds another stopped conversation. A successor of that conversation that
/// never started is prepared again, as its pane's Resume does, so while the
/// agent's process runs the conversation, resuming the successor starts no
/// second writer.
#[test]
fn resume_starts_no_second_writer_for_a_successor_that_never_started() {
    if in_child("resume_starts_no_second_writer_for_a_successor_that_never_started") {
        return;
    }
    let (host, accepted, conversation) = stopped_agent(crate::daemon::read_boot_id().unwrap());
    let project = std::path::PathBuf::from(std::env::var_os("CAGENTS_EXECUTION_FIXTURE").unwrap())
        .join("project");
    let mut before = session_of(&host, accepted.agent.id);
    before.launch.session_id = Some(conversation.clone());
    // One of the lineage, and one no lineage records.
    let in_lineage = unstarted_successor(&before);
    let registry = registry(&host).unwrap();
    registry
        .lock()
        .unwrap()
        .succeed(in_lineage.id, before.id, None, |_| false)
        .unwrap();
    let elsewhere = unstarted_successor(&before);
    for successor in [&in_lineage, &elsewhere] {
        pane_in_a_new_channel(
            &host,
            &project,
            crate::model::CardId::new(),
            successor.clone(),
        );
    }

    let resumed = resume(&host, accepted.agent.id.to_string()[..8].into()).unwrap();
    assert_eq!(resumed.stopped, before.id, "{resumed:?}");
    assert_eq!(resumed.session, before.id);
    assert!(
        host.lock()
            .unwrap()
            .chats
            .get(&before.id)
            .is_some_and(|record| !record.handle.is_completed())
    );

    let refused = resume(&host, elsewhere.id.to_string()).unwrap_err();
    assert!(
        refused.contains("may still be writing to this Claude conversation"),
        "{refused}"
    );
    assert_eq!(
        host.lock().unwrap().chats.len(),
        1,
        "nothing else runs the conversation"
    );
}

/// An agent no pane shows is listed in the scope it was placed in, and only
/// while its session is the one its lineage last continued.
#[test]
fn an_agent_no_pane_shows_is_listed_in_its_own_scope_while_it_is_latest() {
    if in_child("an_agent_no_pane_shows_is_listed_in_its_own_scope_while_it_is_latest") {
        return;
    }
    let (host, accepted, _) = run_and_stop(crate::daemon::read_boot_id().unwrap(), false);
    let session = session_of(&host, accepted.agent.id);
    let project = std::path::PathBuf::from(std::env::var_os("CAGENTS_EXECUTION_FIXTURE").unwrap())
        .join("project");
    let ids = |scope| {
        stopped_in(&host, scope)
            .sessions
            .iter()
            .map(|stopped| stopped.id)
            .collect::<Vec<_>>()
    };
    assert_eq!(ids(Scope::Sessions(Sessions::All)), [session.id]);
    assert_eq!(
        ids(Scope::Sessions(Sessions::Project(project.clone()))),
        [session.id]
    );
    let agent = registry(&host)
        .unwrap()
        .lock()
        .unwrap()
        .agent(accepted.agent.id)
        .unwrap();
    if let crate::agent_execution::Placement::Channel(channel) = agent.placement {
        assert_eq!(ids(Scope::Channel(Some(channel))), [session.id]);
    }
    // Another channel of the project is not where it was placed.
    let elsewhere = crate::model::ChannelId::new();
    registry(&host)
        .unwrap()
        .lock()
        .unwrap()
        .apply(&ModelEdit::CreateChannel {
            id: elsewhere,
            repository: project,
            title: "Elsewhere".into(),
            title_source: Default::default(),
            checkout: None,
        })
        .unwrap();
    assert!(ids(Scope::Channel(Some(elsewhere))).is_empty());

    // A successor its agent did not take over continued the conversation
    // since, so reopening the agent's session would be stale.
    registry(&host)
        .unwrap()
        .lock()
        .unwrap()
        .succeed(crate::model::TerminalId::new(), session.id, None, |_| false)
        .unwrap();
    assert!(ids(Scope::Sessions(Sessions::All)).is_empty());
}

/// A message to a stopped session names the command that resumes it to a
/// sender that may run it, and otherwise says the person can.
#[test]
fn a_message_to_a_stopped_session_names_resume_only_to_who_may_run_it() {
    use crate::agent_execution::mailbox::{Outgoing, To};
    if in_child("a_message_to_a_stopped_session_names_resume_only_to_who_may_run_it") {
        return;
    }
    let (host, accepted, conversation) = stopped_agent(crate::daemon::read_boot_id().unwrap());
    let project = std::path::PathBuf::from(std::env::var_os("CAGENTS_EXECUTION_FIXTURE").unwrap())
        .join("project");
    // A chat only its pane holds, which no agent owns.
    let mut paned = session_of(&host, accepted.agent.id);
    paned.id = crate::model::TerminalId::new();
    paned.launch.session_id = Some(conversation);
    pane_in_a_new_channel(&host, &project, crate::model::CardId::new(), paned.clone());
    let send = |authority: Authority| {
        process(
            &host,
            authority,
            Operation::SendMessage(Outgoing {
                to: To::Session(paned.id.to_string()),
                body: "Are you there?".into(),
                wait_ms: None,
            }),
        )
        .unwrap_err()
    };
    let person = send(Authority::default());
    assert!(
        person.contains(&format!(
            "Resume its conversation first with `cagents resume {}`",
            paned.id
        )),
        "{person}"
    );

    // A session that did not start it may not resume it.
    let sender = submit(
        &host,
        &Authority::default(),
        Submit {
            name: None,
            destination: Destination::New(Spec {
                provider: Provider::Claude,
                account: None,
                target: Target {
                    repository: project.clone(),
                    directory: project.clone(),
                    placement: None,
                },
                surface: Surface::Chat,
                model: None,
                permission_mode: None,
                reasoning_effort: None,
            }),
            prompt: "Read the owned fixture".into(),
            idempotency_key: "sender".into(),
        },
    )
    .unwrap();
    assert_eq!(settled(&host, sender.run.id).state, State::Succeeded);
    let sending = session_of(&host, sender.agent.id).id;
    let authority = registry(&host)
        .unwrap()
        .lock()
        .unwrap()
        .retained_agent_authority(sending)
        .unwrap();
    let agent = send(authority);
    assert!(
        agent.contains(&format!(
            "The person can resume its conversation with `cagents resume {}`",
            paned.id
        )),
        "{agent}"
    );
    assert!(!agent.contains("Resume its conversation first"), "{agent}");
}

/// Whether the person asked for sessions to resume when this computer starts.
fn opt_in(resume: bool) {
    let path = crate::instance::settings_path().unwrap();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, format!("resume_sessions_after_reboot = {resume}\n")).unwrap();
}

/// The daemon's own resume as it runs once the daemon serves.
fn after_boot(host: &SharedHost) -> AfterBoot {
    crate::host::agent_execution::resume::resume_once_per_boot(&Arc::downgrade(host))
}

fn ran(outcome: AfterBoot) -> Pass {
    match outcome {
        AfterBoot::Ran(pass) => pass,
        other => panic!("the resume did not run: {other:?}"),
    }
}

/// The boot whose resume ran to the end, as the daemon recorded it.
fn handled_boot() -> Option<String> {
    let path = crate::instance::project_dirs()
        .unwrap()
        .data_local_dir()
        .join("resumed-boot");
    std::fs::read_to_string(path)
        .ok()
        .map(|boot| boot.trim().to_owned())
}

fn stopped_ids(host: &SharedHost) -> Vec<crate::model::TerminalId> {
    stopped_in(host, Scope::Sessions(Sessions::All))
        .sessions
        .iter()
        .map(|stopped| stopped.id)
        .collect()
}

/// The first daemon after a reboot reopens, by itself, what the earlier boot
/// ran, as `cagents resume --all` would; the agent follows, and the boot is
/// recorded as resumed.
#[test]
fn after_a_reboot_the_daemon_resumes_what_an_earlier_boot_ran() {
    if in_child("after_a_reboot_the_daemon_resumes_what_an_earlier_boot_ran") {
        return;
    }
    opt_in(true);
    let (host, accepted, conversation) = stopped_agent(BootId::new("a-later-boot"));
    let before = session_of(&host, accepted.agent.id);
    let pass = ran(after_boot(&host));
    assert!(pass.failed.is_empty() && pass.left.is_empty(), "{pass:?}");
    assert!(!pass.interrupted);
    assert_eq!(
        pass.resumed
            .iter()
            .map(|resumed| resumed.stopped)
            .collect::<Vec<_>>(),
        [before.id]
    );
    let after = session_of(&host, accepted.agent.id);
    assert_eq!(after.launch.session_id, Some(conversation));
    assert_eq!(after.proof.as_ref(), Some(host.lock().unwrap().proof()));
    assert!(stopped_ids(&host).is_empty());
    assert_eq!(handled_boot().as_deref(), Some("a-later-boot"));
}

/// An agent started from a shell, which no pane shows, resumes too.
#[test]
fn after_a_reboot_an_agent_no_pane_shows_resumes_too() {
    if in_child("after_a_reboot_an_agent_no_pane_shows_resumes_too") {
        return;
    }
    opt_in(true);
    let (host, accepted, _) = run_and_stop(BootId::new("a-later-boot"), false);
    assert!(panes(&host).is_empty());
    let before = session_of(&host, accepted.agent.id);
    let pass = ran(after_boot(&host));
    assert!(pass.failed.is_empty() && pass.left.is_empty(), "{pass:?}");
    assert_eq!(
        pass.resumed
            .iter()
            .map(|resumed| (resumed.stopped, resumed.agent))
            .collect::<Vec<_>>(),
        [(before.id, Some(accepted.agent.id))]
    );
    let after = session_of(&host, accepted.agent.id);
    assert_eq!(after.proof.as_ref(), Some(host.lock().unwrap().proof()));
}

/// The first daemon of a boot cannot tell whether a conversation this boot
/// ran still has a writer, so it leaves it for the person.
#[test]
fn a_conversation_this_boot_ran_is_not_resumed_by_itself() {
    if in_child("a_conversation_this_boot_ran_is_not_resumed_by_itself") {
        return;
    }
    opt_in(true);
    let (host, accepted, _) = stopped_agent(crate::daemon::read_boot_id().unwrap());
    let before = session_of(&host, accepted.agent.id);
    let pass = ran(after_boot(&host));
    assert!(
        pass.resumed.is_empty() && pass.failed.is_empty() && pass.left.is_empty(),
        "{pass:?}"
    );
    assert_eq!(stopped_ids(&host), [before.id]);
}

/// The fixture's agent shown in a pane paired with its provider's terminal,
/// which only the app reopens, stopped with its daemon, then a new daemon on
/// `boot`.
fn stopped_paired_chat(boot: BootId) -> SharedHost {
    let (host, request, path) = fixture();
    let accepted = submit(&host, &Authority::default(), request).unwrap();
    assert_eq!(settled(&host, accepted.run.id).state, State::Succeeded);
    let (mut chat, _) = confirmed_conversation(&host, &path, &accepted);
    let recipe = chat.launch.recipe.clone();
    chat.terminal = Some(Box::new(crate::model::TerminalSession {
        id: crate::model::TerminalId::new(),
        owned_codex_listener: None,
        recovery_origin: None,
        predecessor: None,
        launch: recipe.clone(),
        transcript_path: None,
        resume_hint: None,
        continuity: crate::model::TerminalContinuity::RelaunchAvailable {
            recipe,
            last_exit: None,
        },
    }));
    pane_in_a_new_channel(
        &host,
        &path.parent().unwrap().join("project"),
        crate::model::CardId::from_uuid(accepted.agent.id.as_uuid()),
        chat,
    );
    crate::host::finish_chat_shutdown(&host).unwrap();
    drop(host);
    daemon_on(boot)
}

/// A chat paired with its provider's terminal is left to its pane, and
/// counted as left untried only when an earlier boot ran it: one this boot
/// ran, as before an interrupted resume runs again, is not.
#[test]
fn a_paired_chat_is_left_untried_only_when_an_earlier_boot_ran_it() {
    if in_child("a_paired_chat_is_left_untried_only_when_an_earlier_boot_ran_it") {
        return;
    }
    opt_in(true);
    let host = stopped_paired_chat(crate::daemon::read_boot_id().unwrap());
    let pass = ran(after_boot(&host));
    assert!(
        pass.resumed.is_empty() && pass.failed.is_empty() && pass.left.is_empty(),
        "{pass:?}"
    );
    drop(host);
    let host = daemon_on(BootId::new("a-later-boot"));
    let pass = ran(after_boot(&host));
    assert!(
        pass.resumed.is_empty() && pass.failed.is_empty(),
        "{pass:?}"
    );
    assert_eq!(pass.left.len(), 1, "{pass:?}");
    assert!(pass.left[0].terminal.is_some(), "{pass:?}");
}

/// Once a boot's resume ran, here with the setting off, a daemon restarted
/// within that boot resumes nothing by itself, even what an earlier boot ran
/// and the setting now asks for. The next boot's first daemon does.
#[test]
fn a_daemon_restarted_within_one_boot_resumes_nothing_by_itself() {
    if in_child("a_daemon_restarted_within_one_boot_resumes_nothing_by_itself") {
        return;
    }
    opt_in(false);
    let (host, accepted, _) = stopped_agent(BootId::new("a-later-boot"));
    let before = session_of(&host, accepted.agent.id);
    assert!(matches!(after_boot(&host), AfterBoot::Off));
    assert_eq!(handled_boot().as_deref(), Some("a-later-boot"));
    assert_eq!(stopped_ids(&host), [before.id]);

    // Turned on within the boot, then the daemon restarts.
    opt_in(true);
    drop(host);
    let host = daemon_on(BootId::new("a-later-boot"));
    assert!(matches!(after_boot(&host), AfterBoot::AlreadyRan));
    assert_eq!(stopped_ids(&host), [before.id]);

    drop(host);
    let host = daemon_on(BootId::new("the-boot-after"));
    let pass = ran(after_boot(&host));
    assert_eq!(
        pass.resumed
            .iter()
            .map(|resumed| resumed.stopped)
            .collect::<Vec<_>>(),
        [before.id]
    );
    assert_eq!(handled_boot().as_deref(), Some("the-boot-after"));
}

/// A daemon that stops taking new work, as for an update or a shutdown,
/// interrupts the resume without announcing anything, and leaves it for the
/// next daemon of the boot.
#[test]
fn an_interrupted_resume_is_left_for_the_next_daemon_of_the_boot() {
    if in_child("an_interrupted_resume_is_left_for_the_next_daemon_of_the_boot") {
        return;
    }
    opt_in(true);
    let (host, accepted, _) = stopped_agent(BootId::new("a-later-boot"));
    let before = session_of(&host, accepted.agent.id);
    host.lock().unwrap().shutting_down = true;
    let pass = ran(after_boot(&host));
    assert!(pass.interrupted, "{pass:?}");
    assert!(
        pass.resumed.is_empty() && pass.failed.is_empty(),
        "{pass:?}"
    );
    assert_eq!(handled_boot(), None);
    host.lock().unwrap().shutting_down = false;
    assert_eq!(stopped_ids(&host), [before.id]);

    drop(host);
    let host = daemon_on(BootId::new("a-later-boot"));
    let pass = ran(after_boot(&host));
    assert_eq!(
        pass.resumed
            .iter()
            .map(|resumed| resumed.stopped)
            .collect::<Vec<_>>(),
        [before.id]
    );
    assert_eq!(handled_boot().as_deref(), Some("a-later-boot"));
}

/// A session whose provider cannot start is reported with why, stays
/// listed by `cagents resume`, and the resume still runs to the end.
#[test]
fn a_session_that_cannot_resume_is_reported_and_the_boot_still_counts_as_resumed() {
    if in_child("a_session_that_cannot_resume_is_reported_and_the_boot_still_counts_as_resumed") {
        return;
    }
    opt_in(true);
    let (host, accepted, _) = stopped_agent(BootId::new("a-later-boot"));
    let before = session_of(&host, accepted.agent.id);
    let root = std::path::PathBuf::from(std::env::var_os("CAGENTS_EXECUTION_FIXTURE").unwrap());
    std::fs::remove_file(root.join("bin/claude")).unwrap();
    let pass = ran(after_boot(&host));
    assert!(pass.resumed.is_empty() && !pass.interrupted, "{pass:?}");
    assert_eq!(
        pass.failed
            .iter()
            .map(|(stopped, _)| stopped.id)
            .collect::<Vec<_>>(),
        [before.id]
    );
    assert!(!pass.failed[0].1.is_empty());
    assert_eq!(handled_boot().as_deref(), Some("a-later-boot"));
    assert_eq!(stopped_ids(&host), [before.id]);
}

/// A window that opens while the daemon resumes restores its panes from what
/// they saved before the reboot. Whether it reopens one in place or prepares
/// a successor, the daemon starts no second writer of a conversation it
/// already runs.
#[test]
fn a_window_restoring_its_panes_during_the_resume_starts_no_second_writer() {
    if in_child("a_window_restoring_its_panes_during_the_resume_starts_no_second_writer") {
        return;
    }
    use crate::terminal_ipc::{ClientRequest, ServerResponse};
    const WINDOW: u64 = 7;
    opt_in(true);
    let (host, _, conversation) = stopped_agent(BootId::new("a-later-boot"));
    let saved = panes(&host).pop().unwrap();
    assert_eq!(ran(after_boot(&host)).resumed.len(), 1);

    let start = |session: chat::Session| {
        crate::host::chat::process(
            &host,
            WINDOW,
            ClientRequest::ChatStart {
                session,
                source: None,
                terminate_source: false,
                execution_environment: Default::default(),
            },
        )
    };
    assert!(start(saved.clone()).is_err(), "the pane's own restore");
    if let Ok(ServerResponse::ChatResumePrepared { session: prepared }) = crate::host::chat::process(
        &host,
        WINDOW,
        ClientRequest::ChatPrepareResume {
            source: saved.clone(),
        },
    ) && prepared.id != saved.id
    {
        assert!(start(prepared).is_err(), "a successor the window prepared");
    }
    let state = host.lock().unwrap();
    let writers: Vec<_> = state
        .chats
        .values()
        .filter(|record| record.handle.owns_work() || !record.handle.is_completed())
        .filter(|record| record.handle.session_id().as_deref() == Some(conversation.as_str()))
        .map(|record| record.session.id)
        .collect();
    assert_eq!(writers, [saved.id], "one process runs the conversation");
}

/// A pane a window reopened before the resume reached it is left to that
/// window: the resume starts nothing more and reports no failure.
#[test]
fn a_pane_a_window_reopened_first_is_left_to_it() {
    if in_child("a_pane_a_window_reopened_first_is_left_to_it") {
        return;
    }
    use crate::terminal_ipc::ClientRequest;
    const WINDOW: u64 = 7;
    opt_in(true);
    let (host, _, conversation) = stopped_agent(BootId::new("a-later-boot"));
    let mut saved = panes(&host).pop().unwrap();
    // As the window's restore starts it, on the live daemon's proof.
    saved.proof = Some(host.lock().unwrap().proof().clone());
    crate::host::chat::process(
        &host,
        WINDOW,
        ClientRequest::ChatStart {
            session: saved.clone(),
            source: None,
            terminate_source: false,
            execution_environment: Default::default(),
        },
    )
    .unwrap();
    let pass = ran(after_boot(&host));
    // Neither resumed by the pass nor reported as its failure.
    assert!(
        pass.resumed.is_empty() && pass.failed.is_empty() && pass.left.is_empty(),
        "{pass:?}"
    );
    let state = host.lock().unwrap();
    let writers = state
        .chats
        .values()
        .filter(|record| record.handle.session_id().as_deref() == Some(conversation.as_str()))
        .collect::<Vec<_>>();
    // The one writer is the window's.
    assert_eq!(writers.len(), 1);
    assert!(
        writers[0].subscribers.contains_key(&WINDOW),
        "{:?}",
        writers[0].subscribers.keys().collect::<Vec<_>>()
    );
}

fn send(id: crate::model::TerminalId, text: &str) -> ClientRequest {
    ClientRequest::ChatCommand {
        chat_id: id,
        command: Command::Prompt {
            text: text.into(),
            mode: PromptMode::Send,
            attachments: Vec::new(),
        },
    }
}

/// Waits until chat `id` is ready for another prompt.
fn idle(host: &SharedHost, id: crate::model::TerminalId) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let handle = host.lock().unwrap().chats[&id].handle.clone();
        if handle
            .snapshot(None)
            .is_some_and(|snapshot| snapshot.ready && !snapshot.busy)
        {
            return;
        }
        assert!(Instant::now() < deadline, "the chat never became idle");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// When a daemon is lost without stopping its providers, the next daemon in
/// the same boot restarts a pane's conversation in place. Its agent follows
/// the restarted process, so the window that restarted it can keep working
/// with the agent rather than being told the agent's session is gone.
#[test]
fn a_pane_restarted_after_its_daemon_was_lost_keeps_working_with_its_agent() {
    if in_child("a_pane_restarted_after_its_daemon_was_lost_keeps_working_with_its_agent") {
        return;
    }
    let (host, request, path) = fixture();
    let accepted = submit(&host, &Authority::default(), request).unwrap();
    assert_eq!(settled(&host, accepted.run.id).state, State::Succeeded);
    show_in_a_channel(&host, &path, &accepted);
    let pane = panes(&host).into_iter().next().unwrap();
    // The daemon ends without recording that its provider stopped.
    let handle = host.lock().unwrap().chats[&pane.id].handle.clone();
    handle.close().unwrap();
    std::mem::forget(host);

    let host = daemon_on(crate::daemon::read_boot_id().unwrap());
    let mut start = pane.clone();
    start.proof = Some(host.lock().unwrap().proof().clone());
    crate::host::chat::process(
        &host,
        17,
        ClientRequest::ChatStart {
            session: start,
            source: None,
            terminate_source: false,
            execution_environment: Default::default(),
        },
    )
    .unwrap();
    let running = host.lock().unwrap().chats[&pane.id].session.clone();
    assert!(crate::agent_execution::same_chat_process(
        &session_of(&host, accepted.agent.id),
        &running
    ));
    idle(&host, pane.id);
    crate::host::chat::process(&host, 17, send(pane.id, "Carry on")).unwrap();
}

/// A window the conversation stopped serving, because the daemon reopened it
/// for the agent's next prompt, is told plainly that it is not connected and
/// to reconnect, and nothing is sent on its behalf.
#[test]
fn a_window_left_behind_by_a_reopen_is_told_to_reconnect() {
    if in_child("a_window_left_behind_by_a_reopen_is_told_to_reconnect") {
        return;
    }
    let (host, request, path) = fixture();
    let accepted = submit(&host, &Authority::default(), request).unwrap();
    assert_eq!(settled(&host, accepted.run.id).state, State::Succeeded);
    show_in_a_channel(&host, &path, &accepted);
    let session = session_of(&host, accepted.agent.id);
    crate::host::chat::process(
        &host,
        17,
        ClientRequest::ChatAttach {
            session: session.clone(),
        },
    )
    .unwrap();
    idle(&host, session.id);
    let handle = host.lock().unwrap().chats[&session.id].handle.clone();
    assert!(
        handle
            .try_stop_if_idle(
                handle.resident_generation(),
                crate::daemon::owned_children::DetachedWork::Spare,
            )
            .unwrap()
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    while !host.lock().unwrap().chats[&session.id]
        .handle
        .is_completed()
    {
        assert!(Instant::now() < deadline, "the chat never stopped");
        std::thread::sleep(Duration::from_millis(20));
    }
    // The agent's next prompt reopens its conversation in place.
    let next = submit(
        &host,
        &Authority::default(),
        Submit {
            name: None,
            destination: Destination::Existing(accepted.agent.id),
            prompt: "Again".into(),
            idempotency_key: "again".into(),
        },
    )
    .unwrap();
    assert_eq!(settled(&host, next.run.id).state, State::Succeeded);
    idle(&host, session.id);

    let refused =
        crate::host::chat::process(&host, 17, send(session.id, "From the window")).unwrap_err();
    assert_eq!(
        refused.message,
        crate::chat::failure::delivery::DETACHED_REQUEST
    );
    let outcome = crate::chat::failure::DeliveryOutcome::parse(&refused.message).unwrap();
    assert!(!outcome.unknown && outcome.notice);
}
