//! Closing an agent, or a plain retained conversation, that a crash or power
//! loss left with no recorded daemon stop. A changed kernel boot proves the old
//! provider is gone; nothing that started since may be closed in its place.
use super::*;
use crate::agent_execution::{
    Accepted, Destination, HostedSession, Lineage, Operation, Origin, Reply, Spec, State, Submit,
    Surface, Target,
};
use crate::model::{BootId, HostIncarnation, HostSessionId};

fn host() -> SharedHost {
    Arc::new(HostLock::new(HostState::new(HostSessionProof {
        host_session_id: HostSessionId::new("lost-agent-close"),
        boot_id: BootId::new("boot"),
        host_incarnation: HostIncarnation::new("incarnation"),
    })))
}

struct Fixture {
    directory: tempfile::TempDir,
    source: Session,
    /// Written only if anything starts the provider.
    marker: std::path::PathBuf,
    history: std::path::PathBuf,
}

impl Fixture {
    fn history(&self) -> Vec<u8> {
        std::fs::read(&self.history).unwrap()
    }

    fn authority_rows(&self, id: TerminalId) -> Vec<(String, String)> {
        let db =
            rusqlite::Connection::open(self.directory.path().join("lost-agent.sqlite")).unwrap();
        let mut rows = db
            .prepare("SELECT token,source FROM agent_authorities WHERE session_id=?1")
            .unwrap();
        rows.query_map([id.to_string()], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    }
}

/// A retained Claude conversation saved under `saved_boot`, with its saved
/// transcript on disk and a provider that only records that it was started.
fn retained_source(host: &SharedHost, saved_boot: &str) -> Fixture {
    let directory = tempfile::tempdir().unwrap();
    let registry =
        super::super::model::Registry::at(&directory.path().join("lost-agent.sqlite")).unwrap();
    host.lock().unwrap().execution_registry = Some(Arc::new(std::sync::Mutex::new(registry)));
    let marker = directory.path().join("provider-started");
    let program = directory.path().join("provider");
    std::fs::write(
        &program,
        format!(
            "#!/bin/sh\nprintf started > '{}'\nexit 91\n",
            marker.display()
        ),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
    let project = directory.path().join("project");
    std::fs::create_dir_all(&project).unwrap();
    let home = directory.path().join("claude");
    let mut proof = host.lock().unwrap().proof().clone();
    proof.boot_id = BootId::new(saved_boot);
    let source = Session {
        id: TerminalId::new(),
        launch: chat::Launch {
            provider: Provider::Claude,
            session_id: Some(uuid::Uuid::new_v4().to_string()),
            recipe: crate::model::LaunchRecipe {
                program: program.to_string_lossy().into_owned(),
                arguments: Vec::new(),
                working_directory: project.clone(),
                environment: [(
                    "CLAUDE_CONFIG_DIR".to_owned(),
                    home.to_string_lossy().into_owned(),
                )]
                .into(),
                unset_environment: Default::default(),
            },
        },
        proof: Some(proof),
        terminal: None,
        owned_codex_listener: None,
        native_continuity: chat::NativeContinuity::RetainedClaudeActor { protocol: 2 },
    };
    assert!(source.has_valid_native_continuity());
    let encoded: String = project
        .to_str()
        .unwrap()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let history = home.join("projects").join(encoded).join(format!(
        "{}.jsonl",
        source.launch.session_id.as_ref().unwrap()
    ));
    std::fs::create_dir_all(history.parent().unwrap()).unwrap();
    std::fs::write(
        &history,
        serde_json::json!({"type":"user","uuid":"u1","parentUuid":null,
            "sessionId":source.launch.session_id,
            "message":{"role":"user","content":"lost agent question"}})
        .to_string()
            + "\n",
    )
    .unwrap();
    Fixture {
        directory,
        source,
        marker,
        history,
    }
}

/// The pane's paired CLI view, which may also run as its own terminal.
fn paired_view(recipe: crate::model::LaunchRecipe) -> crate::model::TerminalSession {
    crate::model::TerminalSession {
        id: TerminalId::new(),
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
    }
}

fn registry(host: &SharedHost) -> super::super::agent_execution::SharedRegistry {
    host.lock().unwrap().execution_registry.clone().unwrap()
}

fn submit(directory: &std::path::Path, key: &str) -> Submit {
    Submit {
        name: None,
        destination: Destination::New(Spec {
            provider: Provider::Claude,
            account: None,
            target: Target {
                repository: directory.to_owned(),
                directory: directory.to_owned(),
                placement: None,
            },
            surface: Surface::Chat,
            model: None,
            permission_mode: None,
            reasoning_effort: None,
        }),
        prompt: "ORIGINAL_PROMPT".into(),
        idempotency_key: key.into(),
    }
}

fn open_project(host: &SharedHost, directory: &std::path::Path) {
    registry(host)
        .lock()
        .unwrap()
        .apply(&crate::workspace_model::ModelEdit::OpenProject {
            repository: directory.to_owned(),
            name: "Fixture".into(),
            kind: crate::model::ProjectKind::Folder,
        })
        .unwrap();
}

/// An open agent bound to `source`, with the run daemon startup could not
/// prove finished, and no daemon stop recorded: what a crash leaves.
fn lost_agent(host: &SharedHost, directory: &std::path::Path, source: &Session) -> Accepted {
    open_project(host, directory);
    let registry = registry(host);
    let mut registry = registry.lock().unwrap();
    let accepted = registry
        .submit_agent(&Origin::User, &submit(directory, "lost-agent"), 1)
        .unwrap();
    registry
        .bind_agent(accepted.agent.id, None, HostedSession::Chat(source.clone()))
        .unwrap();
    registry.interrupt_unproven_agent_runs(2).unwrap();
    assert!(!registry.agent_process_ended(accepted.agent.id).unwrap());
    accepted
}

fn register_authority(host: &SharedHost, source: &Session) {
    registry(host)
        .lock()
        .unwrap()
        .register_agent_authority(&HostedSession::Chat(source.clone()))
        .unwrap();
}

fn agent_open(host: &SharedHost, id: crate::model::AgentId) -> bool {
    !registry(host).lock().unwrap().agent(id).unwrap().closed
}

fn close_agent(host: &SharedHost, id: crate::model::AgentId) -> Result<Reply, String> {
    super::super::agent_execution::process(host, Default::default(), Operation::Close(id))
}

/// The same allocation, as a Start in this boot would issue it.
fn current(host: &SharedHost, source: &Session) -> Session {
    let mut current = source.clone();
    current.proof = Some(host.lock().unwrap().proof().clone());
    current
}

fn assert_idle(host: &SharedHost) {
    let state = host.lock().unwrap();
    assert!(state.chats.is_empty());
    assert!(state.chat_spawning.is_empty(), "a close fence leaked");
    assert!(state.native_actor_startups.is_empty());
}

#[test]
fn closing_a_lost_agent_ends_it_without_starting_anything() {
    let host = host();
    let fixture = retained_source(&host, "earlier-boot");
    let source = &fixture.source;
    register_authority(&host, source);
    let accepted = lost_agent(&host, fixture.directory.path(), source);
    let history = fixture.history();

    let reply = close_agent(&host, accepted.agent.id).unwrap();
    assert!(matches!(reply, Reply::Closed(agent) if agent.closed));
    assert!(!fixture.marker.exists(), "closing started no provider");
    let run = registry(&host)
        .lock()
        .unwrap()
        .agent_run(accepted.run.id)
        .unwrap();
    assert_eq!(run.state, State::Interrupted, "its outcome stays unknown");
    assert!(fixture.authority_rows(source.id).is_empty());
    assert_eq!(fixture.history(), history);
    assert_idle(&host);
}

#[test]
fn closing_a_lost_agent_retires_its_capability_under_newer_thread_metadata() {
    for pane in [false, true] {
        let host = host();
        let mut fixture = retained_source(&host, "earlier-boot");
        // Bound before its provider confirmed a thread; the capability was
        // last recorded for the same process with the thread it confirmed.
        fixture.source.launch.session_id = None;
        let source = fixture.source.clone();
        let accepted = lost_agent(&host, fixture.directory.path(), &source);
        let mut confirmed = source.clone();
        confirmed.launch.session_id = Some(uuid::Uuid::new_v4().to_string());
        register_authority(&host, &confirmed);
        assert_eq!(fixture.authority_rows(source.id).len(), 1);
        if pane {
            super::close_retained_from_pane(&host, &source).unwrap();
        } else {
            close_agent(&host, accepted.agent.id).unwrap();
        }
        assert!(!agent_open(&host, accepted.agent.id), "pane: {pane}");
        assert!(
            fixture.authority_rows(source.id).is_empty(),
            "pane: {pane}: the same process kept its capability"
        );
        assert!(!fixture.marker.exists());
        assert_idle(&host);
    }
}

#[test]
fn a_missing_agent_provider_in_this_boot_stays_unconfirmed() {
    let host = host();
    let fixture = retained_source(&host, "boot");
    let accepted = lost_agent(&host, fixture.directory.path(), &fixture.source);
    let failure = close_agent(&host, accepted.agent.id).unwrap_err();
    assert!(failure.contains("Cannot confirm teardown"), "{failure}");
    assert!(super::close_retained_from_pane(&host, &fixture.source).is_err());
    assert!(agent_open(&host, accepted.agent.id));
    assert!(!fixture.marker.exists());
    assert_idle(&host);
}

#[test]
fn closing_a_lost_agents_pane_ends_the_agent() {
    // Named and unnamed: the daemon request a pane's Close sends.
    for named in [true, false] {
        let host = host();
        let mut fixture = retained_source(&host, "earlier-boot");
        if !named {
            fixture.source.launch.session_id = None;
        }
        let source = fixture.source.clone();
        register_authority(&host, &source);
        let accepted = lost_agent(&host, fixture.directory.path(), &source);
        super::close_retained_from_pane(&host, &source).unwrap();
        assert!(!agent_open(&host, accepted.agent.id), "named: {named}");
        let run = registry(&host)
            .lock()
            .unwrap()
            .agent_run(accepted.run.id)
            .unwrap();
        assert_eq!(run.state, State::Interrupted);
        assert!(fixture.authority_rows(source.id).is_empty());
        assert!(!fixture.marker.exists());
        assert_idle(&host);
    }
}

#[test]
fn closing_a_plain_lost_conversations_pane_changes_nothing() {
    let host = host();
    let fixture = retained_source(&host, "earlier-boot");
    register_authority(&host, &fixture.source);
    let authority = fixture.authority_rows(fixture.source.id);
    let history = fixture.history();
    super::close_retained_from_pane(&host, &fixture.source).unwrap();
    assert_eq!(fixture.authority_rows(fixture.source.id), authority);
    assert_eq!(fixture.history(), history);
    assert!(!fixture.marker.exists());
    assert_idle(&host);
}

#[test]
fn a_lost_agent_with_a_start_under_way_does_not_close() {
    for case in ["fresh-capability", "unproved-capability", "starting"] {
        let host = host();
        let fixture = retained_source(&host, "earlier-boot");
        let source = &fixture.source;
        let accepted = lost_agent(&host, fixture.directory.path(), source);
        let live = current(&host, source);
        let mut guard = None;
        match case {
            "fresh-capability" => {
                registry(&host)
                    .lock()
                    .unwrap()
                    .fresh_agent_authority(&HostedSession::Chat(live.clone()))
                    .unwrap();
            }
            "unproved-capability" => {
                // No proof says which boot issued it, so it may be this one.
                let mut unproved = live.clone();
                unproved.proof = None;
                registry(&host)
                    .lock()
                    .unwrap()
                    .fresh_agent_authority(&HostedSession::Chat(unproved))
                    .unwrap();
            }
            _ => guard = capacity::Reservation::new(&host, &live).unwrap(),
        }
        let before = fixture.authority_rows(source.id);
        let refused = close_agent(&host, accepted.agent.id).unwrap_err();
        assert!(refused.contains("may be running"), "{case}: {refused}");
        let refused = super::close_retained_from_pane(&host, source).unwrap_err();
        assert!(refused.contains("may be running"), "{case}: {refused}");
        assert!(agent_open(&host, accepted.agent.id), "{case}");
        assert_eq!(fixture.authority_rows(source.id), before);
        assert!(!fixture.marker.exists());
        let spawning = host.lock().unwrap().chat_spawning.clone();
        assert_eq!(spawning.len(), usize::from(guard.is_some()), "{case}");
        drop(guard);
        assert_idle(&host);
    }
}

#[test]
fn an_unconfirmed_earlier_boot_launch_refuses_close_before_changing_anything() {
    let host = host();
    let fixture = retained_source(&host, "earlier-boot");
    let source = &fixture.source;
    open_project(&host, fixture.directory.path());
    let registry = registry(&host);
    let accepted = registry
        .lock()
        .unwrap()
        .submit_agent(
            &Origin::User,
            &submit(fixture.directory.path(), "pending"),
            1,
        )
        .unwrap();
    registry
        .lock()
        .unwrap()
        .reserve_agent_launch(accepted.agent.id, &HostedSession::Chat(source.clone()))
        .unwrap();
    register_authority(&host, source);
    let before = fixture.authority_rows(source.id);
    assert_eq!(before.len(), 1);
    for _ in 0..2 {
        let failure = close_agent(&host, accepted.agent.id).unwrap_err();
        assert!(failure.contains("Cannot confirm teardown"), "{failure}");
        assert!(super::close_retained_from_pane(&host, source).is_err());
        assert_eq!(fixture.authority_rows(source.id), before);
        assert!(agent_open(&host, accepted.agent.id));
        assert_idle(&host);
    }
}

#[test]
fn closing_a_lost_parent_leaves_its_child_open() {
    let host = host();
    let fixture = retained_source(&host, "earlier-boot");
    let source = &fixture.source;
    let accepted = lost_agent(&host, fixture.directory.path(), source);
    let child = registry(&host)
        .lock()
        .unwrap()
        .submit_agent(
            &Origin::Child {
                parent: Lineage(source.id),
            },
            &submit(fixture.directory.path(), "child"),
            3,
        )
        .unwrap();
    close_agent(&host, accepted.agent.id).unwrap();
    assert!(!agent_open(&host, accepted.agent.id));
    assert!(
        agent_open(&host, child.agent.id),
        "a parent's close leaves its child"
    );
}

#[test]
fn closing_a_lost_childs_pane_ends_only_that_child() {
    for current_proof in [false, true] {
        let host = host();
        let fixture = retained_source(&host, "earlier-boot");
        let source = fixture.source.clone();
        // One fixture owns the Registry. The parent needs only its separate
        // durable descriptor; no parent transcript is opened.
        let mut parent_source = source.clone();
        parent_source.id = TerminalId::new();
        parent_source.launch.session_id = Some(uuid::Uuid::new_v4().to_string());
        let parent = lost_agent(&host, fixture.directory.path(), &parent_source);
        let registry = registry(&host);
        let (child, child_origin, parent_authority, child_authority) = {
            let mut registry = registry.lock().unwrap();
            let parent_authority = registry
                .register_agent_authority(&HostedSession::Chat(parent_source.clone()))
                .unwrap();
            let child_origin = registry.execution_origin(&parent_authority).unwrap();
            assert_eq!(
                child_origin,
                Origin::Child {
                    parent: Lineage(parent_source.id)
                }
            );
            let child = registry
                .submit_agent(
                    &child_origin,
                    &submit(fixture.directory.path(), "lost-child"),
                    3,
                )
                .unwrap();
            registry
                .bind_agent(child.agent.id, None, HostedSession::Chat(source.clone()))
                .unwrap();
            let child_authority = registry
                .register_agent_authority(&HostedSession::Chat(source.clone()))
                .unwrap();
            registry.interrupt_unproven_agent_runs(4).unwrap();
            (child, child_origin, parent_authority, child_authority)
        };
        let (parent_before, parent_run, child_before, child_run) = {
            let registry = registry.lock().unwrap();
            (
                registry.agent(parent.agent.id).unwrap(),
                registry.agent_run(parent.run.id).unwrap(),
                registry.agent(child.agent.id).unwrap(),
                registry.agent_run(child.run.id).unwrap(),
            )
        };
        assert_eq!(parent_run.state, State::Interrupted);
        assert_eq!(child_run.state, State::Interrupted);
        let parent_rows = fixture.authority_rows(parent_source.id);
        assert_eq!(parent_rows.len(), 1);
        let history = fixture.history();
        // A pane holding a descriptor this boot issued is not the lost one.
        let closing = if current_proof {
            current(&host, &source)
        } else {
            source.clone()
        };
        let result = super::close_retained_from_pane(&host, &closing);
        {
            let registry = registry.lock().unwrap();
            let closed = registry.agent(child.agent.id).unwrap();
            assert_eq!(closed.closed, !current_proof, "current: {current_proof}");
            assert_eq!(closed.origin, child_origin);
            assert_eq!(closed.session, child_before.session);
            assert_eq!(registry.agent_run(child.run.id).unwrap(), child_run);
            assert_eq!(registry.agent(parent.agent.id).unwrap(), parent_before);
            assert_eq!(registry.agent_run(parent.run.id).unwrap(), parent_run);
            assert_eq!(
                registry.execution_origin(&parent_authority).unwrap(),
                child_origin
            );
            assert_eq!(
                registry.execution_origin(&child_authority).is_ok(),
                current_proof
            );
        }
        if current_proof {
            assert!(result.is_err(), "a current descriptor closed a lost agent");
        } else {
            result.unwrap();
            assert!(fixture.authority_rows(source.id).is_empty());
        }
        assert_eq!(fixture.authority_rows(parent_source.id), parent_rows);
        assert_eq!(fixture.history(), history);
        assert!(
            !fixture.marker.exists(),
            "pane Close must not start a provider"
        );
        assert_idle(&host);
    }
}

#[test]
fn a_native_discovery_owner_refuses_lost_agent_close() {
    const MODE: &str = "CAGENTS_TEST_LOST_CLOSE_DISCOVERY_ROOT";
    let Some(root) = std::env::var_os(MODE) else {
        // Only the owned runtime directory changes. In particular this
        // subprocess inherits the caller's authority without inspecting it.
        let directory = tempfile::tempdir().unwrap();
        let runtime = directory.path().join("run");
        crate::secure_file::prepare_private_directory(&runtime).unwrap();
        let stdout = directory.path().join("child.stdout");
        let stderr = directory.path().join("child.stderr");
        struct OriginalChild(std::process::Child);
        impl Drop for OriginalChild {
            fn drop(&mut self) {
                if let Ok(None) = self.0.try_wait() {
                    let _ = self.0.kill();
                }
                let _ = self.0.wait();
            }
        }
        let mut child = OriginalChild(
            std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "host::chat::lost_agent_close_tests::a_native_discovery_owner_refuses_lost_agent_close",
                    "--color=never",
                ])
                .env(MODE, directory.path())
                .env("XDG_RUNTIME_DIR", &runtime)
                .stdout(std::fs::File::create(&stdout).unwrap())
                .stderr(std::fs::File::create(&stderr).unwrap())
                .spawn()
                .unwrap(),
        );
        let deadline = Instant::now() + Duration::from_secs(30);
        let status = loop {
            if let Some(status) = child.0.try_wait().unwrap() {
                break status;
            }
            assert!(
                Instant::now() < deadline,
                "discovery fixture did not settle"
            );
            thread::sleep(Duration::from_millis(5));
        };
        let stdout = std::fs::read_to_string(stdout).unwrap();
        assert!(
            status.success() && stdout.contains("1 passed"),
            "private discovery fixture failed:\n{stdout}\n{}",
            std::fs::read_to_string(stderr).unwrap()
        );
        return;
    };
    let root = std::path::PathBuf::from(root);
    assert_eq!(
        std::env::var_os("XDG_RUNTIME_DIR"),
        Some(root.join("run").into_os_string())
    );

    let host = host();
    let mut fixture = retained_source(&host, "earlier-boot");
    let mut view_recipe = fixture.source.launch.recipe.clone();
    view_recipe.program = "/bin/sleep".into();
    view_recipe.arguments = vec!["60".into()];
    view_recipe.environment.clear();
    let view = paired_view(view_recipe);
    let paired = view.id;
    fixture.source.terminal = Some(view.clone());
    let source = &fixture.source;
    register_authority(&host, source);
    let accepted = lost_agent(&host, fixture.directory.path(), source);
    let registry = registry(&host);
    let records = || {
        let registry = registry.lock().unwrap();
        serde_json::to_value((
            registry.agent(accepted.agent.id).unwrap(),
            registry.latest_agent_run(accepted.agent.id).unwrap(),
        ))
        .unwrap()
    };
    let before_records = records();
    let before_authority = fixture.authority_rows(source.id);
    assert!(!before_authority.is_empty());
    let before_history = fixture.history();

    // Untrusted discovery naming this test process, not a helper, adoption
    // proof or execution source. Its presence alone must fence Close, before
    // any authenticated provider connection is possible.
    use crate::daemon::holder_registry::{self as holders, Kind};
    let native = holders::directory(Kind::Native).unwrap();
    crate::secure_file::prepare_private_directory(&native).unwrap();
    let process = holders::Process::read(std::process::id() as i32).unwrap();
    let owner = holders::Record {
        version: 4,
        build: Some(holders::BuildIdentity::current(Kind::Native)),
        kind: Kind::Native,
        application: crate::instance::APPLICATION_ID.into(),
        boot: crate::daemon::read_boot_id().unwrap(),
        terminal: source.id,
        holder: process.clone(),
        root: process,
    };
    owner.create(&native).unwrap();
    let path = holders::socket(&native, source.id).with_extension("json");
    let before_discovery = std::fs::read(&path).unwrap();
    assert!(super::has_native_owner(source).unwrap());

    let refusal = close_agent(&host, accepted.agent.id).unwrap_err();
    assert!(refusal.contains("may be running"), "{refusal}");
    let refusal = super::close_retained_from_pane(&host, source).unwrap_err();
    assert!(refusal.contains("may be running"), "{refusal}");
    assert_eq!(records(), before_records);
    assert_eq!(fixture.authority_rows(source.id), before_authority);
    assert_eq!(fixture.history(), before_history);
    assert_eq!(std::fs::read(&path).unwrap(), before_discovery);
    assert_idle(&host);
    assert!(!fixture.marker.exists());

    owner.remove_record(&native).unwrap();
    assert!(!super::has_native_owner(source).unwrap());
    let refuses = |blocker: &str| {
        let refusal = close_agent(&host, accepted.agent.id).unwrap_err();
        assert!(refusal.contains("may be running"), "{blocker}: {refusal}");
        let refusal = super::close_retained_from_pane(&host, source).unwrap_err();
        assert!(refusal.contains("may be running"), "{blocker}: {refusal}");
        assert_eq!(records(), before_records, "{blocker}");
        assert_eq!(fixture.authority_rows(source.id), before_authority);
        assert_idle(&host);
        assert!(!fixture.marker.exists());
    };

    // A holder discovery record for the paired CLI view, alone.
    let terminals = holders::directory(Kind::Terminal).unwrap();
    crate::secure_file::prepare_private_directory(&terminals).unwrap();
    let process = holders::Process::read(std::process::id() as i32).unwrap();
    let view_owner = holders::Record {
        version: 4,
        build: Some(holders::BuildIdentity::current(Kind::Terminal)),
        kind: Kind::Terminal,
        application: crate::instance::APPLICATION_ID.into(),
        boot: crate::daemon::read_boot_id().unwrap(),
        terminal: paired,
        holder: process.clone(),
        root: process,
    };
    view_owner.create(&terminals).unwrap();
    refuses("paired view discovery");
    view_owner.remove_record(&terminals).unwrap();

    // The paired CLI view running in this daemon, alone.
    let mut running = view.clone();
    running.continuity = crate::model::TerminalContinuity::RelaunchAvailable {
        recipe: running.launch.clone(),
        last_exit: None,
    };
    host.lock().unwrap().register_client(0);
    crate::host::state::spawn_terminal(&host, 0, running, Default::default()).unwrap();
    let handle = host.lock().unwrap().terminals[&paired].handle.clone();
    assert!(
        holders::recovery_candidate(&holders::socket(&terminals, paired), Kind::Terminal)
            .unwrap()
            .is_none(),
        "the live view must refuse on its own"
    );
    refuses("live paired view");
    handle.shutdown().unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !handle.is_completed() {
        assert!(Instant::now() < deadline, "the paired view did not exit");
        thread::sleep(Duration::from_millis(10));
    }
    // Shutdown joins the worker, but its Exited event is queued on the shared
    // observer. Settle that real observation before testing an unblocked close.
    crate::terminal::flush_archived_notices();
    assert!(
        !host.lock().unwrap().is_live(&paired),
        "the host observed the paired view's exit"
    );

    // With each blocker gone, the same original agent closes without
    // starting a provider. A completed view holds nothing.
    assert!(host.lock().unwrap().terminals.contains_key(&paired));
    super::close_retained_from_pane(&host, source).unwrap();
    assert!(!agent_open(&host, accepted.agent.id));
    assert!(fixture.authority_rows(source.id).is_empty());
    assert_eq!(fixture.history(), before_history);
    assert_eq!(
        registry
            .lock()
            .unwrap()
            .latest_agent_run(accepted.agent.id)
            .unwrap()
            .state,
        State::Interrupted
    );
    assert_idle(&host);
    assert!(!fixture.marker.exists());
}

#[test]
fn a_plain_lost_conversation_refuses_close_while_its_start_is_reserved() {
    let host = host();
    let fixture = retained_source(&host, "earlier-boot");
    let source = &fixture.source;
    register_authority(&host, source);
    let authority = fixture.authority_rows(source.id);
    assert_eq!(authority.len(), 1);
    let history = fixture.history();
    let starting = current(&host, source);

    // The token Start holds before publishing a provider, with no managed
    // agent whose separate close fence could hide this.
    let reservation = capacity::Reservation::new(&host, &starting)
        .unwrap()
        .unwrap();
    let (spawning, generation) = {
        let state = host.lock().unwrap();
        assert_eq!(state.chat_spawning.get(&source.id), Some(&starting));
        (state.chat_spawning.clone(), state.inventory_generation)
    };
    for closing in [source, &starting] {
        super::close_retained_from_pane(&host, closing)
            .expect_err("Close must refuse while the original start owns its reservation");
        let state = host.lock().unwrap();
        assert_eq!(state.chat_spawning, spawning);
        assert_eq!(state.inventory_generation, generation);
        assert!(state.chats.is_empty());
        drop(state);
        assert_eq!(fixture.authority_rows(source.id), authority);
        assert_eq!(fixture.history(), history);
        assert!(!fixture.marker.exists(), "Close must not start a provider");
    }

    drop(reservation);
    super::close_retained_from_pane(&host, source).unwrap();
    assert_idle(&host);
    assert_eq!(fixture.authority_rows(source.id), authority);
    assert_eq!(fixture.history(), history);
    assert!(!fixture.marker.exists());
}

#[test]
fn a_lost_agents_card_restarts_in_place_after_its_thread_advanced() {
    let host = host();
    let mut fixture = retained_source(&host, "earlier-boot");
    // An agent is bound before its provider confirms a thread, so its saved
    // binding names none; the card has since learned one and, after the
    // restart, starts it in place with this daemon's proof and its resume
    // recipe, as the chat client does for a conversation it cannot reattach.
    fixture.source.launch.session_id = None;
    let source = fixture.source.clone();
    let accepted = lost_agent(&host, fixture.directory.path(), &source);
    let mut card = current(&host, &source);
    card.native_continuity = chat::NativeContinuity::ReconnectOnly;
    let advanced = uuid::Uuid::new_v4().to_string();
    // The advanced thread's transcript sits beside the original one.
    std::fs::write(
        fixture.history.with_file_name(format!("{advanced}.jsonl")),
        serde_json::json!({"type":"user","uuid":"u2","parentUuid":null,
            "sessionId":advanced,
            "message":{"role":"user","content":"lost agent question"}})
        .to_string()
            + "\n",
    )
    .unwrap();
    card.launch.recipe.arguments = vec!["--resume".into(), advanced.clone()];
    card.launch.session_id = Some(advanced);
    let started = process_inner(
        &host,
        0,
        ClientRequest::ChatStart {
            session: card,
            source: None,
            terminate_source: false,
            execution_environment: Default::default(),
        },
    );
    if let Err(refused) = &started {
        panic!(
            "the agent's own conversation was refused: {}",
            refused.message
        );
    }
    let deadline = Instant::now() + Duration::from_secs(10);
    while !fixture.marker.exists() {
        assert!(
            Instant::now() < deadline,
            "the agent's own conversation did not start"
        );
        thread::sleep(Duration::from_millis(10));
    }
    assert!(agent_open(&host, accepted.agent.id));
}

/// Registers an installed ownership row for `source` as its own boot left it,
/// with the account home `home` captured.
fn register_owner(host: &SharedHost, source: &Session, home: &std::path::Path) {
    use std::os::unix::fs::MetadataExt;
    let canonical = home.canonicalize().unwrap();
    let metadata = std::fs::metadata(&canonical).unwrap();
    let scope = crate::account::scope::LaunchAccountScope {
        provider: Provider::Claude,
        canonical_home: canonical,
        device: metadata.dev(),
        inode: metadata.ino(),
        registered_id: None,
        purge_ids: vec![],
    };
    let process = crate::daemon::holder_registry::Process::read(std::process::id() as i32).unwrap();
    let owner = crate::daemon::holder_registry::Record {
        version: 4,
        build: None,
        kind: crate::daemon::holder_registry::Kind::Native,
        application: crate::instance::APPLICATION_ID.into(),
        boot: source.proof.as_ref().unwrap().boot_id.clone(),
        terminal: source.id,
        holder: process.clone(),
        root: process,
    };
    registry(host)
        .lock()
        .unwrap()
        .register_native_actor_ownership(uuid::Uuid::new_v4(), &owner, source, Ok(&scope))
        .unwrap();
}

#[test]
fn an_earlier_boot_resume_keeps_the_account_home_it_ran_under() {
    let host = host();
    let mut fixture = retained_source(&host, "earlier-boot");
    // The launch names its home through a link, as a linked account does.
    let first = fixture.directory.path().join("claude");
    let second = fixture.directory.path().join("claude-other");
    std::fs::create_dir_all(&second).unwrap();
    let link = fixture.directory.path().join("account");
    std::os::unix::fs::symlink(&first, &link).unwrap();
    fixture.source.launch.recipe.environment.insert(
        "CLAUDE_CONFIG_DIR".into(),
        link.to_string_lossy().into_owned(),
    );
    let source = fixture.source.clone();
    let accepted = lost_agent(&host, fixture.directory.path(), &source);
    register_owner(&host, &source, &first);
    super::retained_resume::ensure_prior_boot_account(&host, &source).unwrap();

    // The same launch now selects another physical home.
    std::fs::remove_file(&link).unwrap();
    std::os::unix::fs::symlink(&second, &link).unwrap();
    let refused = super::retained_resume::ensure_prior_boot_account(&host, &source).unwrap_err();
    assert_eq!(refused, super::retained_resume::PRIOR_BOOT_ACCOUNT_CHANGED);
    let refused = process_inner(
        &host,
        0,
        ClientRequest::ChatPrepareResume {
            source: source.clone(),
        },
    )
    .err()
    .expect("a remapped home prepared a resume");
    assert_eq!(
        refused.message,
        super::retained_resume::PRIOR_BOOT_ACCOUNT_CHANGED
    );
    // Refusing records nothing: the agent is still bound and not stopped.
    {
        let registry = registry(&host);
        let registry = registry.lock().unwrap();
        assert!(!registry.agent_process_ended(accepted.agent.id).unwrap());
        assert_eq!(
            registry.agent(accepted.agent.id).unwrap().session,
            Some(HostedSession::Chat(source.clone()))
        );
    }
    // A Start in place of that owner refuses the same way, before it
    // reserves or starts anything.
    let mut in_place = current(&host, &source);
    in_place.native_continuity = chat::NativeContinuity::ReconnectOnly;
    let refused = process_inner(
        &host,
        0,
        ClientRequest::ChatStart {
            session: in_place,
            source: None,
            terminate_source: false,
            execution_environment: Default::default(),
        },
    )
    .err()
    .expect("a remapped home started in place");
    assert_eq!(
        refused.message,
        super::retained_resume::PRIOR_BOOT_ACCOUNT_CHANGED
    );
    assert!(!fixture.marker.exists());
    assert_idle(&host);

    // With the original home selected again the resume is prepared, and only
    // then is the old process recorded as ended.
    std::fs::remove_file(&link).unwrap();
    std::os::unix::fs::symlink(&first, &link).unwrap();
    let ServerResponse::ChatResumePrepared { session: next } = process_inner(
        &host,
        0,
        ClientRequest::ChatPrepareResume {
            source: source.clone(),
        },
    )
    .unwrap() else {
        panic!("expected a prepared resume");
    };
    assert_ne!(next.id, source.id);
    assert_eq!(next.launch, source.launch);
    assert!(
        registry(&host)
            .lock()
            .unwrap()
            .agent_process_ended(accepted.agent.id)
            .unwrap()
    );
    assert!(!fixture.marker.exists());
    assert_idle(&host);
}

#[test]
fn an_earlier_boot_home_on_a_renumbered_filesystem_is_the_same_home() {
    use std::os::unix::fs::MetadataExt;
    let host = host();
    let fixture = retained_source(&host, "earlier-boot");
    let source = fixture.source.clone();
    let home = fixture.directory.path().join("claude");
    let device = std::fs::metadata(&home).unwrap().dev();
    register_owner(&host, &source, &home);
    // The earlier boot mounted the same filesystem under another device
    // number, as btrfs subvolumes and device-mapper volumes may be.
    let db =
        rusqlite::Connection::open(fixture.directory.path().join("lost-agent.sqlite")).unwrap();
    let (actor, stored): (String, String) = db
        .query_row(
            "SELECT actor, ownership FROM native_actor_ownerships",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    let mut stored: serde_json::Value = serde_json::from_str(&stored).unwrap();
    let recorded = &mut stored["scope"]["Ok"]["device"];
    assert_eq!(recorded.as_u64(), Some(device), "{stored}");
    *recorded = (device ^ 1).into();
    db.execute(
        "UPDATE native_actor_ownerships SET ownership=?1 WHERE actor=?2",
        [stored.to_string(), actor],
    )
    .unwrap();
    super::retained_resume::ensure_prior_boot_account(&host, &source).unwrap();
    let mut in_place = current(&host, &source);
    in_place.native_continuity = chat::NativeContinuity::ReconnectOnly;
    if let Err(refused) = process_inner(
        &host,
        0,
        ClientRequest::ChatStart {
            session: in_place,
            source: None,
            terminate_source: false,
            execution_environment: Default::default(),
        },
    ) {
        panic!("a renumbered home refused its Start: {}", refused.message);
    }

    // A home replaced at the same path is another home.
    let replaced = fixture.directory.path().join("claude-replaced");
    std::fs::rename(&home, &replaced).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    assert_eq!(
        super::retained_resume::ensure_prior_boot_account(&host, &source).unwrap_err(),
        super::retained_resume::PRIOR_BOOT_ACCOUNT_CHANGED
    );
}

#[test]
fn only_the_newest_earlier_boot_owner_names_the_account_home() {
    let host = host();
    let fixture = retained_source(&host, "earlier-boot");
    let source = fixture.source.clone();
    let home = fixture.directory.path().join("claude");
    let other = fixture.directory.path().join("claude-other");
    std::fs::create_dir_all(&other).unwrap();
    // An older boot ran under another home; the newest ran under this one.
    let mut older = source.clone();
    older.proof.as_mut().unwrap().boot_id = BootId::new("older-boot");
    register_owner(&host, &older, &other);
    register_owner(&host, &source, &home);
    super::retained_resume::ensure_prior_boot_account(&host, &source).unwrap();
    // This boot's own rows never count.
    register_owner(&host, &current(&host, &source), &other);
    super::retained_resume::ensure_prior_boot_account(&host, &source).unwrap();
    // The newest earlier-boot row decides, even against the launch.
    register_owner(&host, &older, &other);
    assert_eq!(
        super::retained_resume::ensure_prior_boot_account(&host, &source).unwrap_err(),
        super::retained_resume::PRIOR_BOOT_ACCOUNT_CHANGED
    );
}

#[test]
fn a_claimed_allocation_or_paired_terminal_refuses_lost_close() {
    for case in [
        "paired-terminal-starting",
        "paired-view-spawning",
        "handoff",
        "paired-terminal-handoff",
        "actor-startup",
    ] {
        for managed in [true, false] {
            let host = host();
            let mut fixture = retained_source(&host, "earlier-boot");
            let view = paired_view(fixture.source.launch.recipe.clone());
            let paired = view.id;
            fixture.source.terminal = Some(view);
            let source = fixture.source.clone();
            assert!(source.has_valid_native_continuity());
            register_authority(&host, &source);
            let agent = managed.then(|| lost_agent(&host, fixture.directory.path(), &source));
            let authority = fixture.authority_rows(source.id);
            let handoff = || {
                TerminalHandoff::new(
                    source.clone(),
                    source.terminal.clone().unwrap(),
                    1,
                    BTreeMap::new(),
                    Instant::now() + Duration::from_secs(60),
                )
            };
            let requested = current(&host, &source);
            {
                let mut state = host.lock().unwrap();
                match case {
                    "paired-terminal-starting" => {
                        // Another conversation starting with this pane's CLI view.
                        let mut another = requested;
                        another.id = TerminalId::new();
                        state.chat_spawning.insert(another.id, another);
                    }
                    "handoff" => {
                        state.chat_handoffs.insert(source.id, handoff());
                    }
                    "paired-terminal-handoff" => {
                        state.chat_handoffs.insert(paired, handoff());
                    }
                    "paired-view-spawning" => {
                        // The CLI view's spawn is reserved; its terminal
                        // record and holder discovery record do not exist yet.
                        state.reserve_spawn(paired).unwrap();
                    }
                    _ => {
                        state.native_actor_startups.insert(
                            source.id,
                            super::retained_actor::Startup {
                                source: requested.clone(),
                                requested,
                                context: Arc::new(
                                    chat::protocol::ExecutionContext::capture(
                                        fixture.directory.path(),
                                    )
                                    .unwrap(),
                                ),
                                failed_close: None,
                            },
                        );
                    }
                }
            }
            let claims = || {
                let state = host.lock().unwrap();
                (
                    state.chat_spawning.clone(),
                    state.chat_handoffs.keys().copied().collect::<Vec<_>>(),
                    state
                        .native_actor_startups
                        .keys()
                        .copied()
                        .collect::<Vec<_>>(),
                )
            };
            let before = claims();
            if let Some(accepted) = &agent {
                let refused = close_agent(&host, accepted.agent.id).unwrap_err();
                assert!(refused.contains("may be running"), "{case}: {refused}");
            }
            let refused = super::close_retained_from_pane(&host, &source).unwrap_err();
            let expected = if managed {
                "may be running"
            } else {
                "starting or changing"
            };
            assert!(refused.contains(expected), "{case}, {managed}: {refused}");
            assert_eq!(claims(), before, "{case}: a refused close changed a claim");
            if let Some(accepted) = &agent {
                assert!(agent_open(&host, accepted.agent.id), "{case}");
            }
            assert_eq!(fixture.authority_rows(source.id), authority);
            assert!(!fixture.marker.exists());

            // The claim alone refused it.
            {
                let mut state = host.lock().unwrap();
                state.chat_spawning.clear();
                state.chat_handoffs.clear();
                state.native_actor_startups.clear();
                state.cancel_spawn(paired);
            }
            super::close_retained_from_pane(&host, &source).unwrap();
            if let Some(accepted) = &agent {
                assert!(!agent_open(&host, accepted.agent.id), "{case}");
                assert!(fixture.authority_rows(source.id).is_empty());
            } else {
                assert_eq!(fixture.authority_rows(source.id), authority);
            }
            assert!(!fixture.marker.exists());
            assert_idle(&host);
        }
    }
}

#[test]
fn a_lost_agents_retained_startup_owner_refuses_close() {
    let host = host();
    let fixture = retained_source(&host, "earlier-boot");
    let source = &fixture.source;
    register_authority(&host, source);
    let accepted = lost_agent(&host, fixture.directory.path(), source);
    let authority = fixture.authority_rows(source.id);
    host.lock().unwrap().execution_startups.insert(
        accepted.agent.id,
        (
            HostedSession::Chat(source.clone()),
            Arc::new(chat::protocol::ExecutionContext::capture(fixture.directory.path()).unwrap()),
        ),
    );
    let refused = close_agent(&host, accepted.agent.id).unwrap_err();
    assert!(
        refused.contains("Startup teardown remains unconfirmed"),
        "{refused}"
    );
    let refused = super::close_retained_from_pane(&host, source).unwrap_err();
    assert!(
        refused.contains("Startup teardown remains unconfirmed"),
        "{refused}"
    );
    assert!(agent_open(&host, accepted.agent.id));
    assert_eq!(fixture.authority_rows(source.id), authority);
    assert!(
        host.lock()
            .unwrap()
            .execution_startups
            .contains_key(&accepted.agent.id)
    );
    assert!(!fixture.marker.exists());
}

#[test]
fn a_lost_scheduled_agent_keeps_the_ordinary_refusal() {
    let host = host();
    let fixture = retained_source(&host, "earlier-boot");
    let source = &fixture.source;
    register_authority(&host, source);
    open_project(&host, fixture.directory.path());
    let automation = Origin::Automation {
        id: uuid::Uuid::new_v4(),
    };
    let accepted = {
        let registry = registry(&host);
        let mut registry = registry.lock().unwrap();
        let accepted = registry
            .submit_agent(
                &automation,
                &submit(fixture.directory.path(), "scheduled"),
                1,
            )
            .unwrap();
        registry
            .bind_agent(accepted.agent.id, None, HostedSession::Chat(source.clone()))
            .unwrap();
        registry.interrupt_unproven_agent_runs(2).unwrap();
        accepted
    };
    let authority = fixture.authority_rows(source.id);
    // No caller authority resolves to an automation's origin (only to a user
    // or a session lineage), so a Close request stops at ownership. A pane's
    // Close does reach it, and the boot change does not end it there.
    let refused = close_agent(&host, accepted.agent.id).unwrap_err();
    assert!(refused.contains("does not own"), "{refused}");
    let refused = super::close_retained_from_pane(&host, source).unwrap_err();
    assert!(
        refused.contains("No authenticated retained Claude actor"),
        "{refused}"
    );
    assert!(agent_open(&host, accepted.agent.id));
    assert_eq!(fixture.authority_rows(source.id), authority);
    assert!(!fixture.marker.exists());
    assert_idle(&host);
}
