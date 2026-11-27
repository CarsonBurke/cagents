//! Resuming a stopped conversation without a window: `cagents resume`, and
//! an agent's next prompt after a reboot.
use super::*;
use crate::{
    agent_execution::directory::Scope as Sessions,
    agent_execution::resume::{Resumed, Scope, StoppedSessions},
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
    let mut state = HostState::new(HostSessionProof {
        host_session_id: HostSessionId::new("restarted-resume-fixture"),
        boot_id: boot,
        host_incarnation: HostIncarnation::new("restarted-resume"),
    });
    state.install_model(Registry::at(&path).unwrap());
    (
        Arc::new(crate::host::state::HostLock::new(state)),
        accepted,
        conversation,
    )
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
