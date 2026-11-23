use super::*;
use crate::model::{BootId, HostIncarnation, HostSessionId};

fn test_host_with_incarnation(incarnation: &str) -> HostState {
    HostState::new(HostSessionProof {
        host_session_id: HostSessionId::new("test-session"),
        boot_id: BootId::new("test-boot"),
        host_incarnation: HostIncarnation::new(incarnation),
    })
}

fn test_host() -> HostState {
    test_host_with_incarnation("test-incarnation")
}

#[test]
fn startup_state_is_answered_while_startup_recovery_refuses_other_requests() {
    use crate::terminal_ipc::{ClientRequest, ServerResponse};
    let host = Arc::new(HostLock::new(test_host()));
    let inspect =
        || match crate::host::client::process_request(&host, 0, ClientRequest::StartupState)
            .unwrap()
            .0
        {
            ServerResponse::StartupState(state) => state,
            response => panic!("unexpected startup state response: {response:?}"),
        };
    let refused = || {
        crate::host::client::process_request(&host, 0, ClientRequest::ChatCapacitySet { limit: 1 })
            .unwrap_err()
            .code
    };
    assert_eq!(inspect(), StartupState::Ready);

    let custody = TerminalId::new();
    host.lock().unwrap().startup_custody.insert(custody);
    assert_eq!(inspect(), StartupState::Restoring { awaiting: 1 });
    assert_eq!(refused(), ProtocolErrorCode::TerminalUnavailable);

    let detail = "Upgrade restore retained original ownership: fixture restore failed";
    host.lock().unwrap().retain_startup_recovery(detail.into());
    let retained = StartupState::Retained {
        detail: detail.into(),
    };
    assert_eq!(inspect(), retained);
    // Custody settling afterwards does not end a retained recovery, and a
    // later failure does not replace the original reason.
    host.lock().unwrap().startup_custody.remove(&custody);
    host.lock()
        .unwrap()
        .retain_startup_recovery("a later failure".into());
    assert_eq!(inspect(), retained);
    assert_eq!(refused(), ProtocolErrorCode::TerminalUnavailable);
}

#[test]
fn terminal_spawn_accepts_byte_budgets_and_refuses_over_limit_before_spawn() {
    assert_eq!(
        TerminalSpawnOptions::default().max_scrollback_bytes,
        16 * 1024 * 1024
    );
    for bytes in [0, 16 * 1024 * 1024, TerminalConfig::MAX_SCROLLBACK_BYTES] {
        let options = TerminalSpawnOptions {
            max_scrollback_bytes: bytes,
            ..Default::default()
        };
        validate_spawn_options(&options).unwrap();
    }
    let error = validate_spawn_options(&TerminalSpawnOptions {
        max_scrollback_bytes: TerminalConfig::MAX_SCROLLBACK_BYTES + 1,
        ..Default::default()
    })
    .unwrap_err();
    assert_eq!(error.code, ProtocolErrorCode::InvalidRequest);
    assert!(!error.retryable);
    // A byte budget does not relax any independent dimension guard.
    assert!(
        validate_spawn_options(&TerminalSpawnOptions {
            columns: 0,
            ..Default::default()
        })
        .is_err()
    );
}

#[test]
fn channel_title_admission_rejects_delayed_names_from_the_same_resident_actor() {
    use crate::{
        chat::protocol::{Adapter, ProcessSpec, Step, TransportSpec},
        chat::{self, Command, Event},
        model::{CardId, CardKind, ChannelId, ChannelTitleSource, LaunchRecipe},
        workspace_model::views::{Pane, Scope},
    };
    const SESSION: &str = "12345678-1234-4234-8234-123456789abc";
    struct Names(usize);
    impl Adapter for Names {
        fn initialize(&mut self) -> Result<Step, String> {
            Ok(Step {
                events: vec![
                    Event::Session {
                        id: SESSION.into(),
                        title: Some("Initial bundle audit".into()),
                    },
                    Event::Ready(true),
                ],
                ..Default::default()
            })
        }
        fn command(&mut self, _: Command) -> Result<Step, String> {
            self.0 += 1;
            Ok(Step {
                events: vec![Event::Session {
                    id: SESSION.into(),
                    title: Some(
                        if self.0 == 1 {
                            "Encryption performance review"
                        } else {
                            "Final encryption audit"
                        }
                        .into(),
                    ),
                }],
                ..Default::default()
            })
        }
        fn receive(&mut self, _: serde_json::Value) -> Result<Step, String> {
            Ok(Step::default())
        }
    }
    let directory = tempfile::tempdir().unwrap();
    let mut state = test_host();
    state.install_model(
        super::super::model::Registry::at(&directory.path().join("model.sqlite3")).unwrap(),
    );
    let recipe = LaunchRecipe {
        program: "claude".into(),
        arguments: Vec::new(),
        working_directory: directory.path().into(),
        environment: Default::default(),
        unset_environment: Default::default(),
    };
    let mut spec = ProcessSpec::from(&recipe);
    spec.transport = TransportSpec::History;
    let mut adapter = Names(0);
    let mut initial = adapter.initialize().unwrap();
    let store = chat::runtime::preflight(&spec, &mut initial).unwrap();
    let handle =
        chat::runtime::Handle::start(spec, Box::new(adapter), initial, store, None, |_| {})
            .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while handle
        .conversation_title()
        .ok()
        .and_then(|(session, _)| session)
        .as_deref()
        != Some(SESSION)
    {
        assert!(
            Instant::now() < deadline,
            "history worker did not publish its exact session"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    let session = chat::Session {
        id: TerminalId::new(),
        proof: Some(state.proof().clone()),
        terminal: None,
        native_continuity: Default::default(),
        owned_codex_listener: None,
        launch: chat::Launch {
            provider: crate::agent::Provider::Claude,
            recipe,
            session_id: Some(SESSION.into()),
        },
    };
    state.chats.insert(
        session.id,
        super::super::chat::Record {
            execution_instance: uuid::Uuid::new_v4(),
            session: session.clone(),
            handle: handle.clone(),
            console_recipe: None,
            paired_terminal: None,
            source_process: None,
            subscribers: BTreeMap::new(),
            observations: BTreeMap::new(),
            completion_observed: false,
            returning_terminal: false,
        },
    );
    let id = ChannelId::new();
    let card = CardId::new();
    state
        .edit_model(
            1,
            ModelEdit::OpenProject {
                repository: directory.path().into(),
                name: "project".into(),
                kind: Default::default(),
            },
        )
        .unwrap();
    state
        .edit_model(
            1,
            ModelEdit::CreateChannel {
                id,
                repository: directory.path().into(),
                title: String::new(),
                title_source: Default::default(),
                checkout: None,
            },
        )
        .unwrap();
    let model = state
        .edit_model(
            1,
            ModelEdit::SeedView {
                scope: Scope::Channel(id),
                closed: Vec::new(),
                panes: vec![Pane {
                    id: card,
                    title: "Chat".into(),
                    user_title: None,
                    pending_terminal_start: false,
                    width_fraction: 1.,
                    scope: Some(directory.path().into()),
                    kind: CardKind::Chat(session),
                }],
            },
        )
        .unwrap();
    let revision = model.views[0].revision;
    let suggest = |name: &str| ModelEdit::SuggestConversationTitle {
        id,
        card,
        expected_revision: revision,
        title: name.into(),
        source: ChannelTitleSource::Provider,
    };
    let apply = |state: &mut HostState, origin, edit: ModelEdit| {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match state.edit_model(origin, edit.clone()) {
                Err(error) if error.retryable && error.code == ProtocolErrorCode::QueueFull => {
                    assert!(Instant::now() < deadline, "resident metadata remained busy");
                    std::thread::yield_now();
                }
                result => break result,
            }
        }
    };
    apply(&mut state, 1, suggest("Initial bundle audit")).unwrap();
    handle.command(Command::Compact).unwrap();
    apply(&mut state, 2, suggest("Encryption performance review")).unwrap();
    assert!(
        apply(&mut state, 1, suggest("Initial bundle audit")).is_err(),
        "delayed A must not replace accepted B at the same view revision"
    );
    assert_eq!(
        state.read_model().unwrap().channels[0].title,
        "Encryption performance review"
    );
    handle.command(Command::Compact).unwrap();
    apply(&mut state, 1, suggest("Final encryption audit")).unwrap();
    assert_eq!(
        state.read_model().unwrap().channels[0].title,
        "Final encryption audit"
    );
    handle.close().unwrap();
}

#[test]
fn a_pane_that_has_not_recorded_its_conversation_takes_its_first_prompt_name() {
    use crate::{
        chat::protocol::{Adapter, ProcessSpec, Step, TransportSpec},
        chat::{self, Command, Event},
        model::{CardId, CardKind, ChannelId, ChannelTitleSource, LaunchRecipe},
        workspace_model::views::{Pane, Scope},
    };
    const SESSION: &str = "12345678-1234-4234-8234-123456789abc";
    // A provider that has reported its conversation, with or without a
    // title, or that has not reported one yet, as Claude before it reads
    // a first prompt.
    struct Reports(Option<&'static str>, Option<&'static str>);
    impl Adapter for Reports {
        fn initialize(&mut self) -> Result<Step, String> {
            let session = self.0.map(|id| Event::Session {
                id: id.into(),
                title: self.1.map(str::to_owned),
            });
            Ok(Step {
                events: session.into_iter().chain([Event::Ready(true)]).collect(),
                ..Default::default()
            })
        }
        fn command(&mut self, _: Command) -> Result<Step, String> {
            Ok(Step::default())
        }
        fn receive(&mut self, _: serde_json::Value) -> Result<Step, String> {
            Ok(Step::default())
        }
    }
    let directory = tempfile::tempdir().unwrap();
    let mut state = test_host();
    state.install_model(
        super::super::model::Registry::at(&directory.path().join("model.sqlite3")).unwrap(),
    );
    let recipe = LaunchRecipe {
        program: "claude".into(),
        arguments: Vec::new(),
        working_directory: directory.path().into(),
        environment: Default::default(),
        unset_environment: Default::default(),
    };
    let start = |session: Option<&'static str>, title: Option<&'static str>| {
        let mut spec = ProcessSpec::from(&recipe);
        spec.transport = TransportSpec::History;
        let mut adapter = Reports(session, title);
        let mut initial = adapter.initialize().unwrap();
        let store = chat::runtime::preflight(&spec, &mut initial).unwrap();
        let handle =
            chat::runtime::Handle::start(spec, Box::new(adapter), initial, store, None, |_| {})
                .unwrap();
        let expected = Some((session.map(str::to_owned), title.map(str::to_owned)));
        crate::test_timing::wait_for("the history worker to publish its exact session", || {
            (handle.conversation_title().ok() == expected).then_some(())
        });
        handle
    };
    let reported = start(Some(SESSION), None);
    let unreported = start(None, None);
    let titled = start(Some(SESSION), Some("Roadmap review"));
    // The window shows the chat as it started: before its provider
    // reported the conversation the daemon's chat now holds.
    let session = chat::Session {
        id: TerminalId::new(),
        proof: Some(state.proof().clone()),
        terminal: None,
        native_continuity: Default::default(),
        owned_codex_listener: None,
        launch: chat::Launch {
            provider: crate::agent::Provider::Claude,
            recipe,
            session_id: None,
        },
    };
    state
        .edit_model(
            1,
            ModelEdit::OpenProject {
                repository: directory.path().into(),
                name: "project".into(),
                kind: Default::default(),
            },
        )
        .unwrap();
    let pane = |card, session: chat::Session| Pane {
        id: card,
        title: "Chat".into(),
        user_title: None,
        pending_terminal_start: false,
        width_fraction: 1.,
        scope: Some(directory.path().into()),
        kind: CardKind::Chat(session),
    };
    // Each channel's pane shows its own resident chat of the same
    // conversation.
    let channel =
        |state: &mut HostState, handle: &chat::runtime::Handle, mut session: chat::Session| {
            session.id = TerminalId::new();
            state.chats.insert(
                session.id,
                super::super::chat::Record {
                    execution_instance: uuid::Uuid::new_v4(),
                    session: session.clone(),
                    handle: handle.clone(),
                    console_recipe: None,
                    paired_terminal: None,
                    source_process: None,
                    subscribers: BTreeMap::new(),
                    observations: BTreeMap::new(),
                    completion_observed: false,
                    returning_terminal: false,
                },
            );
            let id = ChannelId::new();
            let card = CardId::new();
            state
                .edit_model(
                    1,
                    ModelEdit::CreateChannel {
                        id,
                        repository: directory.path().into(),
                        title: String::new(),
                        title_source: Default::default(),
                        checkout: None,
                    },
                )
                .unwrap();
            let model = state
                .edit_model(
                    1,
                    ModelEdit::SeedView {
                        scope: Scope::Channel(id),
                        closed: Vec::new(),
                        panes: vec![pane(card, session)],
                    },
                )
                .unwrap();
            let revision = model
                .views
                .iter()
                .find(|view| view.scope == Scope::Channel(id))
                .unwrap()
                .revision;
            (id, card, revision)
        };
    let suggest_as = |(id, card, revision): (ChannelId, CardId, u64), title: &str, source| {
        ModelEdit::SuggestConversationTitle {
            id,
            card,
            expected_revision: revision,
            title: title.into(),
            source,
        }
    };
    let suggest = |channel| {
        suggest_as(
            channel,
            "Plan the quarterly roadmap review",
            ChannelTitleSource::Prompt,
        )
    };
    let title = |state: &HostState, id| {
        state
            .read_model()
            .unwrap()
            .channels
            .into_iter()
            .find(|channel| channel.id == id)
            .unwrap()
            .title
    };

    let unrecorded = channel(&mut state, &reported, session.clone());
    state.edit_model(1, suggest(unrecorded)).unwrap();
    assert_eq!(
        title(&state, unrecorded.0),
        "Plan the quarterly roadmap review"
    );
    // Nor does the provider need to have reported the conversation.
    let unreported_channel = channel(&mut state, &unreported, session.clone());
    state.edit_model(1, suggest(unreported_channel)).unwrap();
    assert_eq!(
        title(&state, unreported_channel.0),
        "Plan the quarterly roadmap review"
    );

    // The title rules still hold for such a pane: once the provider has
    // titled the conversation, a first-prompt name and any other name
    // than the provider's are refused, and the provider's is accepted.
    for (name, source) in [
        (
            "Plan the quarterly roadmap review",
            ChannelTitleSource::Prompt,
        ),
        ("Encryption audit", ChannelTitleSource::Provider),
    ] {
        let refused = channel(&mut state, &titled, session.clone());
        let error = state
            .edit_model(1, suggest_as(refused, name, source))
            .unwrap_err();
        assert_eq!(error.code, ProtocolErrorCode::InvalidRequest);
        assert_eq!(title(&state, refused.0), "");
    }
    let provider = channel(&mut state, &titled, session.clone());
    state
        .edit_model(
            1,
            suggest_as(provider, "Roadmap review", ChannelTitleSource::Provider),
        )
        .unwrap();
    assert_eq!(title(&state, provider.0), "Roadmap review");

    // A pane that recorded another conversation is still refused, and so
    // is one whose provider has not confirmed the recorded one.
    let mut other = session.clone();
    other.launch.session_id = Some("87654321-4321-4321-8321-cba987654321".into());
    let mut recorded = session;
    recorded.launch.session_id = Some(SESSION.into());
    for (handle, pane) in [(&reported, other), (&unreported, recorded.clone())] {
        let refused = channel(&mut state, handle, pane);
        let error = state.edit_model(1, suggest(refused)).unwrap_err();
        assert_eq!(error.code, ProtocolErrorCode::InvalidRequest);
        assert_eq!(title(&state, refused.0), "");
    }

    // The recorded conversation itself is accepted, as before.
    let recorded = channel(&mut state, &reported, recorded);
    state.edit_model(1, suggest(recorded)).unwrap();
    assert_eq!(
        title(&state, recorded.0),
        "Plan the quarterly roadmap review"
    );
    reported.close().unwrap();
    unreported.close().unwrap();
    titled.close().unwrap();
}

#[test]
fn failed_shutdown_keeps_terminal_owned_and_forbids_fresh_replacement() {
    let recipe = crate::model::LaunchRecipe {
        unset_environment: Default::default(),
        program: "/bin/sh".into(),
        arguments: vec!["-c".into(), "trap '' HUP; while :; do sleep 1; done".into()],
        working_directory: PathBuf::from("/"),
        environment: Default::default(),
    };
    let session = TerminalSession {
        id: TerminalId::new(),
        launch: recipe.clone(),
        transcript_path: None,
        resume_hint: None,
        owned_codex_listener: None,
        recovery_origin: None,
        predecessor: None,
        continuity: TerminalContinuity::RelaunchAvailable {
            recipe,
            last_exit: None,
        },
    };
    let id = session.id;
    let handle = TerminalHandle::spawn_with_missing_exit_report_for_test(session.clone());
    let mut host = test_host();
    host.finish_spawn(0, session, handle.clone(), true).unwrap();
    assert!(handle.shutdown().is_err());
    assert!(
        !handle.is_completed(),
        "joining a stopped parser is not root-exit confirmation"
    );
    assert_eq!(host.inventory_count(), 1);
    assert!(!host.terminals[&id].completion_observed);
    assert!(matches!(
        host.descriptor(0, id).unwrap().session.continuity,
        TerminalContinuity::Live { .. }
    ));
    assert!(
        !handle
            .events()
            .try_iter()
            .any(|event| matches!(event, TerminalEvent::Completed { .. }))
    );
}

#[test]
fn pending_terminal_completion_is_idempotent_and_fenced_by_its_pane() {
    use crate::workspace_model::views::{Change, Pane, Scope};
    let directory = tempfile::tempdir().unwrap();
    let repository = directory.path().to_owned();
    let mut registry =
        super::super::model::Registry::at(&directory.path().join("model.sqlite3")).unwrap();
    registry
        .apply(&ModelEdit::OpenProject {
            repository: repository.clone(),
            name: "test".into(),
            kind: Default::default(),
        })
        .unwrap();
    let scope = Scope::Worktree {
        repository: repository.clone(),
        path: repository.clone(),
    };
    let launch = crate::model::LaunchRecipe::shell(repository).unwrap();
    let session = TerminalSession {
        id: TerminalId::new(),
        launch: launch.clone(),
        transcript_path: None,
        resume_hint: None,
        owned_codex_listener: None,
        recovery_origin: None,
        predecessor: None,
        continuity: TerminalContinuity::RelaunchAvailable {
            recipe: launch,
            last_exit: None,
        },
    };
    let pane = Pane {
        id: crate::model::CardId::new(),
        title: "terminal".into(),
        user_title: None,
        pending_terminal_start: true,
        width_fraction: 0.5,
        scope: None,
        kind: crate::model::CardKind::Terminal(session.clone()),
    };
    registry
        .apply(&ModelEdit::SeedView {
            scope: scope.clone(),
            panes: vec![pane.clone()],
            closed: vec![],
        })
        .unwrap();
    let mut state = test_host();
    state.install_model(registry);
    state.register_client(1);
    state.register_client(2);
    let host = Arc::new(HostLock::new(state));
    // Both clients observed the reservation before either actor existed.
    let first = spawn_pending_terminal(
        &host,
        1,
        pane.id,
        session.id,
        TerminalSpawnOptions::default(),
    )
    .unwrap();
    let second = spawn_pending_terminal(
        &host,
        2,
        pane.id,
        session.id,
        TerminalSpawnOptions::default(),
    )
    .unwrap();
    assert_eq!(first.session.id, second.session.id);
    let handle = {
        let mut state = host.lock().unwrap();
        assert_eq!(state.terminals.len(), 1);
        let view = state.read_model().unwrap().views.pop().unwrap();
        assert!(!view.panes[0].pending_terminal_start);
        state
            .model
            .as_mut()
            .unwrap()
            .apply(&ModelEdit::ChangeView {
                scope,
                expected_revision: view.revision,
                operation: uuid::Uuid::new_v4(),
                change: Change::Remove { id: pane.id },
            })
            .unwrap();
        state.terminals[&session.id].handle.clone()
    };
    let error = spawn_pending_terminal(
        &host,
        2,
        pane.id,
        session.id,
        TerminalSpawnOptions::default(),
    )
    .unwrap_err();
    assert_eq!(error.code, ProtocolErrorCode::TerminalNotFound);
    handle.shutdown().unwrap();
}

#[test]
fn browser_login_claim_is_atomic_across_windows_and_released_after_failed_spawn() {
    let shared = Arc::new(crate::host::state::HostLock::new(test_host()));
    let barrier = Arc::new(std::sync::Barrier::new(2));
    let attempts: Vec<_> = (0..2)
        .map(|_| {
            let shared = shared.clone();
            let barrier = barrier.clone();
            thread::spawn(move || {
                let id = TerminalId::new();
                barrier.wait();
                let mut state = shared.lock().unwrap();
                state.reserve_spawn(id).unwrap();
                match state.reserve_browser_login(id) {
                    Ok(()) => Some(id),
                    Err(error) => {
                        assert_eq!(error.message, crate::account::login::BUSY);
                        state.cancel_spawn(id);
                        None
                    }
                }
            })
        })
        .collect();
    let winners: Vec<_> = attempts
        .into_iter()
        .filter_map(|attempt| attempt.join().unwrap())
        .collect();
    assert_eq!(winners.len(), 1);
    let mut state = shared.lock().unwrap();
    state.cancel_spawn(winners[0]);
    assert!(state.reserve_browser_login(TerminalId::new()).is_ok());
}

#[test]
fn browser_login_claim_survives_ui_disconnect_until_actual_worker_teardown() {
    use std::os::unix::fs::PermissionsExt;
    let directory = tempfile::tempdir().unwrap();
    let program = directory.path().join("codex");
    fs::write(&program, "#!/bin/sh\nexec /bin/cat\n").unwrap();
    fs::set_permissions(&program, fs::Permissions::from_mode(0o755)).unwrap();
    let mut recipe = crate::model::LaunchRecipe::shell(directory.path().into()).unwrap();
    recipe.program = program.to_str().unwrap().into();
    recipe.arguments = vec!["login".into()];
    let session = TerminalSession {
        owned_codex_listener: None,
        recovery_origin: None,
        predecessor: None,
        id: TerminalId::new(),
        launch: recipe.clone(),
        transcript_path: None,
        resume_hint: None,
        continuity: TerminalContinuity::RelaunchAvailable {
            recipe,
            last_exit: None,
        },
    };
    let mut state = test_host();
    state.register_client(1);
    state.reserve_spawn(session.id).unwrap();
    state.reserve_browser_login(session.id).unwrap();
    let handle = TerminalHandle::spawn(session.clone(), TerminalConfig::default(), |_| {}).unwrap();
    state
        .finish_spawn(1, session, handle.clone(), true)
        .unwrap();
    state.unregister_client(1);
    state.register_client(2);
    let blocked = state.reserve_browser_login(TerminalId::new()).is_err();
    handle.shutdown().unwrap();
    assert!(blocked, "a UI restart must not release its running login");
    assert!(handle.is_completed());
    assert!(state.reserve_browser_login(TerminalId::new()).is_ok());
}

#[test]
fn browser_callback_failure_survives_hidden_terminal_completion() {
    let mut recipe = crate::model::LaunchRecipe::shell("/".into()).unwrap();
    recipe.program = "/bin/sh".into();
    recipe.arguments = vec![
        "-c".into(),
        "printf '%s\\n' 'Error: Port 127.0.0.1:1455 is already in use' >&2; exit 1".into(),
    ];
    let session = TerminalSession {
        owned_codex_listener: None,
        recovery_origin: None,
        predecessor: None,
        id: TerminalId::new(),
        launch: recipe.clone(),
        transcript_path: None,
        resume_hint: None,
        continuity: TerminalContinuity::RelaunchAvailable {
            recipe,
            last_exit: None,
        },
    };
    let handle = TerminalHandle::spawn(
        session,
        TerminalConfig {
            initially_visible: false,
            ..TerminalConfig::default()
        },
        |_| {},
    )
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !handle.is_completed() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    if !handle.is_completed() {
        let _ = handle.shutdown();
        panic!("fixture login did not finish");
    }
    let text = handle
        .snapshot()
        .lines
        .iter()
        .map(|row| {
            row.cells
                .iter()
                .map(|cell| cell.text.as_str())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        crate::account::login::callback_failed(&text),
        "a hibernated login must retain its callback failure"
    );
}

#[test]
fn notification_activation_prefers_the_focused_owner_and_restores_closed_window_routes() {
    use crate::notifications::Presence;
    let session = TerminalId::new();
    let mut host = test_host();
    let focused = host.register_client(1);
    host.register_client(2);
    host.watch_notifications(
        1,
        Presence {
            window: "primary".into(),
            application: Default::default(),
            focused_target: None,
            sessions: vec![session],
            focused: Some(session),
        },
    );
    host.watch_notifications(
        2,
        Presence {
            window: "second".into(),
            application: Default::default(),
            focused_target: None,
            sessions: vec![session],
            focused: None,
        },
    );
    assert_eq!(
        host.activate_notification(
            crate::notifications::Target::Session(session),
            Some(session),
            Some("second")
        ),
        "primary"
    );
    assert!(
        matches!(focused.pop().unwrap().message, ServerMessage::NotificationActivated { target: found } if found == crate::notifications::Target::Session(session))
    );
    drop(host.unregister_client(1));
    drop(host.unregister_client(2));
    assert_eq!(
        host.activate_notification(
            crate::notifications::Target::Session(session),
            Some(session),
            Some("second")
        ),
        "second"
    );
    host.register_client(3);
    let presence = Presence {
        window: "second".into(),
        application: Default::default(),
        focused_target: None,
        sessions: vec![session],
        focused: None,
    };
    assert_eq!(
        host.watch_notifications(3, presence.clone()),
        Some(crate::notifications::Target::Session(session))
    );
    assert_eq!(host.watch_notifications(3, presence), None);
}

#[test]
fn run_notification_routes_only_to_a_cagents_window_without_a_provider_pane() {
    let mut host = test_host();
    let delivery = host.register_client(1);
    host.watch_notifications(
        1,
        crate::notifications::Presence {
            window: "primary".into(),
            ..Default::default()
        },
    );
    let editor = host.register_client(2);
    host.watch_notifications(
        2,
        crate::notifications::Presence {
            window: "primary".into(),
            application: crate::notifications::Application::Ceditor,
            ..Default::default()
        },
    );
    let target = crate::notifications::Target::AgentRun(crate::model::RunId::new());
    assert_eq!(
        host.activate_notification(target.clone(), None, None),
        "primary"
    );
    delivery.close_after_drain();
    editor.close_after_drain();
    assert!(matches!(delivery.pop().unwrap().message,
        ServerMessage::NotificationActivated { target: actual } if actual == target));
    assert!(editor.pop().is_none());
}

#[test]
fn automation_notification_routes_to_cagents_without_a_provider_and_reconnects_once() {
    use crate::notifications::{Application, Presence, Target};
    let mut host = test_host();
    let agents = host.register_client(1);
    host.watch_notifications(
        1,
        Presence {
            window: "primary".into(),
            ..Default::default()
        },
    );
    let editor = host.register_client(2);
    host.watch_notifications(
        2,
        Presence {
            window: "primary".into(),
            application: Application::Ceditor,
            ..Default::default()
        },
    );
    let id = uuid::Uuid::new_v4();
    let first = Target::Automation { id, sequence: 41 };
    let destination = host.activate_notification_destination(first.clone(), None, None, None);
    assert_eq!(destination.application, Application::Cagents);
    assert_eq!(destination.window, "primary");
    assert!(matches!(agents.pop().unwrap().message,
        ServerMessage::NotificationActivated { target } if target == first));
    drop(host.unregister_client(1));
    let replacement = Target::Automation { id, sequence: 42 };
    let cold = host.activate_notification_destination(replacement.clone(), None, None, None);
    assert_eq!(cold.application, Application::Cagents);
    assert_eq!(cold.window, "primary");
    // A same-window editor does not receive or consume Cagents attention.
    let editor_presence = Presence {
        window: "primary".into(),
        application: Application::Ceditor,
        ..Default::default()
    };
    assert_eq!(host.watch_notifications(2, editor_presence), None);
    editor.close_after_drain();
    assert!(editor.pop().is_none());
    host.register_client(3);
    let returned = Presence {
        window: "primary".into(),
        ..Default::default()
    };
    assert_eq!(
        host.watch_notifications(3, returned.clone()),
        Some(replacement)
    );
    assert_eq!(host.watch_notifications(3, returned), None);
}

#[test]
fn review_activation_survives_cold_editor_reconnect_without_crossing_shells() {
    use crate::notifications::{Application, Destination, Presence, Target, review::ReviewRequest};
    let mut host = test_host();
    let target = Target::PullRequest(ReviewRequest {
        host: "github.com".into(),
        port: None,
        owner: "o".into(),
        repository: "r".into(),
        number: 51,
        viewer: "viewer".into(),
        request: "request".into(),
    });
    let destination = Destination {
        application: Application::Ceditor,
        window: "shared-slot".into(),
    };
    host.register_client(1);
    let agents = Presence {
        window: destination.window.clone(),
        ..Default::default()
    };
    host.watch_notifications(1, agents.clone());
    assert_eq!(
        host.activate_notification_destination(target.clone(), None, None, Some(&destination)),
        destination
    );
    assert_eq!(
        host.watch_notifications(1, agents),
        None,
        "same slot in Cagents has no editor authority"
    );
    host.register_client(2);
    let editor = Presence {
        window: destination.window.clone(),
        application: Application::Ceditor,
        ..Default::default()
    };
    assert_eq!(host.watch_notifications(2, editor.clone()), Some(target));
    assert_eq!(
        host.watch_notifications(2, editor),
        None,
        "activation is consumed once"
    );
}

#[test]
fn explicit_close_fences_attention_without_discarding_completed_history() {
    let recipe = crate::model::LaunchRecipe {
        unset_environment: Default::default(),
        program: "/bin/sh".into(),
        arguments: vec!["-c".into(), "exit 0".into()],
        working_directory: std::env::temp_dir(),
        environment: BTreeMap::new(),
    };
    let id = TerminalId::new();
    let session = TerminalSession {
        id,
        launch: recipe.clone(),
        transcript_path: None,
        resume_hint: None,
        owned_codex_listener: None,
        recovery_origin: None,
        predecessor: None,
        continuity: TerminalContinuity::RelaunchAvailable {
            recipe,
            last_exit: None,
        },
    };
    let handle = TerminalHandle::spawn(session.clone(), TerminalConfig::default(), |_| {}).unwrap();
    let mut host = test_host();
    host.register_client(1);
    host.reserve_spawn(id).unwrap();
    host.finish_spawn(1, session.clone(), handle.clone(), false)
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !handle.is_completed() {
        assert!(Instant::now() < deadline, "fixture did not finish");
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(
        !host.notification_closed.contains(&id),
        "natural completion retains attention"
    );
    host.close_terminal_attention(id, Some(&handle));
    assert!(host.notification_closed.contains(&id));
    assert!(
        host.terminals.contains_key(&id),
        "completed history remains readable"
    );
    host.retire_terminal(id);
    assert!(
        !host.notification_closed.contains(&id),
        "fence storage follows record lifetime"
    );
    let replacement =
        TerminalHandle::spawn(session.clone(), TerminalConfig::default(), |_| {}).unwrap();
    host.reserve_spawn(id).unwrap();
    host.finish_spawn(1, session, replacement.clone(), false)
        .unwrap();
    host.close_terminal_attention(id, Some(&handle));
    host.close_terminal_attention(id, None);
    assert!(
        !host.notification_closed.contains(&id),
        "stale closure must not fence a replacement worker"
    );
    replacement.shutdown().unwrap();
    host.close_terminal_attention(id, Some(&replacement));
    assert!(host.notification_closed.contains(&id));
}

#[test]
pub(super) fn upgrade_agent_relaunch_requires_opt_in_and_survives_repeated_restore() {
    if !upgrade_fixture_in_fresh_process() {
        return;
    }
    use crate::upgrade::handoff::Store;
    let directory = tempfile::tempdir().unwrap();
    // A real foreground process, recognized by its provider argv. Its
    // executable is deliberately different from the recovery command, and
    // it ignores the resume picker selector a relaunch adds.
    let executable = directory.path().join("claude");
    fs::write(&executable, "#!/bin/bash\nexec -a \"$0\" /bin/cat\n").unwrap();
    fs::set_permissions(
        &executable,
        std::os::unix::fs::PermissionsExt::from_mode(0o755),
    )
    .unwrap();
    let recipe = crate::model::LaunchRecipe {
        unset_environment: Default::default(),
        program: executable.to_string_lossy().into_owned(),
        arguments: Vec::new(),
        working_directory: directory.path().into(),
        environment: BTreeMap::from([
            (
                "HOME".into(),
                directory.path().to_string_lossy().into_owned(),
            ),
            // Keep Claude's account state in this fixture, not the real home.
            (
                "CLAUDE_CONFIG_DIR".into(),
                directory
                    .path()
                    .join(".claude")
                    .to_string_lossy()
                    .into_owned(),
            ),
        ]),
    };
    let source = TerminalSession {
        owned_codex_listener: None,
        recovery_origin: None,
        predecessor: None,
        id: TerminalId::new(),
        launch: recipe.clone(),
        transcript_path: None,
        resume_hint: None,
        continuity: TerminalContinuity::RelaunchAvailable {
            recipe,
            last_exit: None,
        },
    };
    let mut host = Arc::new(crate::host::state::HostLock::new(test_host()));
    spawn_terminal(&host, 0, source.clone(), TerminalSpawnOptions::default()).unwrap();
    let empty_proc = directory.path().join("proc");
    fs::create_dir(&empty_proc).unwrap();
    for incarnation in ["second", "third"] {
        let handle = host.lock().unwrap().terminals[&source.id].handle.clone();
        let deadline = Instant::now() + Duration::from_secs(5);
        while handle.observe_agent_process().is_none() {
            assert!(Instant::now() < deadline, "agent never became foreground");
            thread::sleep(Duration::from_millis(10));
        }
        let capability = host
            .lock()
            .unwrap()
            .shutdown_preflight(Instant::now())
            .unwrap()
            .capability;
        assert!(
            prepare_upgrade_with_policy(&host, capability, false, None, || Store::open(
                directory.path()
            ))
            .is_err()
        );
        assert!(!handle.is_completed(), "refusal must preserve the agent");
        let capability = host
            .lock()
            .unwrap()
            .shutdown_preflight(Instant::now())
            .unwrap()
            .capability;
        assert_eq!(
            prepare_upgrade_with_policy(&host, capability, true, None, || Store::open(
                directory.path()
            ))
            .unwrap(),
            (1, 0)
        );
        finish_upgrade(&host).unwrap();
        let saved = Store::open(directory.path())
            .unwrap()
            .load()
            .unwrap()
            .unwrap();
        assert_eq!(
            saved.terminals[0].recipe.program,
            executable.to_string_lossy()
        );
        assert!(!saved.terminals[0].relaunches.is_empty());
        // With no conversation identified, the agent reopens its picker.
        assert_eq!(saved.terminals[0].recipe.arguments, ["--resume"]);
        host = Arc::new(crate::host::state::HostLock::new(
            test_host_with_incarnation(incarnation),
        ));
        restore_upgrade_from(
            &host,
            Store::open(directory.path()).unwrap(),
            &empty_proc,
            None,
        )
        .unwrap();
        let descriptor = host.lock().unwrap().descriptor(0, source.id).unwrap();
        assert!(!descriptor.upgrade_relaunches.is_empty());
    }
    let handle = host.lock().unwrap().terminals[&source.id].handle.clone();
    assert_eq!(
        handle.close_checked(true),
        crate::terminal_ipc::TerminalCloseResult::Closed
    );
}

/// The real CLI names its session only on the frames of a turn, and a
/// provider can stop on its own. Neither an idle resumed conversation nor
/// one that already ended may hold an update hostage.
#[test]
pub(super) fn idle_and_ended_claude_conversations_do_not_block_an_upgrade() {
    if !upgrade_fixture_in_fresh_process() {
        return;
    }
    use crate::{
        agent::Provider,
        chat::{Command, Launch, Session},
        model::LaunchRecipe,
        terminal_ipc::ClientRequest,
        update::impact::{Disposition, Policy},
        upgrade::handoff::Store,
    };
    use std::os::unix::fs::PermissionsExt;
    const IDLE: &str = "12345678-1234-4234-8234-123456789abc";
    const ENDED: &str = "98765432-1234-4234-8234-123456789abc";
    const OTHER: &str = "aaaaaaaa-1234-4234-8234-123456789abc";
    let directory = tempfile::tempdir().unwrap();
    let cwd = directory.path().canonicalize().unwrap();
    let executable = cwd.join("claude");
    // Silent until asked, like the CLI. An interrupt makes it claim a
    // different conversation, which the adapter refuses as a failure.
    fs::write(&executable, r#"#!/usr/bin/python3
import json, os, sys
for line in sys.stdin:
    request = json.loads(line)
    if request["request"]["subtype"] == "interrupt":
        print(json.dumps({"type": "system", "subtype": "init", "session_id": os.environ["OTHER_SESSION"]}), flush=True)
    print(json.dumps({"type": "control_response", "response": {"subtype": "success", "request_id": request["request_id"], "response": {}}}), flush=True)
"#).unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
    let config = cwd.join("config");
    let project: String = cwd
        .to_str()
        .unwrap()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let history = config.join("projects").join(project);
    fs::create_dir_all(&history).unwrap();
    for id in [IDLE, ENDED] {
        fs::write(history.join(format!("{id}.jsonl")), "").unwrap();
    }
    let host = Arc::new(crate::host::state::HostLock::new(test_host()));
    let start = |id: &str| {
        let session = Session {
            native_continuity: Default::default(),
            owned_codex_listener: None,
            id: TerminalId::new(),
            launch: Launch {
                provider: Provider::Claude,
                recipe: LaunchRecipe {
                    unset_environment: Default::default(),
                    program: executable.to_string_lossy().into_owned(),
                    arguments: vec!["--resume".into(), id.into()],
                    working_directory: cwd.clone(),
                    environment: BTreeMap::from([
                        (
                            "CLAUDE_CONFIG_DIR".into(),
                            config.to_string_lossy().into_owned(),
                        ),
                        ("OTHER_SESSION".into(), OTHER.into()),
                    ]),
                },
                session_id: Some(id.into()),
            },
            proof: Some(
                host.lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .proof()
                    .clone(),
            ),
            terminal: None,
        };
        super::super::chat::process(
            &host,
            1,
            ClientRequest::ChatStart {
                session: session.clone(),
                source: None,
                terminate_source: false,
                execution_environment: Default::default(),
            },
        )
        .unwrap();
        let handle = host.lock().unwrap_or_else(|p| p.into_inner()).chats[&session.id]
            .handle
            .clone();
        (session, handle)
    };
    let wait = |condition: &dyn Fn() -> bool, failure: &str| {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !condition() {
            assert!(Instant::now() < deadline, "{failure}");
            thread::sleep(Duration::from_millis(10));
        }
    };
    let (idle, idle_handle) = start(IDLE);
    let (ended, ended_handle) = start(ENDED);
    for (handle, id) in [(&idle_handle, IDLE), (&ended_handle, ENDED)] {
        wait(
            &|| handle.confirmed_session_id().as_deref() == Some(id),
            "an idle resumed conversation was never confirmed",
        );
    }
    ended_handle.command(Command::Interrupt).unwrap();
    wait(
        &|| ended_handle.is_completed() && ended_handle.failure().is_some(),
        "the provider did not end with a failure",
    );
    assert!(ended_handle.owns_work());

    let impact = host
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .upgrade_impact(Policy::WhenIdle)
        .unwrap();
    for session in &impact.sessions {
        assert_eq!(
            session.disposition,
            Disposition::Resumes,
            "{}",
            session.reason
        );
    }
    assert_eq!(impact.sessions.len(), 2);
    assert!(impact.ready());
    // The confirmed path fences live actors; one that stopped has none.
    assert!(
        update::inspect(&host, Policy::WhenIdle, true)
            .unwrap()
            .ready()
    );

    let owners = crate::upgrade::recovery::checkpointed_owners(&impact);
    let store = Store::open(&cwd.join("checkpoint")).unwrap();
    prepare_upgrade_with(&host, impact.capability, || Ok(store)).unwrap();
    finish_upgrade(&host).unwrap();
    let store = Store::open(&cwd.join("checkpoint")).unwrap();
    let saved = store.load().unwrap().unwrap();
    saved.require_ready().unwrap();
    // Only the live conversation is the successor's to restart; the
    // window reopens the ended one from its saved descriptor.
    assert_eq!(saved.chats.len(), 1);
    assert_eq!(saved.chats[0].source, idle);
    // The update worker refuses a checkpoint that omits an owner it
    // requires. The ended conversation is reviewed but not carried, so the
    // worker must require only what this checkpoint names.
    use crate::update::impact::SessionKind;
    let named: BTreeSet<_> = (saved
        .chats
        .iter()
        .map(|chat| (SessionKind::NativeChat, chat.source.id)))
    .chain(
        saved
            .terminals
            .iter()
            .map(|terminal| (SessionKind::Terminal, terminal.source.id)),
    )
    .chain(
        saved
            .carried
            .iter()
            .map(|terminal| (SessionKind::Terminal, terminal.source.id)),
    )
    .collect();
    assert!(
        owners.iter().all(|owner| named.contains(owner)),
        "the worker requires {owners:?}, but the checkpoint names only {named:?}"
    );
    assert!(!owners.iter().any(|(_, id)| *id == ended.id));
    let empty_proc = cwd.join("proc");
    fs::create_dir(&empty_proc).unwrap();
    let replacement = Arc::new(crate::host::state::HostLock::new(
        test_host_with_incarnation("replacement-incarnation"),
    ));
    // Restoring waits for the provider's confirmation, with no prompt.
    restore_upgrade_from(&replacement, store, &empty_proc, None).unwrap();
    let restored = super::super::chat::resolve_upgrade(&replacement, &idle).unwrap();
    assert_eq!(restored.launch.session_id.as_deref(), Some(IDLE));
    assert!(super::super::chat::resolve_upgrade(&replacement, &ended).is_err());
}

#[test]
pub(super) fn native_upgrade_retains_hidden_lineage_and_restarts_final_claude_conversation_once() {
    if !upgrade_fixture_in_fresh_process() {
        return;
    }
    use crate::{
        agent::Provider,
        chat::{Command, Launch, Session},
        model::LaunchRecipe,
        terminal_ipc::ClientRequest,
        upgrade::handoff::Store,
    };
    use std::os::unix::fs::PermissionsExt;
    const A: &str = "12345678-1234-4234-8234-123456789abc";
    const B: &str = "98765432-1234-4234-8234-123456789abc";
    let directory = tempfile::tempdir().unwrap();
    let cwd = directory.path().canonicalize().unwrap();
    let executable = cwd.join("claude");
    let log = cwd.join("launches");
    fs::write(&executable, r#"#!/usr/bin/python3
import json, os, sys
identity = next(arg.split("=", 1)[1] for arg in sys.argv if arg.startswith("--resume="))
with open(os.environ["LAUNCH_LOG"], "a") as log:
    log.write(json.dumps(sys.argv[1:]) + "\n")
print(json.dumps({"type": "system", "subtype": "init", "session_id": identity}), flush=True)
for line in sys.stdin:
    request = json.loads(line)
    if request["request"]["subtype"] == "interrupt":
        print(json.dumps({"type": "conversation_reset", "session_id": identity}), flush=True)
        identity = os.environ["NEXT_SESSION"]
        print(json.dumps({"type": "system", "subtype": "init", "session_id": identity}), flush=True)
    print(json.dumps({"type": "control_response", "response": {"subtype": "success", "request_id": request["request_id"], "response": {}}}), flush=True)
"#).unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
    let config = cwd.join("config");
    let project: String = cwd
        .to_str()
        .unwrap()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let history = config.join("projects").join(project);
    fs::create_dir_all(&history).unwrap();
    for id in [A, B] {
        fs::write(history.join(format!("{id}.jsonl")), "").unwrap();
    }
    let host = Arc::new(crate::host::state::HostLock::new(test_host()));
    let source = Session {
        native_continuity: Default::default(),
        owned_codex_listener: None,
        id: TerminalId::new(),
        launch: Launch {
            provider: Provider::Claude,
            recipe: LaunchRecipe {
                unset_environment: Default::default(),
                program: executable.to_string_lossy().into_owned(),
                arguments: vec!["--resume".into(), A.into()],
                working_directory: cwd.clone(),
                environment: BTreeMap::from([
                    (
                        "CLAUDE_CONFIG_DIR".into(),
                        config.to_string_lossy().into_owned(),
                    ),
                    ("LAUNCH_LOG".into(), log.to_string_lossy().into_owned()),
                    ("NEXT_SESSION".into(), B.into()),
                ]),
            },
            session_id: Some(A.into()),
        },
        proof: Some(
            host.lock()
                .unwrap_or_else(|p| p.into_inner())
                .proof()
                .clone(),
        ),
        terminal: None,
    };
    super::super::chat::process(
        &host,
        1,
        ClientRequest::ChatStart {
            session: source.clone(),
            source: None,
            terminate_source: false,
            execution_environment: Default::default(),
        },
    )
    .unwrap();
    let handle = host.lock().unwrap_or_else(|p| p.into_inner()).chats[&source.id]
        .handle
        .clone();
    let wait_for_identity = |expected: &str| {
        let deadline = Instant::now() + Duration::from_secs(5);
        while handle.confirmed_session_id().as_deref() != Some(expected) {
            assert!(
                Instant::now() < deadline,
                "provider failed to confirm {expected}"
            );
            thread::sleep(Duration::from_millis(10));
        }
    };
    wait_for_identity(A);
    // The hidden actor changes conversations without a presentation read
    // refreshing its persisted card's original launch or cached identity.
    handle.command(Command::Interrupt).unwrap();
    wait_for_identity(B);
    let store = Store::open(&cwd.join("checkpoint")).unwrap();
    let capability = host
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .shutdown_preflight(Instant::now())
        .unwrap()
        .capability;
    prepare_upgrade_with(&host, capability, || Ok(store)).unwrap();
    finish_upgrade(&host).unwrap();
    let store = Store::open(&cwd.join("checkpoint")).unwrap();
    let saved = store.load().unwrap().unwrap();
    saved.require_ready().unwrap();
    assert_eq!(saved.chats[0].source, source);
    assert_eq!(saved.chats[0].launch.session_id.as_deref(), Some(B));
    let empty_proc = cwd.join("proc");
    fs::create_dir(&empty_proc).unwrap();
    let replacement = test_host_with_incarnation("replacement-incarnation");
    let replacement = Arc::new(HostLock::new(replacement));
    restore_upgrade_from(&replacement, store, &empty_proc, None).unwrap();
    let restored = super::super::chat::resolve_upgrade(&replacement, &source).unwrap();
    assert_eq!(restored.launch.session_id.as_deref(), Some(B));
    assert_ne!(restored.proof, source.proof);
    let mut hidden = source.clone();
    hidden.launch.session_id = None;
    assert!(
        super::super::chat::resolve_upgrade(&replacement, &hidden).is_err(),
        "a known issued source cannot manufacture an unissued pre-ID alias"
    );
    let mut changed = source.clone();
    changed.launch.recipe.arguments = vec!["--resume".into(), B.into()];
    assert!(super::super::chat::resolve_upgrade(&replacement, &changed).is_err());
    changed = source.clone();
    changed.proof = restored.proof.clone();
    assert!(super::super::chat::resolve_upgrade(&replacement, &changed).is_err());
    changed = source.clone();
    changed.id = TerminalId::new();
    assert!(super::super::chat::resolve_upgrade(&replacement, &changed).is_err());
    changed = source.clone();
    changed.terminal = Some(Box::new(TerminalSession {
        owned_codex_listener: None,
        recovery_origin: None,
        predecessor: None,
        id: source.id,
        launch: source.launch.recipe.clone(),
        transcript_path: None,
        resume_hint: None,
        continuity: TerminalContinuity::RelaunchAvailable {
            recipe: source.launch.recipe.clone(),
            last_exit: None,
        },
    }));
    assert!(super::super::chat::resolve_upgrade(&replacement, &changed).is_err());
    restore_upgrade_from(
        &replacement,
        Store::open(&cwd.join("checkpoint")).unwrap(),
        &empty_proc,
        None,
    )
    .unwrap();
    // The durable P1 card stays closed at P2. A second full handoff must
    // carry its issued launch/proof, not just the most recent P2 source.
    let capability = replacement
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .shutdown_preflight(Instant::now())
        .unwrap()
        .capability;
    prepare_upgrade_with(&replacement, capability, || {
        Store::open(&cwd.join("checkpoint"))
    })
    .unwrap();
    finish_upgrade(&replacement).unwrap();
    let store = Store::open(&cwd.join("checkpoint")).unwrap();
    let second = store.load().unwrap().unwrap();
    assert_eq!(second.chats[0].previous_sources[0].source, source);
    let mut forged = second.clone();
    forged.chats[0].previous_sources[0].source.id = TerminalId::new();
    assert!(forged.validate().is_err());
    let mut forged = second.clone();
    forged.chats[0].previous_sources[0]
        .source
        .launch
        .recipe
        .environment
        .insert("UNRELATED".into(), "1".into());
    assert!(forged.validate().is_err());
    let mut forged = second.clone();
    forged.chats[0].previous_sources[0].source.proof = second.chats[0].source.proof.clone();
    assert!(forged.validate().is_err());
    let third = test_host_with_incarnation("third-incarnation");
    let replacement = Arc::new(HostLock::new(third));
    restore_upgrade_from(&replacement, store, &empty_proc, None).unwrap();
    let third = super::super::chat::resolve_upgrade(&replacement, &source).unwrap();
    assert_eq!(third.launch.session_id.as_deref(), Some(B));
    assert_ne!(third.proof, restored.proof);
    assert!(
        super::super::chat::resolve_upgrade(&replacement, &hidden).is_err(),
        "another daemon generation cannot legitimize an unissued pre-ID alias"
    );
    assert_eq!(
        super::super::chat::resolve_upgrade(&replacement, &restored).unwrap(),
        third
    );
    assert!(super::super::chat::resolve_upgrade(&replacement, &changed).is_err());
    let mut unrelated = source.clone();
    unrelated.proof = third.proof.clone();
    assert!(super::super::chat::resolve_upgrade(&replacement, &unrelated).is_err());
    unrelated = source.clone();
    unrelated.launch.recipe.arguments = vec!["--resume".into(), B.into()];
    assert!(super::super::chat::resolve_upgrade(&replacement, &unrelated).is_err());
    restore_upgrade_from(
        &replacement,
        Store::open(&cwd.join("checkpoint")).unwrap(),
        &empty_proc,
        None,
    )
    .unwrap();
    let launches: Vec<Vec<String>> = fs::read_to_string(log)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(
        launches.len(),
        3,
        "consumed checkpoint spawned another writer"
    );
    assert!(launches[0].contains(&format!("--resume={A}")));
    assert!(launches[1].contains(&format!("--resume={B}")));
    assert!(!launches[1].iter().any(|arg| arg.contains(A)));
    assert!(launches[2].contains(&format!("--resume={B}")));
    assert!(!launches[2].iter().any(|arg| arg.contains(A)));
    let restored_handle = replacement.lock().unwrap_or_else(|p| p.into_inner()).chats[&source.id]
        .handle
        .clone();
    restored_handle.close().unwrap();
}

pub(in crate::host) fn upgrade_fixture_in_fresh_process() -> bool {
    // Capability validation consults process-global retained child custody,
    // and any terminal test may legitimately hold some. These fixtures need
    // a fresh service. Preserve the caller's environment, but give each
    // child its own storage: update inspection reads the real data store.
    const SELECTOR_ENV: &str = "CAGENTS_UPGRADE_STATE_FIXTURE_SELECTOR";
    let current = std::thread::current();
    let selector = current.name().expect("named test thread");
    match std::env::var_os(SELECTOR_ENV) {
        Some(value) => {
            assert_eq!(value, selector, "unexpected fixture selector");
            true
        }
        None => {
            struct OriginalChild(std::process::Child);
            impl Drop for OriginalChild {
                fn drop(&mut self) {
                    let _ = self.0.kill();
                    let _ = self.0.wait();
                }
            }
            let mut output = tempfile::tempdir().unwrap();
            output.disable_cleanup(true);
            eprintln!("Upgrade fixture output: {}", output.path().display());
            let runtime = output.path().canonicalize().unwrap().join("run");
            crate::secure_file::prepare_private_directory(&runtime).unwrap();
            let stdout_path = output.path().join("stdout");
            let stderr_path = output.path().join("stderr");
            let mut child = OriginalChild(
                std::process::Command::new(std::env::current_exe().unwrap())
                    .args([
                        "--exact",
                        selector,
                        "--nocapture",
                        "--test-threads=1",
                        "--color=never",
                    ])
                    .env(SELECTOR_ENV, selector)
                    .env("XDG_DATA_HOME", output.path().join("data"))
                    .env("XDG_CONFIG_HOME", output.path().join("config"))
                    .env("XDG_RUNTIME_DIR", &runtime)
                    .stdout(fs::File::create(&stdout_path).unwrap())
                    .stderr(fs::File::create(&stderr_path).unwrap())
                    .spawn()
                    .unwrap(),
            );
            // Generous on purpose: the limit exists to report a hang, and a
            // loaded broad run must not turn slowness into a failure.
            let deadline = Instant::now() + Duration::from_secs(180);
            let status = loop {
                if let Some(status) = child.0.try_wait().unwrap() {
                    break status;
                }
                if Instant::now() >= deadline {
                    let _ = child.0.kill();
                    let _ = child.0.wait();
                    let retained = output.keep();
                    panic!(
                        "upgrade fixture timed out; retained at {}; stdout: {}; stderr: {}",
                        retained.display(),
                        fs::read_to_string(&stdout_path).unwrap_or_default(),
                        fs::read_to_string(&stderr_path).unwrap_or_default()
                    );
                }
                std::thread::sleep(Duration::from_millis(10));
            };
            let stdout = fs::read_to_string(stdout_path).unwrap();
            let stderr = fs::read_to_string(stderr_path).unwrap();
            let named = stdout
                .lines()
                .filter(|line| *line == format!("test {selector} ... ok"))
                .count()
                == 1;
            let summaries = stdout
                .lines()
                .filter(|line| line.starts_with("test result:"))
                .collect::<Vec<_>>();
            let qualified = summaries.len() == 1
                && summaries[0].starts_with("test result: ok. 1 passed; 0 failed; 0 ignored;");
            if !status.success() || !named || !qualified {
                let retained = output.keep();
                panic!(
                    "upgrade fixture child failed: {status}; retained at {}; stdout: {stdout}; stderr: {stderr}",
                    retained.display()
                );
            }
            output.close().unwrap();
            false
        }
    }
}

#[test]
pub(super) fn upgrade_commit_failure_preserves_live_control_authority() {
    if !upgrade_fixture_in_fresh_process() {
        return;
    }
    let directory = tempfile::tempdir().unwrap();
    let store = crate::upgrade::handoff::Store::open(directory.path()).unwrap();
    // An open directory descriptor is retained but cannot create files after
    // unlink. This fails the real persistence operation, not a mock echo.
    fs::remove_dir(directory.path()).unwrap();
    let host = Arc::new(crate::host::state::HostLock::new(test_host()));
    let capability = host
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .shutdown_preflight(Instant::now())
        .unwrap()
        .capability;
    let error = prepare_upgrade_with(&host, capability, || Ok(store)).unwrap_err();
    assert_ne!(error.message, RETAINED_CHILD_CLEANUP_MESSAGE);
    let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
    assert!(!state.is_shutting_down());
    assert!(state.reserve_spawn(TerminalId::new()).is_ok());
}

#[test]
pub(super) fn upgrade_rejects_stale_inventory_and_pending_reservations_without_shutdown() {
    if !upgrade_fixture_in_fresh_process() {
        return;
    }
    let mut state = test_host();
    let now = Instant::now();
    let stale = state.shutdown_preflight(now).unwrap().capability;
    let id = TerminalId::new();
    state.reserve_spawn(id).unwrap();
    assert!(state.validate_upgrade_capability(&stale, now).is_err());
    let pending = state.shutdown_preflight(now).unwrap().capability;
    assert!(state.validate_upgrade_capability(&pending, now).is_err());
    state.cancel_spawn(id);
    let current = state.shutdown_preflight(now).unwrap().capability;
    assert!(state.validate_upgrade_capability(&current, now).is_ok());
    assert!(!state.shutting_down);
}

#[test]
pub(super) fn restore_does_not_relaunch_beside_a_process_in_the_saved_unit() {
    if !upgrade_fixture_in_fresh_process() {
        return;
    }
    let directory = tempfile::tempdir().unwrap();
    let store = crate::upgrade::handoff::Store::open(directory.path()).unwrap();
    let host = Arc::new(crate::host::state::HostLock::new(test_host()));
    let capability = host
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .shutdown_preflight(Instant::now())
        .unwrap()
        .capability;
    prepare_upgrade_with(&host, capability, || Ok(store)).unwrap();
    finish_upgrade(&host).unwrap();

    let store = crate::upgrade::handoff::Store::open(directory.path()).unwrap();
    let mut checkpoint = store.load().unwrap().unwrap();
    let conversation = "aaaaaaaa-1111-4222-8333-444444444444";
    let mut recipe = crate::model::LaunchRecipe::shell(directory.path().to_owned()).unwrap();
    recipe.program = "claude".into();
    recipe.arguments = vec!["--resume".into(), conversation.into()];
    let mut launch = recipe.clone();
    launch.program = "/usr/bin/systemd-run".into();
    launch.arguments = vec![
        "--user".into(),
        "--scope".into(),
        "--unit=saved-unit".into(),
        "claude".into(),
        "--resume".into(),
        conversation.into(),
    ];
    checkpoint
        .terminals
        .push(crate::upgrade::handoff::Terminal {
            source: TerminalSession {
                owned_codex_listener: None,
                recovery_origin: None,
                predecessor: None,
                id: TerminalId::new(),
                launch,
                transcript_path: None,
                resume_hint: crate::model::agent_recovery::hint_from_launch(&recipe),
                continuity: TerminalContinuity::DetachedSameBoot {
                    host_session_id: checkpoint.proof.host_session_id.clone(),
                    boot_id: checkpoint.proof.boot_id.clone(),
                    host_incarnation: checkpoint.proof.host_incarnation.clone(),
                },
            },
            previous_sources: Vec::new(),
            relaunches: Vec::new(),
            recipe,
            options: TerminalSpawnOptions::default(),
        });
    store.save(&checkpoint).unwrap();

    // Its environment was cleared, so only the unit ties it to the terminal.
    let process = directory.path().join("proc/43");
    fs::create_dir_all(&process).unwrap();
    fs::write(process.join("stat"), "43 (sleep) S 1").unwrap();
    fs::write(process.join("cmdline"), b"sleep\x001800\0").unwrap();
    fs::write(process.join("environ"), b"").unwrap();
    fs::write(
        process.join("cgroup"),
        "0::/user.slice/user-1000.slice/user@1000.service/app.slice/saved-unit.scope\n",
    )
    .unwrap();

    let replacement = Arc::new(crate::host::state::HostLock::new(test_host()));
    let refusal = restore_upgrade_from(&replacement, store, &directory.path().join("proc"), None)
        .unwrap_err();
    assert!(
        refusal.contains("may still have a live process (43)"),
        "{refusal}"
    );
    assert!(refusal.contains("saved-unit"), "{refusal}");
    assert!(
        replacement
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .terminals
            .is_empty(),
        "nothing may relaunch beside it"
    );
}

#[test]
pub(super) fn committed_upgrade_fences_control_and_retains_checkpoint_until_close() {
    if !upgrade_fixture_in_fresh_process() {
        return;
    }
    let directory = tempfile::tempdir().unwrap();
    let store = crate::upgrade::handoff::Store::open(directory.path()).unwrap();
    let host = Arc::new(crate::host::state::HostLock::new(test_host()));
    let capability = host
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .shutdown_preflight(Instant::now())
        .unwrap()
        .capability;
    assert_eq!(
        prepare_upgrade_with(&host, capability, || Ok(store)).unwrap(),
        (0, 0)
    );
    assert!(
        host.lock()
            .unwrap_or_else(|p| p.into_inner())
            .reserve_spawn(TerminalId::new())
            .is_err()
    );
    let checkpoint = crate::upgrade::handoff::Store::open(directory.path())
        .unwrap()
        .load()
        .unwrap()
        .unwrap();
    assert!(checkpoint.require_ready().is_err());
    finish_upgrade(&host).unwrap();
    crate::upgrade::handoff::Store::open(directory.path())
        .unwrap()
        .load()
        .unwrap()
        .unwrap()
        .require_ready()
        .unwrap();
}

#[test]
pub(super) fn a_handoff_no_window_can_still_save_stops_refusing_the_upgrade() {
    if !upgrade_fixture_in_fresh_process() {
        return;
    }
    use crate::{
        agent::Provider,
        chat::{Launch, Session},
        model::LaunchRecipe,
        upgrade::handoff::Store,
    };
    let directory = tempfile::tempdir().unwrap();
    let host = Arc::new(crate::host::state::HostLock::new(test_host()));
    let recipe = LaunchRecipe {
        unset_environment: Default::default(),
        program: "omp".into(),
        arguments: Vec::new(),
        working_directory: directory.path().to_owned(),
        environment: BTreeMap::new(),
    };
    let source = Session {
        native_continuity: Default::default(),
        owned_codex_listener: None,
        id: TerminalId::new(),
        launch: Launch {
            provider: Provider::Omp,
            recipe: recipe.clone(),
            session_id: Some("12345678-1234-4234-8234-123456789abc".into()),
        },
        proof: Some(
            host.lock()
                .unwrap_or_else(|p| p.into_inner())
                .proof()
                .clone(),
        ),
        terminal: None,
    };
    let replacement = TerminalSession {
        owned_codex_listener: None,
        recovery_origin: None,
        predecessor: None,
        id: TerminalId::new(),
        launch: recipe.clone(),
        transcript_path: None,
        resume_hint: None,
        continuity: TerminalContinuity::RelaunchAvailable {
            recipe,
            last_exit: None,
        },
    };
    let chat = source.id;
    let retain = |deadline| {
        let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
        state.chat_handoffs.insert(
            chat,
            crate::host::chat::TerminalHandoff::new(
                source.clone(),
                replacement.clone(),
                1,
                BTreeMap::new(),
                deadline,
            ),
        );
        state.shutdown_preflight(Instant::now()).unwrap().capability
    };
    let live = retain(Instant::now() + Duration::from_secs(3600));
    let failure = prepare_upgrade_with(&host, live, || Store::open(directory.path())).unwrap_err();
    assert!(
        failure.message.contains("not durably acknowledged"),
        "expected the live receipt to block upgrade; actual error: {failure:?}"
    );
    // Nothing is destroyed by a refused upgrade: the window this receipt
    // belongs to can still replay its replacement afterwards.
    assert!(
        host.lock()
            .unwrap_or_else(|p| p.into_inner())
            .chat_handoffs
            .contains_key(&chat)
    );
    let expired = retain(Instant::now());
    assert_eq!(
        prepare_upgrade_with(&host, expired, || Store::open(directory.path())).unwrap(),
        (0, 0)
    );
    Store::open(directory.path())
        .unwrap()
        .load()
        .unwrap()
        .unwrap();
}

#[test]
pub(super) fn a_carried_terminal_keeps_its_process_across_an_upgrade() {
    if upgrade_fixture_in_fresh_process() {
        carried_terminal_across_upgrade(false);
    }
}

/// Recovery failure retains the live original instead of authorizing replay.
#[test]
pub(super) fn a_carried_terminal_survives_a_failed_recovery_after_adoption() {
    if upgrade_fixture_in_fresh_process() {
        carried_terminal_across_upgrade(true);
    }
}

// One process per case: an earlier case's original can still be settling in
// the process-wide custody that the next case's preparation refuses on.
fn carried_terminal_across_upgrade(invalid_successor: bool) {
    use crate::upgrade::handoff::{Phase, Store};
    use std::os::fd::AsRawFd;
    let mut directory = tempfile::tempdir().unwrap();
    // Preserve the exact detached socket/checkpoint namespace on success
    // and panic until the outer original-family guardian has settled it.
    directory.disable_cleanup(true);
    eprintln!(
        "Retained terminal carry fixture: {}",
        directory.path().display()
    );
    let holders = directory.path().join("holders");
    let empty_proc = directory.path().join("proc");
    fs::create_dir(&empty_proc).unwrap();
    let pid_file = directory.path().join("pid");
    let recipe = crate::model::LaunchRecipe {
        unset_environment: Default::default(),
        program: "/bin/sh".into(),
        arguments: vec![
            "-c".into(),
            format!("echo $$ > '{}'; exec sleep 1000", pid_file.display()),
        ],
        working_directory: directory.path().into(),
        environment: BTreeMap::new(),
    };
    let source = TerminalSession {
        owned_codex_listener: None,
        recovery_origin: None,
        predecessor: None,
        id: TerminalId::new(),
        launch: recipe.clone(),
        transcript_path: None,
        resume_hint: None,
        continuity: TerminalContinuity::RelaunchAvailable {
            recipe,
            last_exit: None,
        },
    };
    let host = Arc::new(crate::host::state::HostLock::new(test_host()));
    spawn_terminal(&host, 0, source.clone(), TerminalSpawnOptions::default()).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    let pid = loop {
        if let Some(pid) = fs::read_to_string(&pid_file)
            .ok()
            .and_then(|pid| pid.trim().parse::<i32>().ok())
        {
            break pid;
        }
        assert!(Instant::now() < deadline, "the shell never started");
        thread::sleep(Duration::from_millis(10));
    };
    let original_root = crate::terminal::holder::Process::read(pid).unwrap();
    let old = host.lock().unwrap().terminals[&source.id].handle.clone();
    let carried_source = host
        .lock()
        .unwrap()
        .descriptor(0, source.id)
        .unwrap()
        .session;
    let capability = host
        .lock()
        .unwrap()
        .shutdown_preflight(Instant::now())
        .unwrap()
        .capability;
    // Its job keeps running, so no relaunch has to be accepted for it.
    assert_eq!(
        prepare_upgrade_with_policy(&host, capability, false, Some(&holders), || {
            Store::open(directory.path())
        })
        .unwrap(),
        (1, 0)
    );
    finish_upgrade(&host).unwrap();
    assert!(old.is_completed());
    let saved = Store::open(directory.path())
        .unwrap()
        .load()
        .unwrap()
        .unwrap();
    assert!(saved.terminals.is_empty());
    assert_eq!(saved.carried[0].source, carried_source);
    // An anonymous carry listens until a successor adopts it. Verify
    // the real handoff endpoint before either restore consumes it.
    assert!(holders.join(source.id.to_string()).exists());

    if invalid_successor {
        // Preserve the malformed successor as the real post-adoption failure.
        let store = Store::open(directory.path()).unwrap();
        let mut failing = store.load().unwrap().unwrap();
        let recipe = crate::model::LaunchRecipe::shell(directory.path().to_owned()).unwrap();
        failing.terminals.push(crate::upgrade::handoff::Terminal {
            source: TerminalSession {
                owned_codex_listener: None,
                recovery_origin: None,
                predecessor: None,
                id: TerminalId::new(),
                launch: recipe.clone(),
                transcript_path: None,
                resume_hint: None,
                continuity: TerminalContinuity::DetachedSameBoot {
                    host_session_id: failing.proof.host_session_id.clone(),
                    boot_id: failing.proof.boot_id.clone(),
                    host_incarnation: failing.proof.host_incarnation.clone(),
                },
            },
            previous_sources: Vec::new(),
            relaunches: Vec::new(),
            recipe,
            // No terminal can start at this size.
            options: TerminalSpawnOptions {
                columns: 0,
                ..TerminalSpawnOptions::default()
            },
        });
        store.save(&failing).unwrap();
        let mut expected_retained: serde_json::Value =
            serde_json::from_slice(&fs::read(&store.path).unwrap()).unwrap();
        let failed = Arc::new(crate::host::state::HostLock::new(
            test_host_with_incarnation("failed"),
        ));
        failed.lock().unwrap().install_model(
            crate::host::model::Registry::at(&directory.path().join("failed-model.sqlite3"))
                .unwrap(),
        );
        let failure =
            restore_upgrade_from(&failed, store, &empty_proc, Some(&holders)).unwrap_err();
        assert!(
            failure.starts_with("Upgrade restore retained original ownership:"),
            "{failure}"
        );
        assert!(
            failure.contains("terminal dimensions or scrollback are outside supported bounds"),
            "{failure}"
        );
        let handle = {
            let state = failed.lock().unwrap();
            assert_eq!(state.terminals.len(), 1);
            assert_eq!(state.startup_recovery(), Some(failure.as_str()));
            assert_eq!(
                state.startup_state(),
                StartupState::Retained {
                    detail: failure.clone()
                }
            );
            let descriptor = state.descriptor(0, source.id).unwrap();
            assert_eq!(descriptor.upgrade_sources, vec![carried_source]);
            assert_eq!(descriptor.session.launch, source.launch);
            state.terminals[&source.id].handle.clone()
        };
        assert!(!handle.is_completed());
        let root_pidfd = handle.original_root_pidfd_for_test().unwrap();
        let fdinfo =
            fs::read_to_string(format!("/proc/self/fdinfo/{}", root_pidfd.as_raw_fd())).unwrap();
        let pinned_pid = fdinfo
            .lines()
            .find_map(|line| line.strip_prefix("Pid:"))
            .unwrap()
            .trim()
            .parse::<i32>()
            .unwrap();
        assert_eq!(pinned_pid, original_root.pid);
        let mut readiness = libc::pollfd {
            fd: root_pidfd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: the cloned original pidfd stays owned across poll.
        assert_eq!(unsafe { libc::poll(&mut readiness, 1, 0) }, 0);
        assert!(original_root.is_current());
        // Anonymous adoption intentionally unlinks its old listener;
        // the retained current worker is the reachable Close surface.
        handle.observe_agent().unwrap();
        let retained_store = Store::open(directory.path()).unwrap();
        assert_eq!(
            retained_store.load().unwrap().unwrap().phase,
            Phase::Restoring
        );
        expected_retained["phase"] = serde_json::to_value(Phase::Restoring).unwrap();
        let actual_retained: serde_json::Value =
            serde_json::from_slice(&fs::read(&retained_store.path).unwrap()).unwrap();
        assert_eq!(actual_retained, expected_retained);
        assert_eq!(
            fs::read_to_string(format!("/proc/{pid}/comm"))
                .unwrap()
                .trim(),
            "sleep"
        );
        assert_eq!(
            handle.close_checked(true),
            crate::terminal_ipc::TerminalCloseResult::Closed
        );
        assert!(!std::path::Path::new(&format!("/proc/{pid}")).exists());
        return;
    }

    let replacement = Arc::new(crate::host::state::HostLock::new(
        test_host_with_incarnation("second"),
    ));
    replacement.lock().unwrap().install_model(
        crate::host::model::Registry::at(&directory.path().join("replacement-model.sqlite3"))
            .unwrap(),
    );
    restore_upgrade_from(
        &replacement,
        Store::open(directory.path()).unwrap(),
        &empty_proc,
        Some(&holders),
    )
    .unwrap();
    assert!(
        Store::open(directory.path())
            .unwrap()
            .load()
            .unwrap()
            .is_none()
    );
    let descriptor = replacement
        .lock()
        .unwrap()
        .descriptor(0, source.id)
        .unwrap();
    assert_eq!(descriptor.upgrade_sources, vec![carried_source]);
    assert_eq!(descriptor.session.launch, source.launch);
    assert_eq!(
        fs::read_to_string(format!("/proc/{pid}/comm"))
            .unwrap()
            .trim(),
        "sleep",
        "the successor adopted the same process rather than starting another"
    );
    let handle = replacement.lock().unwrap().terminals[&source.id]
        .handle
        .clone();
    assert!(!handle.is_completed());
    let root_pidfd = handle.original_root_pidfd_for_test().unwrap();
    let fdinfo =
        fs::read_to_string(format!("/proc/self/fdinfo/{}", root_pidfd.as_raw_fd())).unwrap();
    let pinned_pid = fdinfo
        .lines()
        .find_map(|line| line.strip_prefix("Pid:"))
        .unwrap()
        .trim()
        .parse::<i32>()
        .unwrap();
    assert_eq!(pinned_pid, original_root.pid);
    let mut readiness = libc::pollfd {
        fd: root_pidfd.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: the cloned original pidfd stays owned across poll.
    assert_eq!(unsafe { libc::poll(&mut readiness, 1, 0) }, 0);
    assert!(original_root.is_current());
    assert_eq!(
        handle.close_checked(true),
        crate::terminal_ipc::TerminalCloseResult::Closed
    );
    assert!(!std::path::Path::new(&format!("/proc/{pid}")).exists());
}

#[test]
fn a_carried_terminal_whose_holder_is_gone_restarts_from_its_recipe() {
    use crate::upgrade::handoff::{Carried, Checkpoint, Phase, Store};
    let directory = tempfile::tempdir().unwrap();
    let empty_proc = directory.path().join("proc");
    fs::create_dir(&empty_proc).unwrap();
    let old = test_host().proof().clone();
    let source = TerminalSession {
        owned_codex_listener: None,
        recovery_origin: None,
        predecessor: None,
        id: TerminalId::new(),
        launch: crate::model::LaunchRecipe::shell(directory.path().to_owned()).unwrap(),
        transcript_path: None,
        resume_hint: None,
        continuity: TerminalContinuity::DetachedSameBoot {
            host_session_id: old.host_session_id.clone(),
            boot_id: old.boot_id.clone(),
            host_incarnation: old.host_incarnation.clone(),
        },
    };
    let mut checkpoint = Checkpoint::new(
        old,
        Vec::new(),
        vec![Carried {
            source: source.clone(),
            previous_sources: Vec::new(),
            relaunches: Vec::new(),
            options: TerminalSpawnOptions::default(),
        }],
        Vec::new(),
    );
    checkpoint.phase = Phase::Ready;
    let store = Store::open(directory.path()).unwrap();
    store.save(&checkpoint).unwrap();
    let host = Arc::new(crate::host::state::HostLock::new(
        test_host_with_incarnation("second"),
    ));
    restore_upgrade_from(
        &host,
        store,
        &empty_proc,
        Some(&directory.path().join("holders")),
    )
    .unwrap();
    let descriptor = host.lock().unwrap().descriptor(0, source.id).unwrap();
    assert_eq!(descriptor.upgrade_sources, vec![source]);
    let handle = host.lock().unwrap().terminals[&descriptor.session.id]
        .handle
        .clone();
    assert!(!handle.is_completed());
    assert_eq!(
        handle.close_checked(true),
        crate::terminal_ipc::TerminalCloseResult::Closed
    );
}

#[test]
pub(super) fn restored_shell_links_exact_old_source_and_does_not_replay_consumed_checkpoint() {
    if !upgrade_fixture_in_fresh_process() {
        return;
    }
    use crate::upgrade::handoff::{Checkpoint, Phase, Store, Terminal};
    let directory = tempfile::tempdir().unwrap();
    let empty_proc = directory.path().join("proc");
    fs::create_dir(&empty_proc).unwrap();
    let old = test_host().proof().clone();
    let recipe = crate::model::LaunchRecipe::shell(directory.path().to_owned()).unwrap();
    let source = TerminalSession {
        owned_codex_listener: None,
        recovery_origin: None,
        predecessor: None,
        id: TerminalId::new(),
        launch: recipe.clone(),
        transcript_path: Some(directory.path().join("shell.log")),
        resume_hint: None,
        continuity: TerminalContinuity::DetachedSameBoot {
            host_session_id: old.host_session_id.clone(),
            boot_id: old.boot_id.clone(),
            host_incarnation: old.host_incarnation.clone(),
        },
    };
    let mut checkpoint = Checkpoint::new(
        old,
        vec![Terminal {
            source: source.clone(),
            previous_sources: Vec::new(),
            relaunches: Vec::new(),
            recipe,
            options: TerminalSpawnOptions::default(),
        }],
        Vec::new(),
        Vec::new(),
    );
    checkpoint.phase = Phase::Ready;
    let store = Store::open(directory.path()).unwrap();
    store.save(&checkpoint).unwrap();
    let replacement = test_host_with_incarnation("replacement-incarnation");
    let expected = (*replacement.proof).clone();
    let host = Arc::new(HostLock::new(replacement));
    restore_upgrade_from(&host, store, &empty_proc, None).unwrap();
    let descriptor = host
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .descriptor(0, source.id)
        .unwrap();
    assert_eq!(descriptor.upgrade_sources, vec![source.clone()]);
    assert_eq!(descriptor.session.continuity.live_proof(), Some(expected));
    assert_eq!(descriptor.session.transcript_path, source.transcript_path);
    restore_upgrade_from(
        &host,
        Store::open(directory.path()).unwrap(),
        &empty_proc,
        None,
    )
    .unwrap();
    // Restore enqueues startup visibility controls; a descriptor and an
    // empty checkpoint read do not acknowledge their worker-side completion.
    // This existing synchronous observation follows them on the same FIFO.
    let restored = host.lock().unwrap().terminals[&source.id].handle.clone();
    restored.observe_agent().unwrap();
    // A separate idle probe can resume the shell before host preparation
    // observes it again. Retry only this exact fixture's active-shell
    // refusal; success keeps the actual preparation snapshot frozen.
    let expected_active = upgrade_error(upgrade_blockers_message(
        &[describe_upgrade_blocker(&descriptor.session)],
        false,
    ));
    let prepare_started = Instant::now();
    let mut prepare_attempts = 0;
    loop {
        assert!(
            // This waits on the passwd shell's own startup in a fresh HOME.
            prepare_started.elapsed() < Duration::from_secs(60),
            "restored shell never became eligible after {prepare_attempts} preparation attempts"
        );
        prepare_attempts += 1;
        let capability = {
            let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
            assert_eq!(state.terminals.len(), 1);
            assert!(state.chats.is_empty());
            assert!(state.terminals[&source.id].handle.same_worker(&restored));
            state.shutdown_preflight(Instant::now()).unwrap().capability
        };
        match prepare_upgrade_with(&host, capability, || Store::open(directory.path())) {
            Ok(counts) => {
                assert_eq!(counts, (1, 0));
                break;
            }
            Err(error) if error == expected_active => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => panic!(
                "unexpected restored-shell preparation failure after {prepare_attempts} attempts: {error:?}"
            ),
        }
    }
    finish_upgrade(&host).unwrap();
    let store = Store::open(directory.path()).unwrap();
    let second = store.load().unwrap().unwrap();
    let mut forged = second.clone();
    forged.terminals[0].previous_sources[0].id = TerminalId::new();
    assert!(forged.validate().is_err());
    let mut forged = second.clone();
    forged.terminals[0].previous_sources[0]
        .launch
        .environment
        .insert("UNRELATED".into(), "1".into());
    assert!(forged.validate().is_err());
    let mut forged = second.clone();
    forged.terminals[0].previous_sources[0].continuity =
        second.terminals[0].source.continuity.clone();
    assert!(forged.validate().is_err());
    let third = test_host_with_incarnation("third-incarnation");
    let host = Arc::new(HostLock::new(third));
    restore_upgrade_from(&host, store, &empty_proc, None).unwrap();
    let third = host
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .descriptor(0, source.id)
        .unwrap();
    assert_eq!(
        third.upgrade_sources,
        vec![source.clone(), second.terminals[0].source.clone()]
    );
    assert_ne!(
        third.session.continuity.live_proof(),
        descriptor.session.continuity.live_proof()
    );
    let handle = {
        let state = host.lock().unwrap_or_else(|p| p.into_inner());
        assert_eq!(state.terminals.len(), 1);
        state.terminals[&source.id].handle.clone()
    };
    assert_eq!(
        handle.close_checked(true),
        crate::terminal_ipc::TerminalCloseResult::Closed
    );
}

#[test]
fn hidden_output_is_per_subscriber_and_rearms_without_revealing() {
    let shared = Arc::new(crate::host::state::HostLock::new(test_host()));
    let recipe = crate::model::LaunchRecipe {
        unset_environment: Default::default(),
        program: "/bin/cat".into(),
        arguments: Vec::new(),
        working_directory: std::path::PathBuf::from("/tmp"),
        environment: BTreeMap::new(),
    };
    let session = TerminalSession {
        owned_codex_listener: None,
        recovery_origin: None,
        predecessor: None,
        id: TerminalId::new(),
        launch: recipe.clone(),
        transcript_path: None,
        resume_hint: None,
        continuity: TerminalContinuity::RelaunchAvailable {
            recipe,
            last_exit: None,
        },
    };
    let id = session.id;
    let handle = TerminalHandle::spawn(
        session.clone(),
        TerminalConfig {
            initially_visible: false,
            ..TerminalConfig::default()
        },
        |_| {},
    )
    .unwrap();
    let (first, second) = {
        let mut state = shared.lock().unwrap_or_else(|error| error.into_inner());
        let deliveries = (state.register_client(1), state.register_client(2));
        state.reserve_spawn(id).unwrap();
        state
            .finish_spawn(1, session, handle.clone(), false)
            .unwrap();
        state.attach(2, id).unwrap();
        state
            .set_visibility(2, id, TerminalVisibility::Visible)
            .unwrap();
        deliveries
    };
    let weak = Arc::downgrade(&shared);
    broadcast_output(&weak, id, 1);
    broadcast_output(&weak, id, 2);
    {
        let mut state = shared.lock().unwrap_or_else(|error| error.into_inner());
        state
            .set_visibility(1, id, TerminalVisibility::Hibernated)
            .unwrap();
    }
    broadcast_output(&weak, id, 3); // Hibernation is not a new hidden epoch.
    {
        let mut state = shared.lock().unwrap_or_else(|error| error.into_inner());
        state.acknowledge_output(1, id, 1).unwrap();
        assert_eq!(
            state.subscriptions.by_client[&1][&id],
            TerminalVisibility::Hibernated
        );
        state
            .set_visibility(2, id, TerminalVisibility::Hidden)
            .unwrap();
    }
    broadcast_output(&weak, TerminalId::new(), 4); // A stale identity has no subscribers.
    broadcast_output(&weak, id, 4);
    broadcast_output(&weak, id, 5);
    {
        let mut state = shared.lock().unwrap_or_else(|error| error.into_inner());
        // A late acknowledgement from an earlier scope cannot erase a newer one.
        state.acknowledge_output(1, id, 0).unwrap();
        state.acknowledge_output(1, id, 1).unwrap();
    }
    broadcast_output(&weak, id, 6);
    for (delivery, expected) in [(first, vec![0, 1]), (second, vec![0])] {
        delivery.close_after_drain();
        let mut epochs = Vec::new();
        while let Some(envelope) = delivery.pop() {
            if let ServerMessage::TerminalObserved { terminal_id, epoch } = envelope.message {
                assert_eq!(terminal_id, id);
                epochs.push(epoch);
            }
        }
        assert_eq!(epochs, expected);
    }
    handle.shutdown().unwrap();
}

#[test]
fn disconnected_ui_reattaches_the_same_shell_without_relaunching() {
    let directory = tempfile::tempdir().unwrap();
    let starts = directory.path().join("starts");
    let recipe = crate::model::LaunchRecipe {
        unset_environment: Default::default(),
        program: "/bin/sh".into(),
        arguments: vec![
            "-c".into(),
            "printf '%s\\n' \"$$\" >> \"$1\"; while IFS= read -r line; do printf '\\033]2;reply:%s:%s\\007' \"$$\" \"$line\"; done".into(),
            "continuity-test".into(),
            starts.to_str().unwrap().into(),
        ],
        working_directory: directory.path().into(),
        environment: BTreeMap::new(),
    };
    let session = TerminalSession {
        owned_codex_listener: None,
        recovery_origin: None,
        predecessor: None,
        id: TerminalId::new(),
        launch: recipe.clone(),
        transcript_path: None,
        resume_hint: None,
        continuity: TerminalContinuity::RelaunchAvailable {
            recipe,
            last_exit: None,
        },
    };
    let id = session.id;
    let mut host = test_host();
    host.register_client(1);
    host.reserve_spawn(id).unwrap();
    let handle = TerminalHandle::spawn(session.clone(), TerminalConfig::default(), |_| {}).unwrap();
    let events = handle.events();
    host.finish_spawn(1, session, handle.clone(), false)
        .unwrap();
    let reply = |value: &str| {
        handle.input(format!("{value}\n").into_bytes()).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            let event = events
                .recv_timeout(deadline.saturating_duration_since(std::time::Instant::now()))
                .unwrap();
            if let TerminalEvent::TitleChanged(title) = event
                && title.ends_with(&format!(":{value}"))
            {
                break title;
            }
        }
    };
    let before = reply("before");
    let proof = host
        .descriptor(1, id)
        .unwrap()
        .session
        .continuity
        .live_proof()
        .unwrap();
    drop(host.unregister_client(1));
    host.register_client(2);
    let inventory = host.list(2);
    assert_eq!(
        inventory
            .iter()
            .map(|terminal| terminal.session.id)
            .collect::<Vec<_>>(),
        [id]
    );
    let reattached = host.attach(2, id).unwrap();
    assert!(reattached.session.continuity.can_resume_live(&proof));
    assert!(
        host.reserve_spawn(id).is_err(),
        "reopening cannot duplicate a live terminal"
    );
    let after = reply("after");
    assert_eq!(
        before.trim_end_matches(":before"),
        after.trim_end_matches(":after")
    );
    let pids = fs::read_to_string(starts).unwrap();
    assert_eq!(
        pids.lines().count(),
        1,
        "the launch recipe ran exactly once"
    );
    assert_eq!(before, format!("reply:{}:before", pids.trim()));
    handle.shutdown().unwrap();
}

#[test]
fn redraw_delivery_excludes_hidden_and_hibernated_subscribers() {
    let shared = Arc::new(crate::host::state::HostLock::new(test_host()));
    let terminal_id = TerminalId::new();
    let stream = crate::terminal::RenderStreamFixture::new(|_| {});
    let deliveries = {
        let mut state = shared.lock().unwrap_or_else(|error| error.into_inner());
        let deliveries = [
            TerminalVisibility::Visible,
            TerminalVisibility::Hidden,
            TerminalVisibility::Hibernated,
        ]
        .into_iter()
        .enumerate()
        .map(|(index, visibility)| {
            let client_id = index as u64 + 1;
            let delivery = state.register_client(client_id);
            state
                .subscriptions
                .attach(client_id, terminal_id, visibility);
            delivery
        })
        .collect::<Vec<_>>();
        state.reserve_spawn(terminal_id).unwrap();
        state
            .finish_spawn(
                1,
                redraw_identity_tests::session(terminal_id),
                stream.handle.clone(),
                true,
            )
            .unwrap();
        deliveries
    };

    broadcast_redraw(&Arc::downgrade(&shared), terminal_id, 7, &stream.handle);
    for delivery in &deliveries {
        delivery.close_after_drain();
    }
    // The spawning client first learns which worker it started.
    assert!(matches!(
        deliveries[0].pop().unwrap().message,
        ServerMessage::TerminalWorker { terminal_id: id, instance }
            if id == terminal_id && instance == stream.handle.execution_instance()
    ));
    assert!(matches!(
        deliveries[0].pop().unwrap().message,
        ServerMessage::Snapshot { snapshot, .. } if snapshot.generation == 7
    ));
    assert!(deliveries[0].pop().is_none());
    assert!(deliveries[1].pop().is_none());
    assert!(deliveries[2].pop().is_none());
}

/// A visible subscriber whose delta base the daemon already recorded must
/// receive the repaint itself. Sending `Redraw` made the client request the
/// very base the daemon had just diffed against, which cost a socket round
/// trip on every frame.
#[test]
fn a_recorded_base_turns_a_redraw_edge_into_a_pushed_repaint() {
    let directory = tempfile::tempdir().unwrap();
    let recipe = crate::model::LaunchRecipe::shell(directory.path().to_owned()).unwrap();
    let session = TerminalSession {
        owned_codex_listener: None,
        recovery_origin: None,
        predecessor: None,
        id: TerminalId::new(),
        launch: recipe.clone(),
        transcript_path: None,
        resume_hint: None,
        continuity: TerminalContinuity::RelaunchAvailable {
            recipe,
            last_exit: None,
        },
    };
    let id = session.id;
    let shared = Arc::new(crate::host::state::HostLock::new(test_host()));
    let (delivery, handle, base) = {
        let mut state = shared.lock().unwrap_or_else(|error| error.into_inner());
        let delivery = state.register_client(1);
        state.reserve_spawn(id).unwrap();
        let handle = TerminalHandle::spawn(
            session.clone(),
            TerminalConfig {
                initially_visible: true,
                ..TerminalConfig::default()
            },
            |_| {},
        )
        .unwrap();
        state
            .finish_spawn(1, session, handle.clone(), true)
            .unwrap();
        // Stand in for the request path that delivered this capture.
        let base = handle.snapshot();
        state
            .subscriptions
            .record_sent(1, id, Arc::clone(&base), true);
        (delivery, handle, base)
    };
    let deadline = Instant::now() + Duration::from_secs(5);
    while handle.generation() <= base.generation {
        assert!(Instant::now() < deadline, "the terminal never repainted");
        // The worker withholds the next capture until the presented
        // generation is acknowledged; the UI does this after drawing.
        handle.acknowledge_redraw(handle.generation());
        thread::sleep(Duration::from_millis(10));
    }

    broadcast_redraw(&Arc::downgrade(&shared), id, handle.generation(), &handle);
    delivery.close_after_drain();
    let mut pushed = None;
    while let Some(message) = delivery.pop() {
        if let ServerMessage::Snapshot {
            terminal_id,
            snapshot,
        } = message.message
            && terminal_id == id
        {
            pushed = Some(snapshot);
        }
    }
    let pushed = pushed.expect("the redraw edge did not carry the repaint");
    assert!(pushed.generation > base.generation);
    // Whatever encoding was chosen must resolve against the recorded base,
    // or the saved round trip would cost a full repaint instead.
    pushed.apply(Some(&base)).unwrap();
    handle.shutdown().unwrap();
}

fn empty_stop() -> ShutdownOperation {
    ShutdownOperation::Stop {
        require_empty: true,
        expected_generation: None,
    }
}

/// A plain `cagents daemon stop` that native work refuses must say what it
/// actually wants — a confirmed stop — instead of reporting the
/// `require_empty` demand the caller never made.
#[test]
fn a_stop_refused_for_native_work_names_confirmation_unless_emptiness_was_demanded() {
    let directory = tempfile::tempdir().unwrap();
    let recipe = crate::model::LaunchRecipe::shell(directory.path().to_owned()).unwrap();
    for (require_empty, expected) in [
        (
            false,
            ShutdownOutcome::ConfirmationRequired { current_count: 1 },
        ),
        (true, ShutdownOutcome::EmptyRequired { current_count: 1 }),
    ] {
        let mut state = test_host();
        let id = TerminalId::new();
        state.chat_spawning.insert(
            id,
            crate::chat::Session {
                id,
                launch: crate::chat::Launch {
                    provider: crate::agent::Provider::Claude,
                    recipe: recipe.clone(),
                    session_id: None,
                },
                native_continuity: Default::default(),
                owned_codex_listener: None,
                proof: Some(state.proof().clone()),
                terminal: None,
            },
        );
        assert_eq!(
            state.shutdown(
                ShutdownOperation::Stop {
                    require_empty,
                    expected_generation: None,
                },
                Instant::now()
            ),
            expected
        );
        assert!(!state.shutting_down);
    }
}

#[test]
fn completed_terminal_history_routes_queries_after_the_event_forwarder_finishes() {
    let shared = Arc::new(crate::host::state::HostLock::new(test_host()));
    let (first, second) = {
        let mut state = shared.lock().unwrap();
        (state.register_client(1), state.register_client(2))
    };
    let recipe = crate::model::LaunchRecipe {
        unset_environment: Default::default(),
        program: "/bin/sh".into(),
        arguments: vec!["-c".into(), "printf 'retained hit\\r\\n'; exit 0".into()],
        working_directory: "/".into(),
        environment: BTreeMap::new(),
    };
    let session = TerminalSession {
        owned_codex_listener: None,
        recovery_origin: None,
        predecessor: None,
        id: TerminalId::new(),
        launch: recipe.clone(),
        transcript_path: None,
        resume_hint: None,
        continuity: TerminalContinuity::RelaunchAvailable {
            recipe,
            last_exit: None,
        },
    };
    let id = session.id;
    spawn_terminal(&shared, 1, session, TerminalSpawnOptions::default()).unwrap();
    let handle = {
        let mut state = shared.lock().unwrap();
        state.attach(2, id).unwrap();
        state.terminals[&id].handle.clone()
    };
    let deadline = Instant::now() + Duration::from_secs(5);
    while !handle.is_completed()
        || !shared.lock().unwrap().terminals[&id]
            .events
            .iter()
            .any(|(_, event)| matches!(event, WireTerminalEvent::Completed { .. }))
    {
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(5));
    }
    {
        let mut state = shared.lock().unwrap();
        assert!(state.surface_handle(1, id).is_err());
        assert!(state.history_handle(1, id).is_ok());
        assert!(state.history_handle(3, id).is_err());
        state
            .set_visibility(1, id, TerminalVisibility::Hidden)
            .unwrap();
        state
            .set_visibility(1, id, TerminalVisibility::Visible)
            .unwrap();
    }
    handle.search_for(Some(2), 9, "hit".into(), true).unwrap();
    crate::terminal::flush_archived_notices();
    for (delivery, expected) in [(first, 0), (second, 1)] {
        delivery.close_after_drain();
        let mut results = 0;
        while let Some(envelope) = delivery.pop() {
            if let ServerMessage::Event {
                event:
                    WireTerminalEvent::SearchResults {
                        request: 9,
                        matches,
                        ..
                    },
                ..
            } = envelope.message
            {
                assert_eq!(matches.len(), 1);
                results += 1;
            }
        }
        assert_eq!(results, expected);
    }
    assert!(
        !shared
            .lock()
            .unwrap()
            .replay_events(id, 0)
            .unwrap()
            .iter()
            .any(|event| matches!(event.1, WireTerminalEvent::SearchResults { .. }))
    );
    handle.shutdown().unwrap();
}

#[test]
fn terminal_search_results_are_delivered_only_to_the_requesting_window_and_never_replayed() {
    let shared = Arc::new(crate::host::state::HostLock::new(test_host()));
    let recipe = crate::model::LaunchRecipe {
        unset_environment: Default::default(),
        program: "/bin/cat".into(),
        arguments: Vec::new(),
        working_directory: "/".into(),
        environment: BTreeMap::new(),
    };
    let session = TerminalSession {
        recovery_origin: None,
        predecessor: None,
        owned_codex_listener: None,
        id: TerminalId::new(),
        launch: recipe.clone(),
        transcript_path: None,
        resume_hint: None,
        continuity: TerminalContinuity::RelaunchAvailable {
            recipe,
            last_exit: None,
        },
    };
    let id = session.id;
    let handle = TerminalHandle::spawn(session.clone(), TerminalConfig::default(), |_| {}).unwrap();
    let (first, second) = {
        let mut state = shared.lock().unwrap();
        let deliveries = (state.register_client(1), state.register_client(2));
        state
            .finish_spawn(1, session, handle.clone(), false)
            .unwrap();
        state.attach(2, id).unwrap();
        deliveries
    };
    record_event(
        &Arc::downgrade(&shared),
        id,
        TerminalEvent::SearchResults {
            owner: Some(1),
            request: 1,
            query: "hit".into(),
            matches: Vec::new(),
            truncated: false,
        },
    );
    assert!(
        shared
            .lock()
            .unwrap()
            .replay_events(id, 0)
            .unwrap()
            .is_empty()
    );
    for (delivery, expected) in [(first, 1), (second, 0)] {
        delivery.close_after_drain();
        let mut count = 0;
        while let Some(envelope) = delivery.pop() {
            if matches!(
                envelope.message,
                ServerMessage::Event {
                    event: WireTerminalEvent::SearchResults { request: 1, .. },
                    ..
                }
            ) {
                count += 1;
            }
        }
        assert_eq!(count, expected);
    }
    handle.shutdown().unwrap();
}

#[test]
fn terminal_descriptor_retains_only_native_display_title_after_event_eviction() {
    let shared = Arc::new(crate::host::state::HostLock::new(test_host()));
    let recipe = crate::model::LaunchRecipe {
        unset_environment: Default::default(),
        program: "/bin/cat".into(),
        arguments: vec![],
        working_directory: "/".into(),
        environment: BTreeMap::new(),
    };
    let id = TerminalId::new();
    let session = TerminalSession {
        id,
        recovery_origin: None,
        predecessor: None,
        owned_codex_listener: None,
        launch: recipe.clone(),
        transcript_path: None,
        resume_hint: None,
        continuity: TerminalContinuity::RelaunchAvailable {
            recipe,
            last_exit: None,
        },
    };
    let handle = TerminalHandle::spawn(session.clone(), TerminalConfig::default(), |_| {}).unwrap();
    shared
        .lock()
        .unwrap()
        .finish_spawn(1, session, handle.clone(), false)
        .unwrap();
    let weak = Arc::downgrade(&shared);
    let observations = (|| -> Result<_, ProtocolError> {
        let initial = shared.lock().unwrap().descriptor(1, id)?.title;
        record_event(
            &weak,
            id,
            TerminalEvent::ConversationTitleChanged("Provider conversation".into()),
        );
        let provider_only = shared.lock().unwrap().descriptor(1, id)?.title;
        record_event_from(
            &weak,
            id,
            TerminalEvent::TitleChanged("界".repeat(300)),
            Some(&handle),
        );
        for _ in 0..EVENT_REPLAY_CAPACITY {
            record_event(&weak, id, TerminalEvent::Bell);
        }
        let (retained, evicted) = {
            let state = shared.lock().unwrap();
            (
                state
                    .list(1)
                    .into_iter()
                    .find(|row| row.session.id == id)
                    .unwrap()
                    .title,
                state
                    .replay_events(id, 0)?
                    .iter()
                    .all(|(_, event)| !matches!(event, WireTerminalEvent::TitleChanged { .. })),
            )
        };
        record_event_from(
            &weak,
            id,
            TerminalEvent::TitleChanged(String::new()),
            Some(&handle),
        );
        let cleared = shared.lock().unwrap().descriptor(1, id)?.title;
        Ok((initial, provider_only, retained, evicted, cleared))
    })();
    handle.shutdown().unwrap();
    let (initial, provider_only, retained, evicted, cleared) = observations.unwrap();
    assert_eq!(initial, None);
    assert_eq!(
        provider_only, None,
        "provider conversation titles are a different observation"
    );
    assert_eq!(retained, Some("界".repeat(256)));
    assert!(evicted, "the title must survive actual transition eviction");
    assert_eq!(cleared, Some(String::new()));
}

#[test]
fn terminal_descriptor_title_comes_from_an_actual_pty_osc_event() {
    let shared = Arc::new(crate::host::state::HostLock::new(test_host()));
    let recipe = crate::model::LaunchRecipe {
        unset_environment: Default::default(),
        program: "/bin/sh".into(),
        arguments: vec![
            "-c".into(),
            "printf '\\033]2;Native terminal title\\007'; exec /bin/cat".into(),
        ],
        working_directory: "/".into(),
        environment: BTreeMap::new(),
    };
    let id = TerminalId::new();
    let session = TerminalSession {
        id,
        recovery_origin: None,
        predecessor: None,
        owned_codex_listener: None,
        launch: recipe.clone(),
        transcript_path: None,
        resume_hint: None,
        continuity: TerminalContinuity::RelaunchAvailable {
            recipe,
            last_exit: None,
        },
    };
    let (_, handle) = spawn_terminal_view_owned(
        &shared,
        1,
        session,
        TerminalSpawnOptions::default(),
        None,
        false,
    )
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    let observation = (|| -> Result<_, String> {
        loop {
            let title = shared
                .lock()
                .unwrap()
                .descriptor(1, id)
                .map_err(|error| error.message)?
                .title;
            if title.as_deref() == Some("Native terminal title") {
                return Ok(title);
            }
            if Instant::now() >= deadline {
                return Err("production event observer did not publish the PTY title".into());
            }
            thread::sleep(Duration::from_millis(5));
        }
    })();
    handle.shutdown().unwrap();
    assert_eq!(observation.unwrap(), Some("Native terminal title".into()));
}

#[test]
fn stable_conversation_title_survives_replay_eviction_without_duplicate_edges() {
    let shared = Arc::new(crate::host::state::HostLock::new(test_host()));
    let recipe = crate::model::LaunchRecipe {
        unset_environment: Default::default(),
        program: "/bin/cat".into(),
        arguments: Vec::new(),
        working_directory: "/".into(),
        environment: BTreeMap::new(),
    };
    let session = TerminalSession {
        recovery_origin: None,
        predecessor: None,
        owned_codex_listener: None,
        id: TerminalId::new(),
        launch: recipe.clone(),
        transcript_path: None,
        resume_hint: None,
        continuity: TerminalContinuity::RelaunchAvailable {
            recipe,
            last_exit: None,
        },
    };
    let id = session.id;
    let handle = TerminalHandle::spawn(session.clone(), TerminalConfig::default(), |_| {}).unwrap();
    shared
        .lock()
        .unwrap()
        .finish_spawn(1, session, handle.clone(), false)
        .unwrap();
    let weak = Arc::downgrade(&shared);
    record_event(
        &weak,
        id,
        TerminalEvent::ConversationTitleChanged("Stable title".into()),
    );
    assert_eq!(
        shared.lock().unwrap().replay_events(id, 0).unwrap().len(),
        1
    );
    for _ in 0..EVENT_REPLAY_CAPACITY {
        record_event(&weak, id, TerminalEvent::Bell);
    }
    let replay = shared.lock().unwrap().replay_events(id, 0).unwrap();
    assert_eq!(replay.len(), EVENT_REPLAY_CAPACITY + 1);
    assert!(
        matches!(&replay[0], (1, WireTerminalEvent::ConversationTitleChanged { title }) if title == "Stable title")
    );
    assert_eq!(
        shared.lock().unwrap().replay_events(id, 1).unwrap().len(),
        EVENT_REPLAY_CAPACITY
    );
    record_event(
        &weak,
        id,
        TerminalEvent::ConversationTitleChanged("Renamed".into()),
    );
    let replay = shared.lock().unwrap().replay_events(id, 0).unwrap();
    assert_eq!(
        replay
            .iter()
            .filter(|(_, event)| matches!(
                event,
                WireTerminalEvent::ConversationTitleChanged { .. }
            ))
            .count(),
        1
    );
    let _ = handle.shutdown();
}

#[test]
fn leader_exit_preserves_consumption_credit_but_not_process_authority() {
    let shared = Arc::new(crate::host::state::HostLock::new(test_host()));
    let recipe = crate::model::LaunchRecipe {
        unset_environment: Default::default(),
        program: "/bin/cat".into(),
        arguments: Vec::new(),
        working_directory: std::path::PathBuf::from("/tmp"),
        environment: BTreeMap::new(),
    };
    let session = TerminalSession {
        owned_codex_listener: None,
        recovery_origin: None,
        predecessor: None,
        id: TerminalId::new(),
        launch: recipe.clone(),
        transcript_path: None,
        resume_hint: None,
        continuity: TerminalContinuity::RelaunchAvailable {
            recipe,
            last_exit: None,
        },
    };
    let id = session.id;
    let handle = TerminalHandle::spawn(session.clone(), TerminalConfig::default(), |_| {}).unwrap();
    {
        let mut state = shared.lock().unwrap_or_else(|error| error.into_inner());
        state.reserve_spawn(id).unwrap();
        state
            .finish_spawn(1, session, handle.clone(), false)
            .unwrap();
    }
    // The shell-exit event can precede PTY EOF while a descendant owns it.
    record_event(
        &Arc::downgrade(&shared),
        id,
        TerminalEvent::Exited {
            exit_code: 0,
            signal: None,
            intentional: false,
        },
    );
    let capability = {
        let mut state = shared.lock().unwrap_or_else(|error| error.into_inner());
        state
            .set_visibility(1, id, TerminalVisibility::Visible)
            .unwrap();
        let generation = handle.generation();
        assert!(state.redraw_ack_handle(1, id, generation).is_ok());
        assert!(state.focus_handle(1, id).unwrap().is_some());
        assert!(
            state.surface_handle(1, id).is_ok(),
            "a descendant's live PTY retains input and scrolling"
        );
        assert!(
            state
                .descriptor(1, id)
                .unwrap()
                .session
                .continuity
                .live_proof()
                .is_some()
        );
        assert_eq!(
            state.focus_handle(2, id).err().unwrap().code,
            ProtocolErrorCode::PermissionDenied
        );
        assert_eq!(
            state.handle(1, id).err().unwrap().code,
            ProtocolErrorCode::TerminalUnavailable
        );
        assert_eq!(
            state
                .redraw_ack_handle(2, id, generation)
                .err()
                .unwrap()
                .code,
            ProtocolErrorCode::PermissionDenied
        );
        assert_eq!(
            state.redraw_ack_handle(1, id, u64::MAX).err().unwrap().code,
            ProtocolErrorCode::InvalidRequest
        );
        assert_eq!(
            state.shutdown(empty_stop(), Instant::now()),
            ShutdownOutcome::EmptyRequired { current_count: 1 }
        );
        state.shutdown_preflight(Instant::now()).unwrap().capability
    };
    handle.shutdown().unwrap();
    let mut state = shared.lock().unwrap_or_else(|error| error.into_inner());
    state
        .set_visibility(1, id, TerminalVisibility::Hidden)
        .unwrap();
    state
        .set_visibility(1, id, TerminalVisibility::Visible)
        .unwrap();
    assert_eq!(
        state.shutdown(ShutdownOperation::Confirm { capability }, Instant::now()),
        ShutdownOutcome::InventoryChanged { current_count: 0 }
    );
    assert_eq!(
        state.terminals.len(),
        1,
        "completed descriptors remain available as history"
    );
    assert_eq!(
        state.shutdown(empty_stop(), Instant::now()),
        ShutdownOutcome::Accepted {
            live_terminals_before: 0,
            termination_requested: true
        }
    );
}

#[test]
fn established_terminals_require_confirmation_and_pending_finish_is_rejected() {
    let now = Instant::now();
    let mut state = test_host();
    let recipe = crate::model::LaunchRecipe {
        unset_environment: Default::default(),
        program: "/bin/cat".into(),
        arguments: Vec::new(),
        working_directory: std::path::PathBuf::from("/tmp"),
        environment: BTreeMap::new(),
    };
    let mut handles = Vec::new();
    for count in 1..=3 {
        let session = TerminalSession {
            owned_codex_listener: None,
            recovery_origin: None,
            predecessor: None,
            id: TerminalId::new(),
            launch: recipe.clone(),
            transcript_path: None,
            resume_hint: None,
            continuity: TerminalContinuity::RelaunchAvailable {
                recipe: recipe.clone(),
                last_exit: None,
            },
        };
        state.reserve_spawn(session.id).unwrap();
        let handle =
            TerminalHandle::spawn(session.clone(), TerminalConfig::default(), |_| {}).unwrap();
        handles.push(handle.clone());
        state.finish_spawn(1, session, handle, false).unwrap();
        assert_eq!(
            state.shutdown(empty_stop(), now),
            ShutdownOutcome::EmptyRequired {
                current_count: count
            }
        );
    }
    let pending = TerminalSession {
        owned_codex_listener: None,
        recovery_origin: None,
        predecessor: None,
        id: TerminalId::new(),
        launch: recipe.clone(),
        transcript_path: None,
        resume_hint: None,
        continuity: TerminalContinuity::RelaunchAvailable {
            recipe,
            last_exit: None,
        },
    };
    state.reserve_spawn(pending.id).unwrap();
    let capability = state.shutdown_preflight(now).unwrap().capability;
    assert_eq!(capability.live_terminals, 4);
    assert_eq!(
        state.shutdown(ShutdownOperation::Confirm { capability }, now),
        ShutdownOutcome::Accepted {
            live_terminals_before: 4,
            termination_requested: true
        }
    );
    let pending_handle =
        TerminalHandle::spawn(pending.clone(), TerminalConfig::default(), |_| {}).unwrap();
    assert!(
        state
            .finish_spawn(1, pending, pending_handle.clone(), false)
            .is_err()
    );
    assert_eq!(state.terminals.len(), 3);
    pending_handle.shutdown().unwrap();
    for handle in handles {
        handle.shutdown().unwrap();
    }
}

#[test]
fn shutdown_reservations_and_spawn_fence_are_atomic() {
    let now = Instant::now();
    let mut empty = test_host();
    assert_eq!(
        empty.shutdown(empty_stop(), now),
        ShutdownOutcome::Accepted {
            live_terminals_before: 0,
            termination_requested: true
        }
    );
    assert!(empty.reserve_spawn(TerminalId::new()).is_err());
    assert_eq!(
        empty.shutdown(empty_stop(), now),
        ShutdownOutcome::AlreadyShuttingDown
    );
    for count in [1, 7] {
        let mut state = test_host();
        for _ in 0..count {
            state.reserve_spawn(TerminalId::new()).unwrap();
        }
        assert_eq!(
            state.shutdown(empty_stop(), now),
            ShutdownOutcome::EmptyRequired {
                current_count: count
            }
        );
        assert!(!state.shutting_down);
    }
}

#[test]
fn background_task_replacement_invalidates_stop_and_update_consent() {
    use crate::{
        agent::Provider,
        chat::protocol::{Adapter, ProcessSpec, Step, TransportSpec},
        chat::{self, Command, Event, ToolStatus},
        model::LaunchRecipe,
        update::impact::{Disposition, Policy},
    };
    const CONVERSATION: &str = "12345678-1234-4234-8234-123456789abc";
    struct BackgroundAdapter;
    impl BackgroundAdapter {
        fn task(id: &str, status: ToolStatus) -> Event {
            Event::Subagent {
                model: None,
                waiting: None,
                detail: None,
                id: id.into(),
                // The Claude adapter resolves `kind` to the label a person
                // reads: the agent's name, else its task, else its type.
                kind: id.into(),
                description: id.into(),
                status: Some(status),
                background: Some(true),
            }
        }
    }
    impl Adapter for BackgroundAdapter {
        fn initialize(&mut self) -> Result<Step, String> {
            Ok(Step {
                writes: vec![serde_json::json!({})],
                events: vec![
                    Event::Session {
                        id: CONVERSATION.into(),
                        title: None,
                    },
                    Event::Ready(true),
                    Event::Capabilities(chat::Capabilities {
                        interrupt_stops_background: true,
                        ..Default::default()
                    }),
                    Self::task("first", ToolStatus::Running),
                ],
                ..Default::default()
            })
        }
        fn command(&mut self, command: Command) -> Result<Step, String> {
            let events = match command {
                Command::Compact => vec![
                    Self::task("first", ToolStatus::Succeeded),
                    Self::task("replacement", ToolStatus::Running),
                ],
                Command::Interrupt => vec![Self::task("replacement", ToolStatus::Stopped)],
                _ => vec![],
            };
            Ok(Step {
                events,
                ..Default::default()
            })
        }
        fn receive(&mut self, _: serde_json::Value) -> Result<Step, String> {
            Ok(Step {
                events: vec![Event::Session {
                    id: CONVERSATION.into(),
                    title: None,
                }],
                ..Default::default()
            })
        }
    }
    let directory = tempfile::tempdir().unwrap();
    let config = directory.path().join("claude-config");
    let project: String = directory
        .path()
        .to_str()
        .unwrap()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let transcript = config
        .join("projects")
        .join(project)
        .join(format!("{CONVERSATION}.jsonl"));
    fs::create_dir_all(transcript.parent().unwrap()).unwrap();
    fs::write(transcript, "").unwrap();
    let selected = BTreeMap::from([(
        "CLAUDE_CONFIG_DIR".into(),
        config.to_string_lossy().into_owned(),
    )]);
    let mut environment =
        crate::model::agent_recovery::fresh_native_environment(Provider::Claude, false);
    environment.extend(selected.clone());
    let spec = ProcessSpec {
        stdout_noise: None,
        followed: false,
        execution: None,
        messages: None,
        unset_environment: Default::default(),
        transport: TransportSpec::JsonLines,
        program: "/bin/cat".into(),
        arguments: Vec::new(),
        working_directory: directory.path().into(),
        environment,
    };
    let mut adapter = BackgroundAdapter;
    let mut initial = adapter.initialize().unwrap();
    let store = chat::runtime::preflight(&spec, &mut initial).unwrap();
    let handle =
        chat::runtime::Handle::start(spec, Box::new(adapter), initial, store, None, |_| {})
            .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while handle.confirmed_session_id().as_deref() != Some(CONVERSATION) {
        assert!(
            Instant::now() < deadline,
            "provider identity was not confirmed"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        handle.check_update_readiness().unwrap(),
        chat::runtime::UpdateReadiness::Busy
    );
    let mut state = test_host();
    let session = chat::Session {
        id: TerminalId::new(),
        native_continuity: Default::default(),
        owned_codex_listener: None,
        terminal: None,
        proof: Some(state.proof().clone()),
        launch: chat::Launch {
            provider: Provider::Claude,
            session_id: Some(CONVERSATION.into()),
            recipe: LaunchRecipe {
                unset_environment: Default::default(),
                program: "claude".into(),
                arguments: vec!["--resume".into(), CONVERSATION.into()],
                working_directory: directory.path().into(),
                environment: selected,
            },
        },
    };
    state.chats.insert(
        session.id,
        super::super::chat::Record {
            execution_instance: uuid::Uuid::new_v4(),
            console_recipe: None,
            paired_terminal: None,
            session,
            handle: handle.clone(),
            source_process: None,
            subscribers: BTreeMap::new(),
            observations: BTreeMap::new(),
            completion_observed: false,
            returning_terminal: false,
        },
    );
    let waiting = state.upgrade_impact(Policy::WhenIdle).unwrap();
    assert_eq!(
        waiting.sessions[0].disposition,
        Disposition::WaitsForIdle,
        "{}",
        waiting.sessions[0].reason
    );
    assert!(waiting.sessions[0].interrupted_subagents.is_empty());
    let interrupt = state.upgrade_impact(Policy::Interrupt).unwrap();
    assert_eq!(interrupt.sessions[0].disposition, Disposition::Interrupted);
    assert_eq!(interrupt.sessions[0].interrupted_subagents, ["first"]);
    assert!(
        interrupt.sessions[0].reason.starts_with(
            "The update will stop 1 running background subagent (\u{201c}first\u{201d})"
        ),
        "only a turn under way is called the active turn: {}",
        interrupt.sessions[0].reason
    );
    assert!(
        waiting.sessions[0]
            .reason
            .contains("Background subagent \u{201c}first\u{201d} is still running"),
        "{}",
        waiting.sessions[0].reason
    );
    let now = Instant::now();
    // Another session opening or closing elsewhere is not a change to this
    // conversation's owner, so it cannot revoke a review of it.
    state.inventory_generation += 1;
    let elsewhere = state.upgrade_impact(Policy::Interrupt).unwrap();
    assert_eq!(
        elsewhere.sessions[0].identity,
        interrupt.sessions[0].identity
    );
    assert!(interrupt.preserves_consent(&elsewhere));
    // Messages accepted for it but not yet delivered cannot survive a restart:
    // the update waits for them under either policy, attributed to this
    // conversation rather than to an anonymous blocker.
    for policy in [Policy::WhenIdle, Policy::Interrupt] {
        let queued = state
            .upgrade_impact_with_queued(policy, &BTreeSet::from([interrupt.sessions[0].id]))
            .unwrap();
        assert!(queued.blockers.is_empty());
        assert_eq!(queued.sessions[0].disposition, Disposition::WaitsForIdle);
        assert!(queued.sessions[0].interrupted_subagents.is_empty());
        // The review shows why it waits, not that it is finishing a turn.
        let shown = queued.sessions[0].review().reason;
        assert!(shown.contains("still waiting to be delivered"), "{shown}");
    }
    let capability = state.shutdown_preflight(now).unwrap().capability;
    assert_eq!(capability.background_subagents, 1);
    handle.command(Command::Compact).unwrap();
    let changed = state.upgrade_impact(Policy::Interrupt).unwrap();
    assert!(!interrupt.preserves_consent(&changed));
    assert_eq!(changed.capability.background_subagents, 1);
    assert_ne!(
        changed.capability.background_identity,
        capability.background_identity
    );
    assert!(
        state
            .validate_upgrade_capability(&capability, now)
            .unwrap_err()
            .contains("inventory changed")
    );
    assert_eq!(
        state.shutdown(ShutdownOperation::Confirm { capability }, now),
        ShutdownOutcome::InventoryChanged { current_count: 1 }
    );
    assert!(!state.shutting_down);
    let capability = state.shutdown_preflight(now).unwrap().capability;
    assert_eq!(
        state.shutdown(ShutdownOperation::Confirm { capability }, now),
        ShutdownOutcome::Accepted {
            live_terminals_before: 1,
            termination_requested: true
        }
    );
    handle.interrupt_owned(None, true).unwrap();
    handle.close().unwrap();
}

#[test]
fn shutdown_capabilities_detect_inventory_aba_expiry_tampering_and_restart() {
    let now = Instant::now();
    let mut state = test_host();
    let capability = state.shutdown_preflight(now).unwrap().capability;
    let terminal_id = TerminalId::new();
    state.reserve_spawn(terminal_id).unwrap();
    state.cancel_spawn(terminal_id);
    assert_eq!(
        state.shutdown(ShutdownOperation::Confirm { capability }, now),
        ShutdownOutcome::InventoryChanged { current_count: 0 }
    );
    let capability = state.shutdown_preflight(now).unwrap().capability;
    assert_eq!(
        state.shutdown(
            ShutdownOperation::Confirm {
                capability: capability.clone()
            },
            now + SHUTDOWN_CONFIRMATION_TTL
        ),
        ShutdownOutcome::Expired
    );
    let mut tampered = capability.clone();
    tampered.live_terminals = 500;
    assert_eq!(
        state.shutdown(
            ShutdownOperation::Confirm {
                capability: tampered
            },
            now
        ),
        ShutdownOutcome::InvalidCapability
    );
    let mut restarted = test_host_with_incarnation("new-incarnation");
    assert_eq!(
        restarted.shutdown(
            ShutdownOperation::Confirm {
                capability: capability.clone()
            },
            now
        ),
        ShutdownOutcome::DaemonRestarted
    );
    assert!(matches!(
        state.shutdown(
            ShutdownOperation::Confirm {
                capability: capability.clone()
            },
            now
        ),
        ShutdownOutcome::Accepted { .. }
    ));
    assert_eq!(
        state.shutdown(ShutdownOperation::Confirm { capability }, now),
        ShutdownOutcome::AlreadyShuttingDown
    );
}

#[test]
fn concurrent_reservation_and_empty_stop_have_only_safe_orderings() {
    for spawn_first in [false, true] {
        let host = Arc::new(crate::host::state::HostLock::new(test_host()));
        let (release, wait) = std::sync::mpsc::channel();
        let other = Arc::clone(&host);
        let worker = thread::spawn(move || {
            wait.recv().unwrap();
            other.lock().unwrap().reserve_spawn(TerminalId::new())
        });
        if spawn_first {
            release.send(()).unwrap();
            worker.join().unwrap().unwrap();
            assert_eq!(
                host.lock().unwrap().shutdown(empty_stop(), Instant::now()),
                ShutdownOutcome::EmptyRequired { current_count: 1 }
            );
        } else {
            let outcome = host.lock().unwrap().shutdown(empty_stop(), Instant::now());
            release.send(()).unwrap();
            assert!(matches!(outcome, ShutdownOutcome::Accepted { .. }));
            assert!(worker.join().unwrap().is_err());
        }
    }
}

#[test]
fn disconnect_drops_only_attachments_and_allows_reconnect() {
    let terminal_id = TerminalId::new();
    let mut subscriptions = Subscriptions::default();
    subscriptions.attach(1, terminal_id, TerminalVisibility::Visible);
    assert!(subscriptions.attached(1, terminal_id));

    subscriptions.disconnect(1);
    assert!(!subscriptions.attached(1, terminal_id));
    assert!(subscriptions.clients(terminal_id).is_empty());

    subscriptions.attach(2, terminal_id, TerminalVisibility::Hidden);
    assert!(subscriptions.attached(2, terminal_id));
    assert_eq!(subscriptions.clients(terminal_id), vec![2]);
}

#[test]
fn visibility_is_aggregated_and_empty_terminals_stay_hidden() {
    let terminal_id = TerminalId::new();
    let mut subscriptions = Subscriptions::default();
    assert_eq!(
        subscriptions.visibility(terminal_id),
        TerminalVisibility::Hidden
    );

    subscriptions.attach(1, terminal_id, TerminalVisibility::Hibernated);
    assert_eq!(
        subscriptions.visibility(terminal_id),
        TerminalVisibility::Hibernated
    );
    subscriptions.attach(2, terminal_id, TerminalVisibility::Hidden);
    assert_eq!(
        subscriptions.visibility(terminal_id),
        TerminalVisibility::Hidden
    );
    subscriptions.attach(3, terminal_id, TerminalVisibility::Visible);
    assert_eq!(
        subscriptions.visibility(terminal_id),
        TerminalVisibility::Visible
    );

    subscriptions.detach(3, terminal_id);
    assert_eq!(
        subscriptions.visibility(terminal_id),
        TerminalVisibility::Hidden
    );
    subscriptions.disconnect(2);
    assert_eq!(
        subscriptions.visibility(terminal_id),
        TerminalVisibility::Hibernated
    );
    subscriptions.detach(1, terminal_id);
    assert_eq!(
        subscriptions.visibility(terminal_id),
        TerminalVisibility::Hidden
    );
}

/// The daemon creates a channel in a worktree of its own repository and
/// reports that checkout back; it refuses one that is not a worktree of the
/// repository, changing nothing, and keeps a main-checkout channel's absent.
#[test]
fn the_daemon_validates_and_reports_a_channels_checkout() {
    let fixture = crate::workspace_model::CheckoutFixture::new();
    let directory = tempfile::tempdir().unwrap();
    let mut state = test_host();
    state.install_model(
        super::super::model::Registry::at(&directory.path().join("model.sqlite3")).unwrap(),
    );
    state
        .edit_model(
            1,
            ModelEdit::OpenProject {
                repository: fixture.repository.clone(),
                name: "repo".into(),
                kind: Default::default(),
            },
        )
        .unwrap();
    let create = |checkout: &std::path::Path| ModelEdit::CreateChannel {
        id: crate::model::ChannelId::new(),
        repository: fixture.repository.clone(),
        title: "Feature".into(),
        title_source: Default::default(),
        checkout: Some(checkout.to_path_buf()),
    };
    let elsewhere = tempfile::tempdir().unwrap();
    let refused = state.edit_model(1, create(elsewhere.path())).unwrap_err();
    assert_eq!(refused.code, ProtocolErrorCode::InvalidRequest);
    assert!(
        refused.message.contains("is not a worktree of"),
        "{}",
        refused.message
    );
    assert!(
        state
            .registry()
            .unwrap()
            .model()
            .unwrap()
            .channels
            .is_empty()
    );
    let relative = state
        .edit_model(1, create(std::path::Path::new("feature")))
        .unwrap_err();
    assert_eq!(relative.code, ProtocolErrorCode::InvalidRequest);

    let model = state.edit_model(1, create(&fixture.linked)).unwrap();
    assert_eq!(model.channels[0].checkout, Some(fixture.linked.clone()));
    let model = state.edit_model(1, create(&fixture.main)).unwrap();
    assert_eq!(
        model
            .channels
            .iter()
            .map(|channel| channel.checkout.clone())
            .collect::<Vec<_>>(),
        [Some(fixture.linked.clone()), None],
        "the main checkout is recorded as no checkout"
    );
}
