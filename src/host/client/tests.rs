use super::*;
use crate::model::TerminalId;

#[test]
fn recovery_chat_requests_preserve_ordinary_exit_and_committed_shutdown_fences() {
    for ordinary_exit in [true, false] {
        let (proof, host) = panic_test_host("recovery-chat-exit-fence");
        host.lock()
            .unwrap()
            .retain_startup_recovery("original owner requires explicit Close".into());
        // The new read-only request crosses the real dispatch path before
        // shutdown, without requiring an actor, adoption or transcript.
        assert!(matches!(
            process_request(&host, 0, ClientRequest::ChatList).unwrap().0,
            ServerResponse::ChatListed { sessions } if sessions.is_empty()
        ));
        let source = crate::chat::Session {
            id: TerminalId::new(),
            proof: Some(proof),
            terminal: None,
            native_continuity: Default::default(),
            owned_codex_listener: None,
            launch: crate::chat::Launch {
                provider: crate::agent::Provider::Codex,
                session_id: None,
                recipe: crate::model::LaunchRecipe {
                    program: "/must-not-execute".into(),
                    arguments: Vec::new(),
                    working_directory: "/".into(),
                    environment: Default::default(),
                    unset_environment: Default::default(),
                },
            },
        };
        {
            let mut state = host.lock().unwrap();
            if ordinary_exit {
                state.begin_ordinary_exit_cleanup();
            } else {
                // No live originals exist in this local gate fixture, so
                // the real request_shutdown transition may commit.
                state.request_shutdown();
            }
            assert!(state.shutting_down);
        }
        for request in [
            ClientRequest::ChatList,
            ClientRequest::ChatCloseRetained { session: source },
        ] {
            let startup = chat_startup_request(&request);
            let failure = process_request(&host, 0, request).unwrap_err();
            assert_eq!(failure.retryable, startup);
            assert_eq!(failure.code, ProtocolErrorCode::TerminalUnavailable);
            assert_eq!(
                failure.message,
                if ordinary_exit {
                    "Ordinary daemon exit is waiting for exact original child cleanup; only inspection and explicit checked Close remain available"
                } else {
                    "Daemon upgrade/shutdown is committed; no further control mutations are accepted"
                }
            );
            let state = host.lock().unwrap();
            assert!(state.shutting_down);
            assert_eq!(state.ordinary_exit_cleanup_pending(), ordinary_exit);
            assert_eq!(
                state.startup_recovery(),
                Some("original owner requires explicit Close")
            );
            assert!(state.chats.is_empty());
            assert!(state.chat_spawning.is_empty());
            assert!(state.native_closing.is_empty());
        }
    }
}

/// The daemon's admission refuses an automatic account switch before it
/// changes anything, so the switch is marked as refused before any effect
/// and keeps the sentence; an explicit recovery keeps the refusal as is.
#[test]
fn admission_refusing_an_automatic_switch_marks_it_not_admitted() {
    let (proof, host) = panic_test_host("automatic-switch-admission");
    let source = crate::chat::Session {
        id: TerminalId::new(),
        proof: Some(proof),
        terminal: None,
        native_continuity: Default::default(),
        owned_codex_listener: None,
        launch: crate::chat::Launch {
            provider: crate::agent::Provider::Codex,
            session_id: Some("12345678-1234-4234-8234-123456789abc".into()),
            recipe: crate::model::LaunchRecipe {
                program: "/must-not-execute".into(),
                arguments: Vec::new(),
                working_directory: "/".into(),
                environment: Default::default(),
                unset_environment: Default::default(),
            },
        },
    };
    let mut replacement = source.clone();
    replacement.id = TerminalId::new();
    replacement.proof = None;
    host.lock().unwrap().request_shutdown();
    for (idle_generation, code) in [
        (Some(3), ProtocolErrorCode::NotAdmitted),
        (None, ProtocolErrorCode::TerminalUnavailable),
    ] {
        let failure = process_request(
            &host,
            0,
            ClientRequest::ChatRecoverAccount {
                source: source.clone(),
                replacement: replacement.clone(),
                idle_generation,
                execution_environment: Default::default(),
            },
        )
        .unwrap_err();
        assert_eq!(failure.code, code, "{idle_generation:?}");
        assert_eq!(
            failure.message,
            "Daemon upgrade/shutdown is committed; no further control mutations are accepted"
        );
        assert!(!failure.retryable);
        let state = host.lock().unwrap();
        assert!(state.chats.is_empty());
        assert!(state.chat_spawning.is_empty());
    }
}

fn wait_client_retirement(mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !ready() {
        assert!(
            Instant::now() < deadline,
            "original terminal retirement did not settle"
        );
        thread::sleep(Duration::from_millis(5));
    }
}

fn finish_test_terminal_close(
    host: &SharedHost,
    terminal: TerminalId,
    handle: &crate::terminal::TerminalHandle,
    generation: Result<Option<String>, String>,
) -> Result<(), ProtocolError> {
    let source = host.lock().unwrap().terminals[&terminal].session.clone();
    let close = reserve_terminal_close(host, terminal, handle, &source, None, generation, None)?;
    finish_terminal_close(host, close)
}

fn retry_retirement_close(
    host: &SharedHost,
    request: impl Fn() -> ClientRequest,
) -> RequestOutcome {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match process_request(host, 1, request()) {
            Ok(reply) => return reply,
            Err(error)
                if error.code == ProtocolErrorCode::TerminalUnavailable
                    && error.retryable
                    && (error.message.contains("original terminal holder")
                        || error.message.contains("Terminal worker retirement")) =>
            {
                assert!(Instant::now() < deadline, "close stayed pending: {error:?}");
                thread::sleep(Duration::from_millis(5));
            }
            Err(error) => panic!("unexpected close refusal: {error:?}"),
        }
    }
}

fn retention_client_host(
    name: &str,
) -> (
    SharedHost,
    tempfile::TempDir,
    crate::model::TerminalSession,
    crate::terminal::TerminalHandle,
) {
    let (_, host) = panic_test_host(name);
    let directory = tempfile::Builder::new().prefix("rp-").tempdir().unwrap();
    let mut recipe = crate::model::LaunchRecipe::shell(directory.path().into()).unwrap();
    recipe.program = "/bin/true".into();
    recipe.arguments.clear();
    let session = crate::model::TerminalSession {
        id: TerminalId::new(),
        launch: recipe.clone(),
        transcript_path: None,
        resume_hint: None,
        owned_codex_listener: None,
        recovery_origin: None,
        predecessor: None,
        continuity: crate::model::TerminalContinuity::RelaunchAvailable {
            recipe,
            last_exit: None,
        },
    };
    let terminal =
        crate::terminal::TerminalHandle::spawn(session.clone(), Default::default(), |_| {})
            .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !terminal.is_completed() {
        assert!(Instant::now() < deadline, "real PTY did not complete");
        thread::sleep(Duration::from_millis(5));
    }
    terminal.shutdown().unwrap();
    let proof = host.lock().unwrap().proof_handle();
    wait_client_retirement(|| {
        matches!(
            terminal.completed_retirement(&session, &proof),
            crate::terminal::TerminalRetirement::Released
        )
    });
    {
        let mut state = host.lock().unwrap();
        state.install_model(
            super::super::model::Registry::at(&directory.path().join("model.sqlite3")).unwrap(),
        );
        state.register_client(1);
        state.reserve_spawn(session.id).unwrap();
        state
            .finish_spawn(1, session.clone(), terminal.clone(), false)
            .unwrap();
    }
    (host, directory, session, terminal)
}

/// Hold the real handler after final retirement but before serve_with can
/// enqueue its response. Ordinary exit must not seal that delivery gap.
#[test]
fn destructive_terminal_replies_drain_before_ordinary_exit_seals_delivery() {
    use std::sync::mpsc;
    const CASE: &str = "CAGENTS_EXIT_DRAIN_TEST_CASE";
    let name = thread::current().name().unwrap().to_owned();
    let cases = match std::env::var(CASE) {
        Ok(case) => vec![case],
        Err(_) => ["checked", "release", "shutdown", "chat"]
            .into_iter()
            .map(str::to_owned)
            .collect(),
    };
    fn chat_close_case() {
        use crate::chat::protocol::{Adapter, ProcessSpec, Step};
        struct Quiet;
        impl Adapter for Quiet {
            fn initialize(&mut self) -> Result<Step, String> {
                Ok(Step::default())
            }
            fn command(&mut self, _: crate::chat::Command) -> Result<Step, String> {
                Ok(Step::default())
            }
            fn receive(&mut self, _: serde_json::Value) -> Result<Step, String> {
                Ok(Step::default())
            }
        }
        thread_local! {
            static PAUSE: std::cell::RefCell<Option<(mpsc::Sender<()>, mpsc::Receiver<()>)>> =
                const { std::cell::RefCell::new(None) };
        }
        fn close_then_pause(
            host: &SharedHost,
            client: ClientId,
            request: ClientRequest,
        ) -> Result<RequestOutcome, ProtocolError> {
            let closing = matches!(&request, ClientRequest::ChatClose { .. });
            if matches!(&request, ClientRequest::ChatCloseRetained { .. }) {
                assert!(
                    host.lock().unwrap().ordinary_exit_cleanup_blocked(),
                    "retained Close must own reply custody even on refusal"
                );
            }
            let result = process_request(host, client, request);
            if closing {
                assert!(result.is_ok(), "real native chat close failed: {result:?}");
                assert!(host.lock().unwrap().chats.is_empty());
                PAUSE.with(|pause| {
                    let (closed, resume) = pause.borrow_mut().take().unwrap();
                    closed.send(()).unwrap();
                    resume.recv_timeout(Duration::from_secs(5)).unwrap();
                });
            }
            result
        }
        fn send(
            stream: &mut UnixStream,
            proof: &HostSessionProof,
            id: u64,
            request: ClientRequest,
        ) {
            FrameCodec::new(MAX_CLIENT_FRAME_BYTES)
                .write(
                    stream,
                    &ClientEnvelope {
                        version: PROTOCOL_VERSION,
                        request_id: id,
                        proof: Some(proof.clone()),
                        request,
                    },
                )
                .unwrap();
        }
        fn reply(stream: &mut UnixStream, id: u64) -> ServerMessage {
            let mut codec = FrameCodec::new(MAX_FRAME_BYTES);
            loop {
                let envelope = codec
                    .read::<ServerEnvelope>(stream)
                    .unwrap()
                    .expect("native close reply lost before delivery drained");
                if envelope.request_id == Some(id) {
                    return envelope.message;
                }
            }
        }
        let (proof, host) = panic_test_host("chat-reply-custody");
        let directory = tempfile::Builder::new().prefix("cq-").tempdir().unwrap();
        let mut recipe = crate::model::LaunchRecipe::shell(directory.path().into()).unwrap();
        // The adapter is in-process; its harmless JSON-lines transport
        // is a real local cat, never an installed model/provider.
        recipe.program = "/bin/cat".into();
        recipe.arguments.clear();
        let spec = ProcessSpec::from(&recipe);
        let mut initial = Step::default();
        let store = crate::chat::runtime::preflight(&spec, &mut initial).unwrap();
        let actor = crate::chat::runtime::Handle::start(
            spec,
            Box::new(Quiet),
            initial,
            store,
            None,
            |_| {},
        )
        .unwrap();
        assert!(!actor.is_completed());
        let source = crate::chat::Session {
            id: TerminalId::new(),
            proof: Some(proof.clone()),
            terminal: None,
            native_continuity: Default::default(),
            owned_codex_listener: None,
            launch: crate::chat::Launch {
                provider: crate::agent::Provider::Omp,
                recipe,
                session_id: None,
            },
        };
        {
            let mut state = host.lock().unwrap();
            state.install_model(
                super::super::model::Registry::at(&directory.path().join("model.sqlite3")).unwrap(),
            );
            state.chats.insert(
                source.id,
                super::super::chat::Record {
                    execution_instance: uuid::Uuid::new_v4(),
                    console_recipe: None,
                    paired_terminal: None,
                    session: source.clone(),
                    handle: actor.clone(),
                    source_process: None,
                    subscribers: std::collections::BTreeMap::from([(1, false)]),
                    observations: Default::default(),
                    completion_observed: false,
                    returning_terminal: false,
                },
            );
        }
        let (mut stream, server_stream) = UnixStream::pair().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let (closed_tx, closed_rx) = mpsc::channel();
        let (resume_tx, resume_rx) = mpsc::channel();
        let serving_host = Arc::clone(&host);
        let serving = thread::spawn(move || {
            PAUSE.with(|pause| *pause.borrow_mut() = Some((closed_tx, resume_rx)));
            serve_with(server_stream, 1, serving_host, close_then_pause);
        });
        send(
            &mut stream,
            &proof,
            1,
            ClientRequest::Connect {
                expected_host: Some(proof.clone()),
                resume: Vec::new(),
            },
        );
        assert!(matches!(
            reply(&mut stream, 1),
            ServerMessage::Response {
                response: ServerResponse::Connected { .. }
            }
        ));
        // This actor has no detached authenticated holder. Real retained
        // cleanup must refuse it; its error reply must release the guard.
        // Successful retained-native teardown is not claimed here.
        send(
            &mut stream,
            &proof,
            2,
            ClientRequest::ChatCloseRetained {
                session: source.clone(),
            },
        );
        assert!(matches!(reply(&mut stream, 2), ServerMessage::Error(error)
            if error.code == ProtocolErrorCode::TerminalUnavailable && error.retryable));
        let deadline = Instant::now() + Duration::from_secs(5);
        while host.lock().unwrap().ordinary_exit_cleanup_blocked() {
            assert!(
                Instant::now() < deadline,
                "refused retained close leaked reply custody"
            );
            thread::sleep(Duration::from_millis(5));
        }
        assert!(!actor.is_completed());
        assert!(
            host.lock().unwrap().chats[&source.id]
                .handle
                .same_attachment(&actor)
        );
        assert!(host.lock().unwrap().chat_spawning.is_empty());
        assert!(host.lock().unwrap().native_closing.is_empty());
        send(
            &mut stream,
            &proof,
            3,
            ClientRequest::ChatClose { chat_id: source.id },
        );
        closed_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(actor.is_completed());
        assert!(host.lock().unwrap().chats.is_empty());
        let drain = super::super::begin_ordinary_exit(&host).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !drain.try_seal_absence() {
            assert!(
                Instant::now() < deadline,
                "chat child custody did not settle"
            );
            thread::sleep(Duration::from_millis(5));
        }
        let mut cleanup_error = None;
        assert!(host.lock().unwrap().ordinary_exit_cleanup_blocked());
        assert!(!super::super::finish_shutdown_owners(
            &host,
            &mut cleanup_error,
            false
        ));
        resume_tx.send(()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while host.lock().unwrap().ordinary_exit_cleanup_blocked() {
            assert!(
                Instant::now() < deadline,
                "native chat reply guard did not release"
            );
            thread::sleep(Duration::from_millis(5));
        }
        assert!(super::super::finish_shutdown_owners(
            &host,
            &mut cleanup_error,
            false
        ));
        assert!(cleanup_error.is_none());
        let deliveries = {
            let mut state = host.lock().unwrap();
            assert!(drain.try_seal_absence());
            assert!(!state.ordinary_exit_cleanup_blocked());
            let deliveries = state.close_ordinary_exit_registration();
            assert_eq!(
                state.begin_checked_close_reply(99).unwrap_err().code,
                ProtocolErrorCode::TerminalUnavailable
            );
            deliveries
        };
        super::super::close_shutdown_deliveries(&host, &deliveries);
        assert!(matches!(reply(&mut stream, 3), ServerMessage::Response {
            response: ServerResponse::ChatClosed { chat_id, .. }
        } if chat_id == source.id));
        let mut codec = FrameCodec::new(MAX_FRAME_BYTES);
        while let Some(envelope) = codec.read::<ServerEnvelope>(&mut stream).unwrap() {
            assert_eq!(envelope.request_id, None);
            assert!(matches!(envelope.message, ServerMessage::HostShuttingDown));
        }
        serving.join().unwrap();
        assert!(host.lock().unwrap().clients.is_empty());
        assert!(!host.lock().unwrap().ordinary_exit_cleanup_blocked());
    }
    for case in cases {
        let Some(_root) = super::super::exit_drain_tests::private_runtime_case(&name, Some(&case))
        else {
            continue;
        };
        if case == "chat" {
            chat_close_case();
            continue;
        }
        thread_local! {
            static PAUSE: std::cell::RefCell<Option<(mpsc::Sender<()>, mpsc::Receiver<()>)>> =
                const { std::cell::RefCell::new(None) };
        }
        fn retire_then_pause(
            host: &SharedHost,
            client: ClientId,
            request: ClientRequest,
        ) -> Result<RequestOutcome, ProtocolError> {
            let closing = matches!(
                &request,
                ClientRequest::CloseTerminalChecked { .. }
                    | ClientRequest::CloseTerminalSource { .. }
                    | ClientRequest::ReleaseCompletedTerminal { .. }
                    | ClientRequest::ShutdownTerminal { .. }
            );
            let result = process_request(host, client, request);
            if closing {
                assert!(
                    result.is_ok(),
                    "real terminal retirement failed: {result:?}"
                );
                let state = host.lock().unwrap();
                assert!(state.terminals.is_empty());
                assert!(state.terminal_retirements.is_empty());
                drop(state);
                PAUSE.with(|pause| {
                    let (retired, resume) = pause.borrow_mut().take().unwrap();
                    retired.send(()).unwrap();
                    resume.recv_timeout(Duration::from_secs(5)).unwrap();
                });
            }
            result
        }
        fn send(
            stream: &mut UnixStream,
            proof: &HostSessionProof,
            request_id: u64,
            request: ClientRequest,
        ) {
            FrameCodec::new(MAX_CLIENT_FRAME_BYTES)
                .write(
                    stream,
                    &ClientEnvelope {
                        version: PROTOCOL_VERSION,
                        request_id,
                        proof: Some(proof.clone()),
                        request,
                    },
                )
                .unwrap();
        }
        fn reply(stream: &mut UnixStream, request_id: u64) -> ServerMessage {
            let mut codec = FrameCodec::new(MAX_FRAME_BYTES);
            loop {
                let envelope = codec
                    .read::<ServerEnvelope>(stream)
                    .unwrap()
                    .expect("correlated reply was lost before delivery drained");
                if envelope.request_id == Some(request_id) {
                    return envelope.message;
                }
            }
        }

        let (host, _directory, mut session, _terminal) = retention_client_host(&case);
        let proof = current_proof(&host);
        assert!(!host.lock().unwrap().ordinary_exit_cleanup_blocked());
        let (mut stream, server_stream) = UnixStream::pair().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let (retired_tx, retired_rx) = mpsc::channel();
        let (resume_tx, resume_rx) = mpsc::channel();
        let serving_host = Arc::clone(&host);
        let serving = thread::spawn(move || {
            PAUSE.with(|pause| *pause.borrow_mut() = Some((retired_tx, resume_rx)));
            serve_with(server_stream, 1, serving_host, retire_then_pause);
        });
        send(
            &mut stream,
            &proof,
            1,
            ClientRequest::Connect {
                expected_host: Some(proof.clone()),
                resume: Vec::new(),
            },
        );
        assert!(matches!(
            reply(&mut stream, 1),
            ServerMessage::Response {
                response: ServerResponse::Connected { .. }
            }
        ));
        let id = session.id;
        let request = match case.as_str() {
            "checked" => ClientRequest::CloseTerminalChecked {
                terminal_id: id,
                allow_active: false,
            },
            "release" => {
                session.continuity = crate::model::TerminalContinuity::Exited {
                    last_exit: crate::model::TerminalExit {
                        code: Some(0),
                        signal: None,
                        occurred_at_unix_ms: 1,
                    },
                };
                ClientRequest::ReleaseCompletedTerminal {
                    session,
                    allow_active: false,
                }
            }
            "shutdown" => ClientRequest::ShutdownTerminal { terminal_id: id },
            _ => panic!("unknown destructive request fixture"),
        };
        send(&mut stream, &proof, 2, request);
        retired_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(retention_pin_generation(&host, id), Ok(None));
        let drain = super::super::begin_ordinary_exit(&host).unwrap();
        wait_client_retirement(|| drain.try_seal_absence());
        // Actual original-child absence is already sealed: only the
        // outstanding correlated reply can block final Host registration.
        assert!(host.lock().unwrap().ordinary_exit_cleanup_blocked());
        resume_tx.send(()).unwrap();
        wait_client_retirement(|| !host.lock().unwrap().ordinary_exit_cleanup_blocked());
        let deliveries = {
            let mut state = host.lock().unwrap();
            assert!(drain.try_seal_absence());
            assert!(!state.ordinary_exit_cleanup_blocked());
            let deliveries = state.close_ordinary_exit_registration();
            assert_eq!(
                state.begin_checked_close_reply(99).unwrap_err().code,
                ProtocolErrorCode::TerminalUnavailable,
            );
            deliveries
        };
        // Close the real writer BEFORE reading the queued success: the
        // frame must survive close_after_drain, not merely a live socket.
        super::super::close_shutdown_deliveries(&host, &deliveries);
        let response = reply(&mut stream, 2);
        if case == "shutdown" {
            assert!(matches!(
                response,
                ServerMessage::Response {
                    response: ServerResponse::Accepted
                }
            ));
        } else {
            assert!(matches!(response, ServerMessage::Response {
                response: ServerResponse::TerminalCloseChecked {
                    terminal_id,
                    outcome: crate::terminal_ipc::TerminalCloseResult::Closed,
                }
            } if terminal_id == id));
        }
        let mut codec = FrameCodec::new(MAX_FRAME_BYTES);
        while let Some(envelope) = codec.read::<ServerEnvelope>(&mut stream).unwrap() {
            assert_eq!(envelope.request_id, None);
            assert!(matches!(envelope.message, ServerMessage::HostShuttingDown));
        }
        serving.join().unwrap();
        assert!(host.lock().unwrap().clients.is_empty());
        assert!(!host.lock().unwrap().ordinary_exit_cleanup_blocked());
    }
}

fn terminal_presence(id: TerminalId) -> crate::notifications::Presence {
    crate::notifications::Presence {
        application: crate::notifications::Application::Cagents,
        window: "retention-client".into(),
        focused_target: None,
        sessions: vec![id],
        focused: None,
    }
}

/// A Close reserves the terminal while its pane stays open, and the app names
/// every open pane in its presence. A pane with no terminal here reserves
/// nothing, so closing one that's proven gone can't be refused this way.
#[test]
fn notification_presence_is_refused_only_for_a_terminal_mid_close() {
    let (host, _directory, session, handle) = retention_client_host("presence-close");
    let id = session.id;
    let presence = |id| ClientRequest::NotificationPresence {
        presence: terminal_presence(id),
    };
    process_request(&host, 1, presence(TerminalId::new())).unwrap();
    host.lock()
        .unwrap()
        .terminal_retirements
        .insert(id, super::super::state::Retirement::explicit_close(handle));
    let error = process_request(&host, 1, presence(id)).unwrap_err();
    assert_eq!(error.code, ProtocolErrorCode::TerminalUnavailable);
    assert!(error.retryable);
}

#[test]
fn notification_presence_reservation_fences_durable_pins_and_shutdown() {
    let (host, directory, session, handle) = retention_client_host("presence-retention");
    let id = session.id;
    {
        let mut state = host.lock().unwrap();
        state
            .terminal_retirements
            .insert(id, super::super::state::Retirement::submitting(handle));
    }
    let registry = super::super::agent_execution::registry(&host).unwrap();
    for watch in [false, true] {
        let request = if watch {
            ClientRequest::WatchNotifications {
                presence: terminal_presence(id),
            }
        } else {
            ClientRequest::NotificationPresence {
                presence: terminal_presence(id),
            }
        };
        let error = process_request(&host, 1, request).unwrap_err();
        assert_eq!(error.code, ProtocolErrorCode::TerminalUnavailable);
        assert!(error.retryable);
        assert!(
            registry
                .lock()
                .unwrap()
                .retention_pin_generation(id)
                .unwrap()
                .is_none()
        );
    }
    for request in [
        ClientRequest::Shutdown,
        ClientRequest::ShutdownV1 {
            version: SHUTDOWN_VERSION,
            operation: ShutdownOperation::Preflight,
        },
        ClientRequest::UpgradeImpact {
            policy: crate::update::impact::Policy::Interrupt,
        },
    ] {
        let error = process_request(&host, 1, request).unwrap_err();
        assert!(error.retryable && error.message.contains("pending"));
    }
    host.lock()
        .unwrap()
        .terminal_retirements
        .get_mut(&id)
        .unwrap()
        .failure = Some("retained exact owner".into());
    let error = process_request(&host, 1, ClientRequest::Shutdown).unwrap_err();
    assert!(error.retryable && error.message.contains("unconfirmed"));
    assert!(!host.lock().unwrap().shutting_down);
    // Failed cleanup remains conservatively claimable for presentation;
    // only the in-flight cleanup reservation refuses a new saved claim.
    process_request(
        &host,
        1,
        ClientRequest::NotificationPresence {
            presence: terminal_presence(id),
        },
    )
    .unwrap();
    assert!(
        registry
            .lock()
            .unwrap()
            .retention_pin_generation(id)
            .unwrap()
            .is_some()
    );
    let mut editor = terminal_presence(id);
    editor.application = crate::notifications::Application::Ceditor;
    assert!(
        process_request(
            &host,
            1,
            ClientRequest::NotificationPresence { presence: editor }
        )
        .is_err()
    );
    process_request(
        &host,
        1,
        ClientRequest::ShutdownTerminal { terminal_id: id },
    )
    .unwrap();
    let state = host.lock().unwrap();
    assert!(!state.terminals.contains_key(&id));
    assert!(!state.terminal_retirements.contains_key(&id));
    drop(state);
    assert!(
        registry
            .lock()
            .unwrap()
            .retention_pin_generation(id)
            .unwrap()
            .is_none()
    );
    let (_, missing_registry) = panic_test_host("presence-without-registry");
    missing_registry.lock().unwrap().register_client(1);
    assert!(
        process_request(
            &missing_registry,
            1,
            ClientRequest::NotificationPresence {
                presence: terminal_presence(TerminalId::new()),
            }
        )
        .is_ok(),
        "unavailable retention storage must not break the conservative old notification path"
    );
    assert!(
        super::super::model::Registry::at(&directory.path().join("model.sqlite3"))
            .unwrap()
            .retention_pin_generation(id)
            .unwrap()
            .is_none()
    );
}

#[test]
fn close_waits_beside_other_clients_input_instead_of_ahead_of_it() {
    let (host, _directory, session, handle) = retention_client_host("close-fence");
    let id = session.id;
    process_request(
        &host,
        1,
        ClientRequest::NotificationPresence {
            presence: terminal_presence(id),
        },
    )
    .unwrap();
    let pin = retention_pin_generation(&host, id);
    assert!(pin.as_ref().unwrap().is_some());
    handle.shutdown().unwrap();
    let gate = control_gate(&host);
    let registry = super::super::agent_execution::registry(&host).unwrap();
    let input_can_enter = || {
        (0..500).any(|_| {
            thread::sleep(Duration::from_millis(1));
            gate.try_shared().is_some()
        })
    };
    let slow_request = gate.shared();
    let worker = {
        let (host, handle) = (host.clone(), handle.clone());
        thread::spawn(move || finish_test_terminal_close(&host, id, &handle, pin))
    };
    thread::sleep(Duration::from_millis(50));
    assert!(
        input_can_enter(),
        "close queued ahead of other clients' input"
    );
    let slow_commit = registry.lock().unwrap();
    drop(slow_request);
    thread::sleep(Duration::from_millis(50));
    assert!(
        input_can_enter(),
        "close held the writer while another thread committed"
    );
    drop(slow_commit);
    worker.join().unwrap().unwrap();
    assert!(!host.lock().unwrap().terminals.contains_key(&id));
    assert_eq!(retention_pin_generation(&host, id), Ok(None));
}

#[test]
fn a_close_refused_after_removing_its_pin_can_retry() {
    let (host, _directory, session, handle) = retention_client_host("close-refused");
    let id = session.id;
    process_request(
        &host,
        1,
        ClientRequest::NotificationPresence {
            presence: terminal_presence(id),
        },
    )
    .unwrap();
    let pin = retention_pin_generation(&host, id);
    handle.shutdown().unwrap();
    let gate = control_gate(&host);
    let slow_request = gate.shared();
    let refusal = close_fence(&host, &gate, id, Instant::now()).unwrap_err();
    assert_eq!(refusal.code, ProtocolErrorCode::TerminalUnavailable);
    assert!(refusal.retryable);
    drop(slow_request);

    let source = host.lock().unwrap().terminals[&id].session.clone();
    let close =
        reserve_terminal_close(&host, id, &handle, &source, None, pin.clone(), None).unwrap();
    super::super::agent_execution::registry(&host)
        .unwrap()
        .lock()
        .unwrap()
        .forget_retention_pin(id, pin.as_ref().unwrap().as_ref().unwrap())
        .unwrap();
    drop(close);
    assert!(host.lock().unwrap().terminals.contains_key(&id));

    finish_test_terminal_close(&host, id, &handle, Ok(None)).unwrap();
    assert!(!host.lock().unwrap().terminals.contains_key(&id));
}

#[test]
fn republishing_a_window_writes_only_new_or_restored_claims() {
    let (host, _directory, session, _handle) = retention_client_host("republish");
    let id = session.id;
    host.lock().unwrap().register_client(7);
    process_request(
        &host,
        7,
        ClientRequest::WatchNotifications {
            presence: terminal_presence(id),
        },
    )
    .unwrap();
    let publish = |presence| {
        process_request(&host, 1, ClientRequest::NotificationPresence { presence }).unwrap();
    };
    let claimed = retention_pin_generation(&host, id);
    assert!(claimed.as_ref().unwrap().is_some());
    publish(terminal_presence(id));
    assert_eq!(retention_pin_generation(&host, id), claimed);

    // Adding a card leaves the existing claim, so its close can proceed,
    // and dropping that card again claims nothing new.
    let other = TerminalId::new();
    let mut both = terminal_presence(id);
    both.sessions.push(other);
    publish(both);
    assert_eq!(retention_pin_generation(&host, id), claimed);
    assert!(retention_pin_generation(&host, other).unwrap().is_some());
    publish(terminal_presence(id));
    assert_eq!(retention_pin_generation(&host, id), claimed);

    // A close removed this claim while the window still reports the card.
    super::super::agent_execution::registry(&host)
        .unwrap()
        .lock()
        .unwrap()
        .forget_retention_pin(id, claimed.as_ref().unwrap().as_ref().unwrap())
        .unwrap();
    publish(terminal_presence(id));
    let restored = retention_pin_generation(&host, id);
    assert!(restored.as_ref().unwrap().is_some());
    assert_ne!(restored, claimed);
}

#[test]
fn explicit_completed_close_cannot_forget_a_republished_pin_or_fake_missing_cleanup() {
    let (host, _directory, mut session, handle) = retention_client_host("close-republication");
    let id = session.id;
    process_request(
        &host,
        1,
        ClientRequest::NotificationPresence {
            presence: terminal_presence(id),
        },
    )
    .unwrap();
    let closing_generation = retention_pin_generation(&host, id);
    assert!(closing_generation.as_ref().unwrap().is_some());
    handle.shutdown().unwrap();
    // An already-admitted presence can publish after its initial Host
    // check. Final compaction must drain this real reader before deciding.
    let gate = control_gate(&host);
    let publication = gate.shared();
    host.lock()
        .unwrap()
        .ensure_retention_available(&[id])
        .unwrap();
    let worker_host = host.clone();
    let worker_handle = handle.clone();
    let worker_source = host.lock().unwrap().terminals[&id].session.clone();
    let worker_generation = closing_generation.clone();
    let (started, start) = crossbeam_channel::bounded(1);
    let (finished, finish) = crossbeam_channel::bounded(1);
    let worker = thread::spawn(move || {
        started.send(()).unwrap();
        assert!(
            finished
                .send(reserve_terminal_close(
                    &worker_host,
                    id,
                    &worker_handle,
                    &worker_source,
                    None,
                    worker_generation,
                    None,
                ))
                .is_ok()
        );
    });
    start.recv_timeout(Duration::from_secs(2)).unwrap();
    assert!(
        matches!(
            finish.recv_timeout(Duration::from_millis(100)),
            Err(crossbeam_channel::RecvTimeoutError::Timeout)
        ),
        "compaction bypassed admitted ownership publication"
    );
    super::super::agent_execution::registry(&host)
        .unwrap()
        .lock()
        .unwrap()
        .pin_window_terminals(&[id])
        .unwrap();
    let replacement_generation = retention_pin_generation(&host, id);
    assert_ne!(closing_generation, replacement_generation);
    drop(publication);
    assert!(
        finish
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .is_err()
    );
    worker.join().unwrap();
    assert!(host.lock().unwrap().terminals.contains_key(&id));
    assert_eq!(retention_pin_generation(&host, id), replacement_generation);
    // Retry captures the newly published generation only after the old
    // attempt truthfully retained both record and newer saved membership.
    let latest = retention_pin_generation(&host, id);
    finish_test_terminal_close(&host, id, &handle, latest).unwrap();
    assert!(!host.lock().unwrap().terminals.contains_key(&id));
    session.continuity = crate::model::TerminalContinuity::Exited {
        last_exit: crate::model::TerminalExit {
            code: Some(0),
            signal: None,
            occurred_at_unix_ms: 1,
        },
    };
    let error = process_request(
        &host,
        1,
        ClientRequest::ReleaseCompletedTerminal {
            session,
            allow_active: true,
        },
    )
    .unwrap_err();
    assert_eq!(error.code, ProtocolErrorCode::TerminalUnavailable);
    assert!(error.retryable);
    assert_eq!(retention_pin_generation(&host, id), Ok(None));
    let (ended_host, _ended_directory, ended, _) = retention_client_host("checked-ended-history");
    assert!(
        matches!(
            process_request(
                &ended_host,
                1,
                ClientRequest::CloseTerminalChecked {
                    terminal_id: ended.id,
                    allow_active: false,
                }
            )
            .unwrap()
            .0,
            ServerResponse::TerminalCloseChecked {
                outcome: crate::terminal_ipc::TerminalCloseResult::Closed,
                ..
            }
        ),
        "completed history still has a real close/join path"
    );
    assert!(!ended_host.lock().unwrap().terminals.contains_key(&ended.id));
    let (unknown_host, unknown_directory, unknown, _) =
        retention_client_host("unknown-pin-storage");
    let writer =
        rusqlite::Connection::open(unknown_directory.path().join("model.sqlite3")).unwrap();
    writer
        .execute_batch("PRAGMA user_version=2147483647")
        .unwrap();
    let error = process_request(
        &unknown_host,
        1,
        ClientRequest::CloseTerminalChecked {
            terminal_id: unknown.id,
            allow_active: false,
        },
    )
    .unwrap_err();
    assert_eq!(error.code, ProtocolErrorCode::TerminalUnavailable);
    assert!(error.retryable && error.message.contains("ownership was unknown"));
    assert!(
        unknown_host
            .lock()
            .unwrap()
            .terminals
            .contains_key(&unknown.id),
        "unknown pin storage must not be treated as a known absent local pin"
    );
}

#[test]
fn completed_close_preserves_a_replacement_worker_and_exact_native_pair() {
    use crate::chat::protocol::{Adapter, ProcessSpec, Step};
    struct Quiet;
    impl Adapter for Quiet {
        fn initialize(&mut self) -> Result<Step, String> {
            Ok(Step::default())
        }
        fn command(&mut self, _: crate::chat::Command) -> Result<Step, String> {
            Ok(Step::default())
        }
        fn receive(&mut self, _: serde_json::Value) -> Result<Step, String> {
            Ok(Step::default())
        }
    }
    let (host, _directory, session, old) = retention_client_host("close-worker-replacement");
    let id = session.id;
    process_request(
        &host,
        1,
        ClientRequest::NotificationPresence {
            presence: terminal_presence(id),
        },
    )
    .unwrap();
    let old_pin = retention_pin_generation(&host, id);
    let removed = host.lock().unwrap().retire_terminal(id).unwrap();
    drop(removed);
    let mut current = session.clone();
    current.launch.program = "/bin/cat".into();
    let replacement =
        crate::terminal::TerminalHandle::spawn(current.clone(), Default::default(), |_| {})
            .unwrap();
    {
        let mut state = host.lock().unwrap();
        state.reserve_spawn(id).unwrap();
        state
            .finish_spawn(1, current.clone(), replacement.clone(), false)
            .unwrap();
    }
    assert!(terminal_close_pair(&host, id, &old).is_err());
    assert!(finish_test_terminal_close(&host, id, &old, old_pin.clone()).is_err());
    assert!(
        host.lock().unwrap().terminals[&id]
            .handle
            .same_worker(&replacement)
    );
    assert!(!replacement.is_completed());
    assert_eq!(retention_pin_generation(&host, id), old_pin);

    let mut initial = Step::default();
    let spec = ProcessSpec::from(&current.launch);
    let store = crate::chat::runtime::preflight(&spec, &mut initial).unwrap();
    let actor = crate::chat::runtime::Handle::start(
        spec,
        Box::new(Quiet),
        initial,
        store,
        Some("pair-one".into()),
        |_| {},
    )
    .unwrap();
    {
        let mut state = host.lock().unwrap();
        let proof = state.proof_handle().as_ref().clone();
        state.chats.insert(
            id,
            super::super::chat::Record {
                execution_instance: uuid::Uuid::new_v4(),
                session: crate::chat::Session {
                    id,
                    proof: Some(proof),
                    terminal: Some(Box::new(current.clone())),
                    launch: crate::chat::Launch {
                        provider: crate::agent::Provider::Codex,
                        recipe: current.launch.clone(),
                        session_id: Some("pair-one".into()),
                    },
                    native_continuity: Default::default(),
                    owned_codex_listener: None,
                },
                console_recipe: None,
                paired_terminal: Some(replacement.clone()),
                handle: actor.clone(),
                source_process: None,
                subscribers: Default::default(),
                observations: Default::default(),
                completion_observed: false,
                returning_terminal: false,
            },
        );
    }
    let captured = terminal_close_pair(&host, id, &replacement)
        .unwrap()
        .unwrap();
    let spec = ProcessSpec::from(&current.launch);
    let mut initial = Step::default();
    let store = crate::chat::runtime::preflight(&spec, &mut initial).unwrap();
    let next_actor = crate::chat::runtime::Handle::start(
        spec,
        Box::new(Quiet),
        initial,
        store,
        Some("pair-two".into()),
        |_| {},
    )
    .unwrap();
    {
        let mut state = host.lock().unwrap();
        let record = state.chats.get_mut(&id).unwrap();
        record.handle = next_actor.clone();
        record.execution_instance = uuid::Uuid::new_v4();
    }
    super::super::chat::close_terminal_actor(&host, id, &captured).unwrap();
    assert!(
        !next_actor.is_completed(),
        "old receipt must not close a replacement actor"
    );
    assert!(finish_test_terminal_close(&host, id, &replacement, old_pin.clone()).is_err());
    assert!(
        host.lock().unwrap().terminals[&id]
            .handle
            .same_worker(&replacement)
    );
    assert_eq!(retention_pin_generation(&host, id), old_pin);
    let removed = host.lock().unwrap().chats.remove(&id).unwrap();
    drop(removed);
    actor.close().unwrap();
    next_actor.close().unwrap();
    assert!(matches!(
        retry_retirement_close(&host, || ClientRequest::CloseTerminalChecked {
            terminal_id: id,
            allow_active: true,
        })
        .0,
        ServerResponse::TerminalCloseChecked {
            outcome: crate::terminal_ipc::TerminalCloseResult::Closed,
            ..
        }
    ));
    assert!(!host.lock().unwrap().terminals.contains_key(&id));
    assert!(retention_pin_generation(&host, id).unwrap().is_none());
}

#[test]
fn source_checked_close_cannot_retarget_a_completed_replacement_after_basis_read() {
    let (host, _directory, mut session, original) = retention_client_host("source-close-race");
    let id = session.id;
    session.continuity = crate::model::TerminalContinuity::Exited {
        last_exit: crate::model::TerminalExit {
            code: Some(0),
            signal: None,
            occurred_at_unix_ms: 1,
        },
    };
    let basis = host
        .lock()
        .unwrap()
        .terminal_removal_basis(&session)
        .unwrap();
    // B publishes after the metadata reply and before the destructive RPC.
    let replacement =
        crate::terminal::TerminalHandle::spawn(session.clone(), Default::default(), |_| {})
            .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !replacement.is_completed() {
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(5));
    }
    {
        let mut state = host.lock().unwrap();
        drop(state.retire_terminal(id));
    }
    let absent = process_request(
        &host,
        1,
        ClientRequest::CloseTerminalSource {
            session: session.clone(),
            basis: basis.clone(),
            completed_only: true,
            allow_active: false,
            capture: None,
        },
    )
    .unwrap_err();
    assert_eq!(absent.code, ProtocolErrorCode::TerminalNotFound);
    assert_eq!(
        absent.message,
        crate::terminal_ipc::TERMINAL_CLOSE_SOURCE_ABSENT
    );
    {
        let mut state = host.lock().unwrap();
        state.reserve_spawn(id).unwrap();
        state
            .finish_spawn(1, session.clone(), replacement.clone(), false)
            .unwrap();
    }
    for completed_only in [false, true] {
        for allow_active in [false, true] {
            let error = process_request(
                &host,
                1,
                ClientRequest::CloseTerminalSource {
                    session: session.clone(),
                    basis: basis.clone(),
                    completed_only,
                    allow_active,
                    capture: None,
                },
            )
            .unwrap_err();
            assert_eq!(error.code, ProtocolErrorCode::InvalidRequest);
            assert_eq!(
                error.message,
                crate::terminal_ipc::TERMINAL_CLOSE_SOURCE_CONFLICT
            );
            let state = host.lock().unwrap();
            assert!(state.terminals[&id].handle.same_worker(&replacement));
            assert!(!state.terminal_retirements.contains_key(&id));
        }
    }
    let current = host
        .lock()
        .unwrap()
        .terminal_removal_basis(&session)
        .unwrap();
    retry_retirement_close(&host, || ClientRequest::CloseTerminalSource {
        session: session.clone(),
        basis: current.clone(),
        completed_only: true,
        allow_active: false,
        capture: None,
    });
    assert!(!host.lock().unwrap().terminals.contains_key(&id));
    original.shutdown().unwrap();
    replacement.shutdown().unwrap();
}

#[test]
fn explicit_close_retains_registered_original_until_actual_holder_receipt() {
    use std::os::unix::{
        ffi::{OsStrExt, OsStringExt},
        fs::PermissionsExt,
    };
    const CHILD: &str = "CAGENTS_EXPLICIT_RECEIPT_CHILD";
    const AUTH_PRESENT: &str = "CAGENTS_EXPLICIT_RECEIPT_AUTH_PRESENT";
    let name = thread::current().name().unwrap().to_owned();
    if std::env::var(CHILD).as_deref() != Ok(name.as_str()) {
        let private = tempfile::Builder::new().prefix("cr-").tempdir().unwrap();
        std::fs::set_permissions(private.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let stdout = private.path().join("stdout.log");
        let stderr = private.path().join("stderr.log");
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", &name, "--nocapture", "--test-threads=1"])
            .env(CHILD, &name)
            .env("TMPDIR", private.path())
            .env("XDG_RUNTIME_DIR", private.path())
            .env(
                AUTH_PRESENT,
                std::env::var_os(crate::agent_execution::AUTHORITY_ENV)
                    .is_some()
                    .to_string(),
            )
            .stdout(std::fs::File::create(&stdout).unwrap())
            .stderr(std::fs::File::create(&stderr).unwrap())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(40);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if Instant::now() >= deadline {
                if let Ok(bytes) = std::fs::read(private.path().join("release-obstruction")) {
                    let fault = std::path::PathBuf::from(std::ffi::OsString::from_vec(bytes));
                    if fault.starts_with(private.path()) {
                        let _ = std::fs::remove_dir(fault);
                    }
                }
                let _ = child.kill();
                let _ = child.wait();
                panic!(
                    "explicit receipt child timed out; evidence {}",
                    private.keep().display()
                );
            }
            thread::sleep(Duration::from_millis(5));
        };
        let raw_out = std::fs::read_to_string(stdout).unwrap();
        let raw_err = std::fs::read_to_string(stderr).unwrap();
        print!("{raw_out}");
        eprint!("{raw_err}");
        assert!(
            status.success(),
            "receipt child {status}; evidence {}",
            private.keep().display()
        );
        assert!(raw_out.contains(&name) && raw_out.contains("1 passed; 0 failed"));
        return;
    }
    assert_eq!(
        std::env::var(AUTH_PRESENT).unwrap(),
        std::env::var_os(crate::agent_execution::AUTHORITY_ENV)
            .is_some()
            .to_string()
    );
    let proof = HostSessionProof {
        host_session_id: crate::model::HostSessionId::new(uuid::Uuid::new_v4().to_string()),
        boot_id: crate::daemon::read_boot_id().unwrap(),
        host_incarnation: crate::model::HostIncarnation::new(uuid::Uuid::new_v4().to_string()),
    };
    let paths = crate::daemon::RuntimePaths::discover().unwrap();
    let _lease = crate::daemon::HostLease::acquire(&paths, &proof).unwrap();
    let directory = tempfile::Builder::new().prefix("rp-").tempdir().unwrap();
    let mut recipe = crate::model::LaunchRecipe::shell(directory.path().into()).unwrap();
    recipe.program = "/bin/sh".into();
    recipe.arguments = vec![
        "-c".into(),
        "printf 'exact-registered-close-result'; exit 0".into(),
    ];
    let source = crate::model::TerminalSession {
        id: TerminalId::new(),
        launch: recipe,
        transcript_path: None,
        resume_hint: None,
        owned_codex_listener: None,
        recovery_origin: None,
        predecessor: None,
        continuity: crate::model::TerminalContinuity::DetachedSameBoot {
            host_session_id: proof.host_session_id.clone(),
            boot_id: proof.boot_id.clone(),
            host_incarnation: proof.host_incarnation.clone(),
        },
    };
    let recovery = crate::terminal::holder::Recovery {
        source: source.clone(),
        previous_sources: vec![],
        relaunches: vec![],
        options: Default::default(),
        execution_environment: None,
        execution_recipe: None,
    };
    let handle = crate::terminal::TerminalHandle::spawn(
        source.clone(),
        crate::terminal::TerminalConfig {
            holder_recovery: Some(recovery),
            ..Default::default()
        },
        |_| {},
    )
    .unwrap();
    wait_client_retirement(|| handle.is_completed() && handle.worker_joined_for_test());
    let original = handle.holder_identity().unwrap().clone();
    assert!(original.holder.is_current());
    assert!(!original.root.is_current());
    let host = Arc::new(super::super::state::HostLock::new(
        super::super::state::HostState::new(proof.clone()),
    ));
    {
        let mut state = host.lock().unwrap();
        state.install_model(
            super::super::model::Registry::at(&directory.path().join("model.sqlite3")).unwrap(),
        );
        state.register_client(1);
        state.reserve_spawn(source.id).unwrap();
        state
            .finish_spawn(1, source.clone(), handle.clone(), false)
            .unwrap();
    }
    process_request(
        &host,
        1,
        ClientRequest::NotificationPresence {
            presence: terminal_presence(source.id),
        },
    )
    .unwrap();
    let pin = retention_pin_generation(&host, source.id);
    assert!(pin.as_ref().unwrap().is_some());
    let receipt = paths.directory.join("holders").join(format!(
        "{}-{}-{}.closed",
        original.terminal, original.holder.pid, original.holder.start
    ));
    std::fs::create_dir(&receipt).unwrap();
    struct Obstruction(std::path::PathBuf);
    impl Drop for Obstruction {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir(&self.0);
        }
    }
    let obstruction = Obstruction(receipt.clone());
    let marker = std::env::temp_dir().join("release-obstruction");
    std::fs::write(&marker, receipt.as_os_str().as_bytes()).unwrap();
    let error = process_request(
        &host,
        1,
        ClientRequest::ShutdownTerminal {
            terminal_id: source.id,
        },
    )
    .unwrap_err();
    assert_eq!(error.code, ProtocolErrorCode::TerminalUnavailable);
    assert!(error.retryable);
    assert!(handle.worker_joined_for_test());
    assert!(
        original.holder.is_current(),
        "join must not be mistaken for original holder exit"
    );
    assert!(matches!(
        handle.completed_retirement(&source, &proof),
        crate::terminal::TerminalRetirement::WaitingForHolder(_)
    ));
    assert!(
        host.lock().unwrap().terminals[&source.id]
            .handle
            .same_worker(&handle)
    );
    assert_eq!(retention_pin_generation(&host, source.id), pin);
    // Force the unlocked-probe admission interval and its interrupted
    // request path without starting any provider or installing a test hook.
    let current_source = host.lock().unwrap().terminals[&source.id].session.clone();
    let interrupted = reserve_terminal_close(
        &host,
        source.id,
        &handle,
        &current_source,
        None,
        pin.clone(),
        None,
    )
    .unwrap();
    let mut refused_recipe = source.launch.clone();
    refused_recipe.program = "/missing-retirement-claim-never-started".into();
    let candidate = crate::chat::Session {
        id: TerminalId::new(),
        terminal: Some(Box::new(source.clone())),
        proof: Some(proof.clone()),
        launch: crate::chat::Launch {
            provider: crate::agent::Provider::Claude,
            recipe: refused_recipe,
            session_id: None,
        },
        native_continuity: Default::default(),
        owned_codex_listener: None,
    };
    let error = process_request(
        &host,
        1,
        ClientRequest::ChatStart {
            session: candidate.clone(),
            source: None,
            terminate_source: false,
            execution_environment: Default::default(),
        },
    )
    .unwrap_err();
    assert_eq!(error.code, ProtocolErrorCode::TerminalUnavailable);
    assert!(error.retryable && error.message.contains("completing retirement"));
    drop(interrupted);
    assert!(
        host.lock().unwrap().terminal_retirements[&source.id]
            .failure
            .is_some()
    );
    let error = process_request(
        &host,
        1,
        ClientRequest::ChatStart {
            session: candidate,
            source: None,
            terminate_source: false,
            execution_environment: Default::default(),
        },
    )
    .unwrap_err();
    assert_eq!(error.code, ProtocolErrorCode::TerminalUnavailable);
    assert!(
        error.retryable && error.message.contains("unconfirmed"),
        "retained failure permits inspection/retry, never a new native writer"
    );
    let error = process_request(
        &host,
        1,
        ClientRequest::CloseTerminalChecked {
            terminal_id: source.id,
            allow_active: false,
        },
    )
    .unwrap_err();
    assert!(error.retryable && original.holder.is_current());
    let mut ended = source.clone();
    ended.continuity = crate::model::TerminalContinuity::Exited {
        last_exit: crate::model::TerminalExit {
            code: Some(0),
            signal: None,
            occurred_at_unix_ms: 1,
        },
    };
    assert!(
        process_request(
            &host,
            1,
            ClientRequest::ReleaseCompletedTerminal {
                session: ended.clone(),
                allow_active: false,
            }
        )
        .unwrap_err()
        .retryable
    );
    assert_eq!(retention_pin_generation(&host, source.id), pin);
    drop(obstruction);
    std::fs::remove_file(marker).unwrap();
    wait_client_retirement(|| {
        matches!(
            handle.completed_retirement(&source, &proof),
            crate::terminal::TerminalRetirement::Released
        )
    });
    let exact_receipt = std::fs::read(&receipt).unwrap();
    assert!(!exact_receipt.is_empty());
    let saved_receipt = receipt.with_extension("closed.saved");
    std::fs::rename(&receipt, &saved_receipt).unwrap();
    let error = process_request(
        &host,
        1,
        ClientRequest::ReleaseCompletedTerminal {
            session: ended.clone(),
            allow_active: false,
        },
    )
    .unwrap_err();
    assert!(error.retryable);
    assert!(
        host.lock().unwrap().terminals[&source.id]
            .handle
            .same_worker(&handle)
    );
    assert_eq!(
        retention_pin_generation(&host, source.id),
        pin,
        "kernel exit without the exact receipt must retain the retry surface"
    );
    std::fs::rename(&saved_receipt, &receipt).unwrap();
    assert_eq!(std::fs::read(&receipt).unwrap(), exact_receipt);
    // A separate real WAL writer permits the membership read but refuses
    // our IMMEDIATE deletion for the existing bounded busy timeout.
    let blocked = rusqlite::Connection::open(directory.path().join("model.sqlite3")).unwrap();
    blocked.execute_batch("BEGIN IMMEDIATE").unwrap();
    let error = process_request(
        &host,
        1,
        ClientRequest::ReleaseCompletedTerminal {
            session: ended.clone(),
            allow_active: false,
        },
    )
    .unwrap_err();
    assert_eq!(error.code, ProtocolErrorCode::TerminalUnavailable);
    assert!(error.retryable && error.message.contains("pin could not be removed"));
    assert!(
        host.lock().unwrap().terminals[&source.id]
            .handle
            .same_worker(&handle)
    );
    assert_eq!(retention_pin_generation(&host, source.id), pin);
    assert!(
        host.lock().unwrap().terminal_retirements[&source.id]
            .failure
            .is_some()
    );
    blocked.execute_batch("ROLLBACK").unwrap();
    assert!(matches!(
        process_request(
            &host,
            1,
            ClientRequest::ReleaseCompletedTerminal {
                session: ended,
                allow_active: false,
            }
        )
        .unwrap()
        .0,
        ServerResponse::TerminalCloseChecked {
            outcome: crate::terminal_ipc::TerminalCloseResult::Closed,
            ..
        }
    ));
    assert!(!host.lock().unwrap().terminals.contains_key(&source.id));
    assert!(
        !host
            .lock()
            .unwrap()
            .terminal_retirements
            .contains_key(&source.id)
    );
    assert_eq!(retention_pin_generation(&host, source.id), Ok(None));
    wait_client_retirement(|| {
        !original.holder.is_current() && !crate::daemon::child_reaper::has_outstanding_ownership()
    });
    assert_eq!(std::fs::read(receipt).unwrap(), exact_receipt);
}

#[test]
fn resident_release_drains_admitted_control_operations_before_retirement() {
    let (proof, host) = panic_test_host("resident-release-gate");
    let gate = control_gate(&host);
    // Model an admitted submission between authentication and acceptance.
    let admitted = gate.shared();
    let release = crate::terminal_ipc::ChatReleaseProof {
        session: crate::chat::Session {
            id: TerminalId::new(),
            proof: Some(proof),
            terminal: None,
            native_continuity: Default::default(),
            owned_codex_listener: None,
            launch: crate::chat::Launch {
                provider: crate::agent::Provider::Claude,
                session_id: None,
                recipe: crate::model::LaunchRecipe {
                    program: "/missing-never-started".into(),
                    arguments: vec![],
                    working_directory: "/fixture".into(),
                    environment: Default::default(),
                    unset_environment: Default::default(),
                },
            },
        },
        execution_instance: uuid::Uuid::new_v4(),
        generation: 0,
    };
    // Inspection stays a shared operation and promptly reports absence.
    assert!(
        process_request(
            &host,
            1,
            ClientRequest::ChatRelease {
                proof: release.clone(),
                dry_run: true
            }
        )
        .is_err()
    );
    let worker_host = host.clone();
    let (started, ready) = crossbeam_channel::bounded(1);
    let (finished, result) = crossbeam_channel::bounded(1);
    let worker = thread::spawn(move || {
        started.send(()).unwrap();
        finished
            .send(process_request(
                &worker_host,
                1,
                ClientRequest::ChatRelease {
                    proof: release,
                    dry_run: false,
                },
            ))
            .unwrap();
    });
    ready.recv_timeout(Duration::from_secs(2)).unwrap();
    assert!(
        matches!(
            result.recv_timeout(Duration::from_millis(100)),
            Err(crossbeam_channel::RecvTimeoutError::Timeout)
        ),
        "mutating release crossed an admitted operation before its acceptance completed"
    );
    drop(admitted);
    assert!(
        result
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .is_err()
    );
    worker.join().unwrap();
}

#[test]
fn completed_release_refuses_live_or_changed_sessions() {
    use crate::model::{
        BootId, HostIncarnation, HostSessionId, LaunchRecipe, TerminalExit, TerminalSession,
    };
    let proof = HostSessionProof {
        host_session_id: HostSessionId::new("completed-release"),
        boot_id: BootId::new("boot"),
        host_incarnation: HostIncarnation::new("incarnation"),
    };
    let host = Arc::new(crate::host::state::HostLock::new(
        super::super::state::HostState::new(proof),
    ));
    let model = tempfile::tempdir().unwrap();
    host.lock().unwrap().install_model(
        super::super::model::Registry::at(&model.path().join("model.sqlite3")).unwrap(),
    );
    let mut recipe = LaunchRecipe::shell("/".into()).unwrap();
    recipe.program = "/bin/cat".into();
    recipe.arguments.clear();
    let mut session = TerminalSession {
        id: TerminalId::new(),
        launch: recipe.clone(),
        owned_codex_listener: None,
        recovery_origin: None,
        predecessor: None,
        transcript_path: None,
        resume_hint: None,
        continuity: crate::model::TerminalContinuity::RelaunchAvailable {
            recipe,
            last_exit: None,
        },
    };
    let handle = crate::terminal::TerminalHandle::spawn(
        session.clone(),
        crate::terminal::TerminalConfig::default(),
        |_| {},
    )
    .unwrap();
    {
        let mut state = host.lock().unwrap();
        state.register_client(1);
        state.reserve_spawn(session.id).unwrap();
        state
            .finish_spawn(1, session.clone(), handle.clone(), true)
            .unwrap();
    }
    session.continuity = crate::model::TerminalContinuity::Exited {
        last_exit: TerminalExit {
            code: Some(0),
            signal: None,
            occurred_at_unix_ms: 0,
        },
    };
    assert!(
        process_request(
            &host,
            1,
            ClientRequest::ReleaseCompletedTerminal {
                session: session.clone(),
                allow_active: false,
            }
        )
        .is_err()
    );
    assert!(
        !handle.is_completed(),
        "a forged completed receipt cannot stop a running process"
    );
    handle.shutdown().unwrap();
    let mut changed = session.clone();
    changed.launch.arguments.push("different".into());
    assert!(
        process_request(
            &host,
            1,
            ClientRequest::ReleaseCompletedTerminal {
                session: changed,
                allow_active: false
            }
        )
        .is_err()
    );
    let _ = retry_retirement_close(&host, || ClientRequest::ReleaseCompletedTerminal {
        session: session.clone(),
        allow_active: false,
    });
}

#[test]
fn completed_terminal_requires_confirmation_for_its_exact_native_actor() {
    assert_completed_native_confirmation(crate::agent::Provider::Codex);
}

#[test]
fn completed_console_requires_confirmation_for_its_exact_native_actor() {
    assert_completed_native_confirmation(crate::agent::Provider::Claude);
}

fn assert_completed_native_confirmation(provider: crate::agent::Provider) {
    for completed_only in [true, false] {
        assert_completed_native_confirmation_case(provider, completed_only);
    }
}

fn assert_completed_native_confirmation_case(
    provider: crate::agent::Provider,
    completed_only: bool,
) {
    use crate::{
        chat::{
            self,
            protocol::{Adapter, ProcessSpec, Step},
        },
        model::{
            BootId, HostIncarnation, HostSessionId, TerminalContinuity, TerminalExit,
            TerminalSession,
        },
    };
    struct Quiet;
    impl Adapter for Quiet {
        fn initialize(&mut self) -> Result<Step, String> {
            Ok(Step::default())
        }
        fn command(&mut self, _: chat::Command) -> Result<Step, String> {
            Ok(Step::default())
        }
        fn receive(&mut self, _: serde_json::Value) -> Result<Step, String> {
            Ok(Step::default())
        }
    }
    let proof = HostSessionProof {
        host_session_id: HostSessionId::new("paired-completion"),
        boot_id: BootId::new("boot"),
        host_incarnation: HostIncarnation::new("incarnation"),
    };
    let host = Arc::new(crate::host::state::HostLock::new(
        super::super::state::HostState::new(proof.clone()),
    ));
    let model = tempfile::tempdir().unwrap();
    host.lock().unwrap().install_model(
        super::super::model::Registry::at(&model.path().join("model.sqlite3")).unwrap(),
    );
    let mut recipe = crate::model::LaunchRecipe::shell("/".into()).unwrap();
    recipe.program = "/bin/true".into();
    recipe.arguments.clear();
    let session = TerminalSession {
        id: TerminalId::new(),
        launch: recipe.clone(),
        owned_codex_listener: None,
        recovery_origin: None,
        predecessor: None,
        transcript_path: None,
        resume_hint: None,
        continuity: TerminalContinuity::Exited {
            last_exit: TerminalExit {
                code: Some(0),
                signal: None,
                occurred_at_unix_ms: 1,
            },
        },
    };
    let terminal =
        crate::terminal::TerminalHandle::spawn(session.clone(), Default::default(), |_| {})
            .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !terminal.is_completed() {
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(5));
    }
    recipe.program = "/bin/cat".into();
    let spec = ProcessSpec::from(&recipe);
    let mut initial = Step::default();
    let store = chat::runtime::preflight(&spec, &mut initial).unwrap();
    let actor = chat::runtime::Handle::start(
        spec,
        Box::new(Quiet),
        initial,
        store,
        Some("exact-thread".into()),
        |_| {},
    )
    .unwrap();
    {
        let mut state = host.lock().unwrap();
        state.register_client(1);
        state.reserve_spawn(session.id).unwrap();
        state
            .finish_spawn(1, session.clone(), terminal.clone(), false)
            .unwrap();
        state.chats.insert(
            session.id,
            super::super::chat::Record {
                execution_instance: uuid::Uuid::new_v4(),
                console_recipe: (provider != crate::agent::Provider::Codex)
                    .then(|| session.launch.clone()),
                paired_terminal: Some(terminal),
                session: chat::Session {
                    id: session.id,
                    native_continuity: Default::default(),
                    owned_codex_listener: None,
                    proof: Some(proof),
                    terminal: Some(Box::new(session.clone())),
                    launch: chat::Launch {
                        provider,
                        recipe,
                        session_id: Some("exact-thread".into()),
                    },
                },
                handle: actor.clone(),
                source_process: None,
                subscribers: Default::default(),
                observations: Default::default(),
                completion_observed: false,
                returning_terminal: false,
            },
        );
    }
    let basis = host
        .lock()
        .unwrap()
        .terminal_removal_basis(&session)
        .unwrap();
    let request = |allow_active| ClientRequest::CloseTerminalSource {
        session: session.clone(),
        basis: basis.clone(),
        completed_only,
        allow_active,
        capture: None,
    };
    assert!(matches!(
        process_request(&host, 1, request(false)).unwrap().0,
        ServerResponse::TerminalCloseChecked {
            outcome: crate::terminal_ipc::TerminalCloseResult::Active,
            ..
        }
    ));
    assert!(
        !actor.is_completed(),
        "a completed PTY does not authorize stopping its native actor"
    );
    assert!(
        !host
            .lock()
            .unwrap()
            .notification_closed
            .contains(&session.id)
    );
    host.lock()
        .unwrap()
        .chats
        .get_mut(&session.id)
        .unwrap()
        .session
        .terminal
        .as_mut()
        .unwrap()
        .launch
        .arguments
        .push("changed".into());
    assert!(process_request(&host, 1, request(true)).is_err());
    assert!(!actor.is_completed());
    host.lock()
        .unwrap()
        .chats
        .get_mut(&session.id)
        .unwrap()
        .session
        .terminal = Some(Box::new(session.clone()));
    assert!(matches!(
        retry_retirement_close(&host, || request(true)).0,
        ServerResponse::TerminalCloseChecked {
            outcome: crate::terminal_ipc::TerminalCloseResult::Closed,
            ..
        }
    ));
    assert!(actor.is_completed());
    assert!(!host.lock().unwrap().chats.contains_key(&session.id));
    let state = host.lock().unwrap();
    assert!(!state.terminals.contains_key(&session.id));
    assert!(!state.notification_closed.contains(&session.id));
}

#[test]
pub(in crate::host) fn slow_chat_close_allows_list_but_fences_upgrade_and_shutdown() {
    if !super::super::state::tests::upgrade_fixture_in_fresh_process() {
        return;
    }
    use crate::{
        agent::Provider,
        chat::{
            self, Command,
            protocol::{Adapter, ProcessSpec, Step},
            runtime::Handle,
        },
        model::{BootId, HostIncarnation, HostSessionId, LaunchRecipe},
    };
    use std::collections::BTreeMap;

    struct DelayedCommand {
        entered: crossbeam_channel::Sender<()>,
        release: crossbeam_channel::Receiver<()>,
    }
    impl Adapter for DelayedCommand {
        fn initialize(&mut self) -> Result<Step, String> {
            Ok(Step::default())
        }
        fn command(&mut self, _: Command) -> Result<Step, String> {
            self.entered.send(()).map_err(|error| error.to_string())?;
            self.release
                .recv_timeout(Duration::from_secs(5))
                .map_err(|error| error.to_string())?;
            Ok(Step::default())
        }
        fn receive(&mut self, _: serde_json::Value) -> Result<Step, String> {
            Ok(Step::default())
        }
    }

    let directory = tempfile::tempdir().unwrap();
    let proof = HostSessionProof {
        host_session_id: HostSessionId::new("control-barrier"),
        boot_id: BootId::new("boot"),
        host_incarnation: HostIncarnation::new("incarnation"),
    };
    let host = Arc::new(crate::host::state::HostLock::new(
        super::super::state::HostState::new(proof.clone()),
    ));
    let recipe = LaunchRecipe {
        unset_environment: Default::default(),
        program: "/bin/cat".into(),
        arguments: Vec::new(),
        working_directory: directory.path().to_path_buf(),
        environment: BTreeMap::new(),
    };
    let spec = ProcessSpec::from(&recipe);
    let mut initial = Step::default();
    let store = chat::runtime::preflight(&spec, &mut initial).unwrap();
    let (entered, pending) = crossbeam_channel::bounded(1);
    let (release, released) = crossbeam_channel::bounded(1);
    let handle = Handle::start(
        spec,
        Box::new(DelayedCommand {
            entered,
            release: released,
        }),
        initial,
        store,
        None,
        |_| {},
    )
    .unwrap();
    let chat_id = TerminalId::new();
    let capability = {
        let mut state = host.lock().unwrap();
        state.register_client(1);
        state.register_client(2);
        state.chats.insert(
            chat_id,
            super::super::chat::Record {
                execution_instance: uuid::Uuid::nil(),
                console_recipe: None,
                paired_terminal: None,
                session: chat::Session {
                    native_continuity: Default::default(),
                    owned_codex_listener: None,
                    id: chat_id,
                    launch: chat::Launch {
                        provider: Provider::Claude,
                        recipe,
                        session_id: None,
                    },
                    proof: Some(proof),
                    terminal: None,
                },
                handle: handle.clone(),
                source_process: None,
                subscribers: BTreeMap::from([(1, false)]),
                observations: BTreeMap::new(),
                completion_observed: false,
                returning_terminal: false,
            },
        );
        state.shutdown_preflight(Instant::now()).unwrap().capability
    };
    let command_handle = handle.clone();
    let command = thread::spawn(move || command_handle.command(Command::Interrupt));
    pending.recv_timeout(Duration::from_secs(2)).unwrap();
    let close_host = Arc::clone(&host);
    let close = thread::spawn(move || {
        process_request(&close_host, 1, ClientRequest::ChatClose { chat_id })
    });
    let gate = control_gate(&host);
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match gate.try_exclusive() {
            None => break,
            Some(guard) => drop(guard),
        }
        assert!(Instant::now() < deadline, "chat close was not admitted");
        thread::yield_now();
    }

    // The actual close is joining the blocked provider worker. A separate
    // client's ordinary request must complete before that worker is released.
    let list_host = Arc::clone(&host);
    let (listed, listing) = crossbeam_channel::bounded(1);
    let list = thread::spawn(move || {
        listed
            .send(process_request(&list_host, 2, ClientRequest::List))
            .unwrap();
    });
    assert!(matches!(
        listing
            .recv_timeout(Duration::from_secs(1))
            .unwrap()
            .unwrap()
            .0,
        ServerResponse::Listed { .. }
    ));
    list.join().unwrap();

    let upgrade_host = Arc::clone(&host);
    let (upgraded, upgrading) = crossbeam_channel::bounded(1);
    let upgrade = thread::spawn(move || {
        upgraded
            .send(process_request(
                &upgrade_host,
                2,
                ClientRequest::PrepareUpgrade {
                    capability,
                    relaunch_agents: false,
                },
            ))
            .unwrap();
    });
    assert!(matches!(
        upgrading.recv_timeout(Duration::from_millis(100)),
        Err(crossbeam_channel::RecvTimeoutError::Timeout)
    ));
    release.send(()).unwrap();
    let _ = command.join().unwrap();
    assert!(matches!(
        close.join().unwrap().unwrap().0,
        ServerResponse::ChatClosed { chat_id: id, .. } if id == chat_id
    ));
    let error = upgrading
        .recv_timeout(Duration::from_secs(2))
        .unwrap()
        .unwrap_err();
    assert!(error.message.contains("inventory changed"));
    upgrade.join().unwrap();
    assert!(!host.lock().unwrap().shutting_down);
    assert!(!host.lock().unwrap().chats.contains_key(&chat_id));

    // A successful exclusive shutdown commits the same admission fence,
    // without persisting an upgrade checkpoint in the user's runtime path.
    assert!(matches!(
        process_request(&host, 2, ClientRequest::Shutdown)
            .unwrap()
            .0,
        ServerResponse::ShutdownAccepted
    ));
    let error = process_request(&host, 2, ClientRequest::List).unwrap_err();
    assert_eq!(error.code, ProtocolErrorCode::TerminalUnavailable);
    assert!(!error.retryable);
    assert!(prepare_resume(&host, 2, Vec::new()).is_err());
}

#[test]
fn daemon_activity_pushes_without_refresh_and_slow_probe_does_not_block_ipc() {
    fn send(
        stream: &mut UnixStream,
        proof: &HostSessionProof,
        request_id: u64,
        request: ClientRequest,
    ) {
        FrameCodec::new(MAX_CLIENT_FRAME_BYTES)
            .write(
                stream,
                &ClientEnvelope {
                    version: PROTOCOL_VERSION,
                    request_id,
                    proof: Some(proof.clone()),
                    request,
                },
            )
            .unwrap();
    }

    fn response(stream: &mut UnixStream, proof: &HostSessionProof) -> ServerEnvelope {
        let mut codec = FrameCodec::new(MAX_FRAME_BYTES);
        loop {
            let envelope = codec.read::<ServerEnvelope>(stream).unwrap().unwrap();
            assert_eq!(&envelope.proof, proof);
            if envelope.request_id.is_some() {
                return envelope;
            }
        }
    }

    fn wait_file(path: &std::path::Path) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while !std::fs::metadata(path).is_ok_and(|metadata| metadata.len() > 0) {
            assert!(
                Instant::now() < deadline,
                "fixture did not start: {}",
                path.display()
            );
            thread::sleep(Duration::from_millis(5));
        }
    }

    struct Cleanup {
        stream: UnixStream,
        server: Option<thread::JoinHandle<()>>,
        handle: Option<crate::terminal::TerminalHandle>,
    }
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = self.stream.shutdown(Shutdown::Both);
            if let Some(handle) = &self.handle {
                let _ = handle.shutdown();
            }
            if let Some(server) = self.server.take() {
                server.join().unwrap();
            }
        }
    }

    let directory = tempfile::tempdir().unwrap();
    let executable = directory.path().join("claude");
    // A real ELF foreground executable with an exact provider basename.
    // The copied shell reads our local `agents` script for the real probe;
    // no installed provider, credentials, network, or model is involved.
    std::fs::copy("/bin/sh", &executable).unwrap();
    std::fs::write(
        directory.path().join("agents"),
        "printf '%s' \"$$\" > probe-started\nexec /bin/sleep 30\n",
    )
    .unwrap();
    let launch = crate::model::LaunchRecipe {
        unset_environment: Default::default(),
        program: executable.to_string_lossy().into_owned(),
        arguments: vec![
            "-c".into(),
            "printf ready > terminal-ready; while IFS= read -r line; do printf '%s\\n' \"$line\"; done".into(),
        ],
        working_directory: directory.path().to_path_buf(),
        environment: [
            ("HOME".into(), directory.path().to_string_lossy().into_owned()),
            ("CLAUDE_CONFIG_DIR".into(), directory.path().to_string_lossy().into_owned()),
            ("PATH".into(), "/usr/bin:/bin".into()),
        ].into_iter().collect(),
    };
    let terminal_id = TerminalId::new();
    let session = crate::model::TerminalSession {
        owned_codex_listener: None,
        recovery_origin: None,
        predecessor: None,
        id: terminal_id,
        launch: launch.clone(),
        transcript_path: None,
        resume_hint: None,
        continuity: crate::model::TerminalContinuity::RelaunchAvailable {
            recipe: launch,
            last_exit: None,
        },
    };
    let proof = HostSessionProof {
        host_session_id: crate::model::HostSessionId::new("slow-probe"),
        boot_id: crate::model::BootId::new("slow-probe"),
        host_incarnation: crate::model::HostIncarnation::new("slow-probe"),
    };
    let host = Arc::new(crate::host::state::HostLock::new(
        super::super::state::HostState::new(proof.clone()),
    ));
    let (mut stream, server_stream) = UnixStream::pair().unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(1)))
        .unwrap();
    stream
        .set_write_timeout(Some(Duration::from_secs(1)))
        .unwrap();
    let server_host = Arc::clone(&host);
    let mut cleanup = Cleanup {
        stream: stream.try_clone().unwrap(),
        server: Some(thread::spawn(move || serve(server_stream, 1, server_host))),
        handle: None,
    };
    send(
        &mut stream,
        &proof,
        1,
        ClientRequest::Connect {
            expected_host: Some(proof.clone()),
            resume: Vec::new(),
        },
    );
    assert!(matches!(
        response(&mut stream, &proof).message,
        ServerMessage::Response {
            response: ServerResponse::Connected { .. }
        }
    ));
    send(
        &mut stream,
        &proof,
        2,
        ClientRequest::Spawn {
            session,
            options: Default::default(),
        },
    );
    assert!(matches!(
        response(&mut stream, &proof).message,
        ServerMessage::Response {
            response: ServerResponse::Spawned { .. }
        }
    ));
    cleanup.handle = Some(host.lock().unwrap().handle(1, terminal_id).unwrap());
    wait_file(&directory.path().join("terminal-ready"));
    // The daemon samples without any refresh request. Process discovery is
    // separately owned by the PTY; force its initial tick for this fixture.
    assert!(
        cleanup
            .handle
            .as_ref()
            .unwrap()
            .observe_agent_process()
            .is_some()
    );
    let monitor = super::super::activity::Monitor::start(&host).unwrap();
    let started = directory.path().join("probe-started");
    wait_file(&started);
    let probe_pid: i32 = std::fs::read_to_string(&started).unwrap().parse().unwrap();
    // Upgrade and shutdown admission never waits for the provider listing.
    let gate = host.lock().unwrap().control_gate.clone();
    assert!(
        gate.try_exclusive().is_some(),
        "a provider listing in flight must not hold the control gate"
    );
    // ...but it holds the lease that account and worktree reservations
    // wait for.
    let probes = host.lock().unwrap().agent_probes.clone();
    assert_eq!(
        probes
            .running()
            .into_iter()
            .map(|(_, probe)| probe.working_directory)
            .collect::<Vec<_>>(),
        [directory.path().to_path_buf()]
    );

    // Compatibility refreshes read the cache immediately; none schedules a
    // probe or creates a per-connection worker. Input shares this socket.
    for request_id in 3..12 {
        send(
            &mut stream,
            &proof,
            request_id,
            ClientRequest::RefreshAgentActivity { terminal_id },
        );
    }
    send(
        &mut stream,
        &proof,
        12,
        ClientRequest::Input {
            terminal_id,
            bytes: b"responsive\n".to_vec(),
        },
    );
    send(
        &mut stream,
        &proof,
        13,
        ClientRequest::Resize {
            terminal_id,
            size: crate::terminal_ipc::WireTerminalSize {
                columns: 90,
                rows: 30,
                cell_width_px: 9,
                cell_height_px: 18,
            },
        },
    );
    send(
        &mut stream,
        &proof,
        14,
        ClientRequest::Focus {
            terminal_id,
            focused: true,
        },
    );
    send(
        &mut stream,
        &proof,
        15,
        ClientRequest::AcknowledgeRedraw {
            terminal_id,
            generation: 0,
        },
    );
    for request_id in 3..16 {
        let envelope = response(&mut stream, &proof);
        assert_eq!(envelope.request_id, Some(request_id));
        assert!(matches!(envelope.message, ServerMessage::Response { .. }));
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    while unsafe { libc::kill(probe_pid, 0) } == 0 {
        assert!(
            Instant::now() < deadline,
            "timed-out daemon probe must be reaped"
        );
        thread::sleep(Duration::from_millis(5));
    }

    // The next observation succeeds after failure backoff, with no client
    // refresh, and arrives as an ordinary semantic event.
    let process = cleanup
        .handle
        .as_ref()
        .unwrap()
        .cached_agent_process()
        .unwrap();
    let rows = serde_json::json!([{
        "kind": "interactive", "pid": process.pid,
        "cwd": directory.path(), "status": "busy", "title": "Activity fixture"
    }]);
    std::fs::write(
        directory.path().join("agents"),
        format!("printf '%s' '{}'\n", rows),
    )
    .unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(8)))
        .unwrap();
    let working = Some(crate::agent::Activity {
        provider: crate::agent::Provider::Claude,
        state: crate::agent::ActivityState::Working,
    });
    let mut codec = FrameCodec::new(MAX_FRAME_BYTES);
    loop {
        let envelope = codec.read::<ServerEnvelope>(&mut stream).unwrap().unwrap();
        if matches!(envelope.message, ServerMessage::Event {
            event: WireTerminalEvent::AgentActivityChanged { activity }, ..
        } if activity == working)
        {
            assert_eq!(envelope.request_id, None);
            break;
        }
    }
    assert_eq!(cleanup.handle.as_ref().unwrap().agent_activity(), working);

    // Closing a GUI is independent of the daemon monitor, and a replacement
    // connection receives current activity in its attach snapshot.
    let handle = cleanup.handle.take();
    drop(cleanup);
    let (mut stream, server_stream) = UnixStream::pair().unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(1)))
        .unwrap();
    let server_host = Arc::clone(&host);
    let cleanup = Cleanup {
        stream: stream.try_clone().unwrap(),
        server: Some(thread::spawn(move || serve(server_stream, 2, server_host))),
        handle,
    };
    send(
        &mut stream,
        &proof,
        1,
        ClientRequest::Connect {
            expected_host: Some(proof.clone()),
            resume: Vec::new(),
        },
    );
    assert!(matches!(
        response(&mut stream, &proof).message,
        ServerMessage::Response {
            response: ServerResponse::Connected { .. }
        }
    ));
    send(
        &mut stream,
        &proof,
        2,
        ClientRequest::Attach {
            terminal_id,
            after_generation: 0,
            after_event_sequence: 0,
        },
    );
    let envelope = response(&mut stream, &proof);
    assert!(matches!(envelope.message, ServerMessage::Response {
        response: ServerResponse::Attached { terminal, .. }
    } if terminal.agent_activity == working));
    drop(cleanup);
    drop(monitor);
    assert!(!host.lock().unwrap().clients.contains_key(&2));
}

#[test]
fn unsupported_shutdown_revision_does_not_mutate_host() {
    let proof = HostSessionProof {
        host_session_id: crate::model::HostSessionId::new("test"),
        boot_id: crate::model::BootId::new("test"),
        host_incarnation: crate::model::HostIncarnation::new("test"),
    };
    let host = Arc::new(crate::host::state::HostLock::new(
        super::super::state::HostState::new(proof),
    ));
    let error = process_request(
        &host,
        1,
        ClientRequest::ShutdownV1 {
            version: SHUTDOWN_VERSION + 1,
            operation: ShutdownOperation::Stop {
                require_empty: false,
                expected_generation: None,
            },
        },
    )
    .unwrap_err();
    assert_eq!(error.code, ProtocolErrorCode::UnsupportedVersion);
    assert!(
        host.lock()
            .unwrap_or_else(|error| error.into_inner())
            .reserve_spawn(crate::model::TerminalId::new())
            .is_ok()
    );
}

fn panic_test_host(name: &str) -> (HostSessionProof, SharedHost) {
    let proof = HostSessionProof {
        host_session_id: crate::model::HostSessionId::new(name),
        boot_id: crate::model::BootId::new(name),
        host_incarnation: crate::model::HostIncarnation::new(name),
    };
    let host = Arc::new(crate::host::state::HostLock::new(
        super::super::state::HostState::new(proof.clone()),
    ));
    (proof, host)
}

#[test]
fn agent_close_reply_drains_before_ordinary_exit_seals_delivery() {
    use crate::agent_execution::{
        Destination, Operation, Origin, Reply, Spec, Submit, Surface, Target,
    };
    use std::sync::mpsc;
    let name = thread::current().name().unwrap().to_owned();
    let Some(_root) = super::super::exit_drain_tests::private_runtime_case(&name, None) else {
        return;
    };
    thread_local! {
        static PAUSE: std::cell::RefCell<Option<(mpsc::Sender<()>, mpsc::Receiver<()>)>> =
            const { std::cell::RefCell::new(None) };
    }
    fn close_then_pause(
        host: &SharedHost,
        client: ClientId,
        request: ClientRequest,
    ) -> Result<RequestOutcome, ProtocolError> {
        let result = process_request(host, client, request);
        if matches!(&result, Ok((ServerResponse::Agent(crate::agent_execution::Reply::Closed(agent)), _, _)) if agent.closed)
        {
            PAUSE.with(|pause| {
                let (closed, resume) = pause.borrow_mut().take().unwrap();
                closed.send(()).unwrap();
                resume.recv_timeout(Duration::from_secs(5)).unwrap();
            });
        }
        result
    }
    fn send(
        stream: &mut UnixStream,
        proof: &HostSessionProof,
        request_id: u64,
        request: ClientRequest,
    ) {
        FrameCodec::new(MAX_CLIENT_FRAME_BYTES)
            .write(
                stream,
                &ClientEnvelope {
                    version: PROTOCOL_VERSION,
                    request_id,
                    proof: Some(proof.clone()),
                    request,
                },
            )
            .unwrap();
    }
    fn reply(stream: &mut UnixStream, request_id: u64) -> ServerMessage {
        let mut codec = FrameCodec::new(MAX_FRAME_BYTES);
        loop {
            let envelope = codec
                .read::<ServerEnvelope>(stream)
                .unwrap()
                .expect("Agent Close reply was lost before delivery drained");
            if envelope.request_id == Some(request_id) {
                return envelope.message;
            }
        }
    }
    let (proof, host) = panic_test_host("agent-close-reply-custody");
    let directory = tempfile::tempdir().unwrap();
    let repository = directory.path().canonicalize().unwrap();
    let mut registry =
        super::super::model::Registry::at(&directory.path().join("model.sqlite3")).unwrap();
    registry
        .apply(&crate::workspace_model::ModelEdit::OpenProject {
            repository: repository.clone(),
            name: "Close fixture".into(),
            kind: crate::model::ProjectKind::Folder,
        })
        .unwrap();
    // Accept a fixture-owned pending run without dispatching any actor.
    // Close still crosses the real origin/ownership checks and durable
    // terminal-state transition, and may release the last run owner.
    let accepted = registry
        .submit_agent(
            &Origin::User,
            &Submit {
                name: None,
                destination: Destination::New(Spec {
                    provider: crate::agent::Provider::Claude,
                    account: None,
                    target: Target {
                        repository: repository.clone(),
                        directory: repository,
                        placement: None,
                    },
                    surface: Surface::Chat,
                    model: None,
                    permission_mode: None,
                    reasoning_effort: None,
                }),
                prompt: "Never dispatched".into(),
                idempotency_key: "close-reply-fixture".into(),
            },
            1,
        )
        .unwrap();
    let agent = accepted.agent.id;
    host.lock().unwrap().install_model(registry);
    let (mut stream, server_stream) = UnixStream::pair().unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let (closed_tx, closed_rx) = mpsc::channel();
    let (resume_tx, resume_rx) = mpsc::channel();
    let serving_host = Arc::clone(&host);
    let serving = thread::spawn(move || {
        PAUSE.with(|pause| *pause.borrow_mut() = Some((closed_tx, resume_rx)));
        serve_with(server_stream, 1, serving_host, close_then_pause);
    });
    send(
        &mut stream,
        &proof,
        1,
        ClientRequest::Connect {
            expected_host: Some(proof.clone()),
            resume: Vec::new(),
        },
    );
    assert!(matches!(
        reply(&mut stream, 1),
        ServerMessage::Response {
            response: ServerResponse::Connected { .. }
        }
    ));
    send(
        &mut stream,
        &proof,
        2,
        ClientRequest::Agent {
            authority: crate::agent_execution::Authority::default(),
            operation: Operation::Close(crate::model::AgentId::new()),
        },
    );
    assert!(matches!(reply(&mut stream, 2), ServerMessage::Error(_)));
    let deadline = Instant::now() + Duration::from_secs(5);
    while host.lock().unwrap().ordinary_exit_cleanup_blocked() {
        assert!(
            Instant::now() < deadline,
            "refused Agent Close leaked reply custody"
        );
        thread::sleep(Duration::from_millis(5));
    }
    send(
        &mut stream,
        &proof,
        3,
        ClientRequest::Agent {
            authority: crate::agent_execution::Authority::default(),
            operation: Operation::Close(agent),
        },
    );
    closed_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    let registry = super::super::agent_execution::registry(&host).unwrap();
    assert!(registry.lock().unwrap().agent(agent).unwrap().closed);
    assert_eq!(
        registry
            .lock()
            .unwrap()
            .agent_run(accepted.run.id)
            .unwrap()
            .state,
        crate::agent_execution::State::Cancelled
    );
    let drain = super::super::begin_ordinary_exit(&host).unwrap();
    assert!(drain.try_seal_absence());
    let mut cleanup_error = None;
    assert!(host.lock().unwrap().ordinary_exit_cleanup_blocked());
    assert!(!super::super::finish_shutdown_owners(
        &host,
        &mut cleanup_error,
        false
    ));
    resume_tx.send(()).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while host.lock().unwrap().ordinary_exit_cleanup_blocked() {
        assert!(
            Instant::now() < deadline,
            "Agent Close reply guard did not release"
        );
        thread::sleep(Duration::from_millis(5));
    }
    assert!(super::super::finish_shutdown_owners(
        &host,
        &mut cleanup_error,
        false
    ));
    assert!(cleanup_error.is_none());
    let deliveries = {
        let mut state = host.lock().unwrap();
        assert!(drain.try_seal_absence());
        state.close_ordinary_exit_registration()
    };
    super::super::close_shutdown_deliveries(&host, &deliveries);
    assert!(matches!(reply(&mut stream, 3), ServerMessage::Response {
        response: ServerResponse::Agent(Reply::Closed(closed)),
    } if closed.id == agent && closed.closed));
    let mut codec = FrameCodec::new(MAX_FRAME_BYTES);
    while let Some(envelope) = codec.read::<ServerEnvelope>(&mut stream).unwrap() {
        assert_eq!(envelope.request_id, None);
        assert!(matches!(envelope.message, ServerMessage::HostShuttingDown));
    }
    serving.join().unwrap();
    assert!(!host.lock().unwrap().ordinary_exit_cleanup_blocked());
}

/// The host mutex panics when its holder locks it again, on the promise
/// that only that request is lost. Its sender must hear that it failed and
/// keep a connection that still answers.
#[test]
fn a_request_that_panics_is_answered_and_the_connection_keeps_serving() {
    static RELOCKED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    fn relocking_once(
        host: &SharedHost,
        client_id: ClientId,
        request: ClientRequest,
    ) -> Result<RequestOutcome, ProtocolError> {
        if !RELOCKED.swap(true, std::sync::atomic::Ordering::AcqRel) {
            let _held = host.lock().unwrap_or_else(|p| p.into_inner());
            let _ = host.lock();
        }
        process_request(host, client_id, request)
    }
    fn exchange(
        stream: &mut UnixStream,
        proof: &HostSessionProof,
        request_id: u64,
        request: ClientRequest,
    ) -> ServerMessage {
        FrameCodec::new(MAX_CLIENT_FRAME_BYTES)
            .write(
                stream,
                &ClientEnvelope {
                    version: PROTOCOL_VERSION,
                    request_id,
                    proof: Some(proof.clone()),
                    request,
                },
            )
            .unwrap();
        let mut codec = FrameCodec::new(MAX_FRAME_BYTES);
        loop {
            let envelope = codec
                .read::<ServerEnvelope>(stream)
                .expect("the request was never answered")
                .expect("the connection was closed");
            if envelope.request_id == Some(request_id) {
                return envelope.message;
            }
        }
    }

    let (proof, host) = panic_test_host("panicking-request");
    let (mut stream, server_stream) = UnixStream::pair().unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let server_host = Arc::clone(&host);
    let server = thread::spawn(move || serve_with(server_stream, 1, server_host, relocking_once));
    assert!(matches!(
        exchange(
            &mut stream,
            &proof,
            1,
            ClientRequest::Connect {
                expected_host: Some(proof.clone()),
                resume: Vec::new(),
            },
        ),
        ServerMessage::Response {
            response: ServerResponse::Connected { .. }
        }
    ));

    let ServerMessage::Error(error) = exchange(&mut stream, &proof, 2, ClientRequest::List) else {
        panic!("the panicking request must be answered with an error");
    };
    assert_eq!(error.code, ProtocolErrorCode::Internal);
    assert!(matches!(
        exchange(&mut stream, &proof, 3, ClientRequest::List),
        ServerMessage::Response { .. }
    ));

    drop(stream);
    server.join().expect("the client thread survives the panic");
    assert!(host.lock().unwrap().clients.is_empty());
}

/// A terminal's worker can publish an event after a client attaches but
/// before its reply and replay are queued. That event must still reach the
/// client after them: ahead of the reply it would make the client retry, and
/// ahead of the replay it would advance the client's cursor past older events.
#[test]
fn a_live_event_follows_the_attach_reply_and_its_replay() {
    use crate::terminal::{TerminalConfig, TerminalEvent, TerminalHandle};

    fn attach_then_publish(
        host: &SharedHost,
        client_id: ClientId,
        request: ClientRequest,
    ) -> Result<RequestOutcome, ProtocolError> {
        let attached = match &request {
            ClientRequest::Attach { terminal_id, .. } => Some(*terminal_id),
            _ => None,
        };
        let outcome = process_request(host, client_id, request);
        if let Some(terminal_id) = attached {
            crate::host::state::record_event(
                &Arc::downgrade(host),
                terminal_id,
                TerminalEvent::ConversationTitleChanged("Live".into()),
            );
        }
        outcome
    }

    let (proof, host) = panic_test_host("attach-replay-order");
    let recipe = crate::model::LaunchRecipe {
        unset_environment: Default::default(),
        program: "/bin/cat".into(),
        arguments: Vec::new(),
        working_directory: "/".into(),
        environment: Default::default(),
    };
    let session = crate::model::TerminalSession {
        recovery_origin: None,
        predecessor: None,
        owned_codex_listener: None,
        id: TerminalId::new(),
        launch: recipe.clone(),
        transcript_path: None,
        resume_hint: None,
        continuity: crate::model::TerminalContinuity::RelaunchAvailable {
            recipe,
            last_exit: None,
        },
    };
    let terminal_id = session.id;
    let handle = TerminalHandle::spawn(session.clone(), TerminalConfig::default(), |_| {}).unwrap();
    host.lock()
        .unwrap()
        .finish_spawn(2, session, handle.clone(), false)
        .unwrap();
    crate::host::state::record_event(
        &Arc::downgrade(&host),
        terminal_id,
        TerminalEvent::ConversationTitleChanged("Replayed".into()),
    );

    let (mut stream, server_stream) = UnixStream::pair().unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let server_host = Arc::clone(&host);
    let server =
        thread::spawn(move || serve_with(server_stream, 1, server_host, attach_then_publish));
    let mut writer = FrameCodec::new(MAX_CLIENT_FRAME_BYTES);
    let mut reader = FrameCodec::new(MAX_FRAME_BYTES);
    let mut send = |request_id, request| {
        writer
            .write(
                &mut stream,
                &ClientEnvelope {
                    version: PROTOCOL_VERSION,
                    request_id,
                    proof: Some(proof.clone()),
                    request,
                },
            )
            .unwrap();
    };
    send(
        1,
        ClientRequest::Connect {
            expected_host: Some(proof.clone()),
            resume: Vec::new(),
        },
    );
    send(
        2,
        ClientRequest::Attach {
            terminal_id,
            after_generation: 0,
            after_event_sequence: 0,
        },
    );
    let mut order = Vec::new();
    while !order.iter().any(|step: &String| step == "event 2") {
        let envelope = reader
            .read::<ServerEnvelope>(&mut stream)
            .unwrap()
            .expect("the connection stays open");
        order.push(match envelope.message {
            ServerMessage::Response {
                response: ServerResponse::Connected { .. },
            } => "connected".into(),
            ServerMessage::Response {
                response: ServerResponse::Attached { .. },
            } => "attached".into(),
            ServerMessage::Event {
                terminal_id: id,
                sequence,
                ..
            } if id == terminal_id => format!("event {sequence}"),
            ServerMessage::Synchronized {
                terminal_id: id,
                event_sequence,
                ..
            } if id == terminal_id => format!("synchronized {event_sequence}"),
            _ => continue,
        });
    }
    assert_eq!(
        order,
        [
            "connected",
            "attached",
            "event 1",
            "synchronized 1",
            "event 2"
        ]
    );

    drop(stream);
    server.join().unwrap();
    handle.shutdown().unwrap();
}

#[test]
fn a_client_thread_that_unwinds_unregisters_its_client_and_releases_its_writer() {
    let (_, host) = panic_test_host("unwinding-client");
    let (delivered, delivery) = std::sync::mpsc::channel();
    let client_host = Arc::clone(&host);
    let client = thread::spawn(move || {
        let registration = Registration::new(client_host, 7);
        delivered.send(Arc::clone(&registration.delivery)).unwrap();
        panic!("fixture client failure");
    });
    let delivery = delivery.recv().unwrap();
    assert!(client.join().is_err());
    assert!(!host.lock().unwrap().clients.contains_key(&7));
    // The writer blocks here until the queue is closed, holding the socket.
    assert!(delivery.pop().is_none());
}

#[test]
fn handshake_timeout_tracks_one_absolute_deadline() {
    let now = Instant::now();
    let deadline = now + Duration::from_secs(5);
    assert_eq!(
        remaining_handshake_timeout(deadline, now + Duration::from_secs(2)).unwrap(),
        Duration::from_secs(3)
    );
    let error = remaining_handshake_timeout(deadline, deadline).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    let error =
        remaining_handshake_timeout(deadline, deadline + Duration::from_secs(1)).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::TimedOut);
}

/// A terminal starts existing when some client spawns it, and no redraw,
/// event or snapshot edge says so. Without this announcement a connected
/// view cannot learn that another client - the CLI - created a terminal.
#[test]
fn spawning_announces_the_terminal_to_observers_but_never_to_its_creator() {
    use crate::model::{BootId, HostIncarnation, HostSessionId, TerminalContinuity};

    let directory = tempfile::tempdir().unwrap();
    let recipe = crate::model::LaunchRecipe::shell(directory.path().to_owned()).unwrap();
    let session = crate::model::TerminalSession {
        owned_codex_listener: None,
        recovery_origin: None,
        predecessor: None,
        id: crate::model::TerminalId::new(),
        launch: recipe.clone(),
        transcript_path: None,
        resume_hint: None,
        continuity: TerminalContinuity::RelaunchAvailable {
            recipe,
            last_exit: None,
        },
    };
    let terminal_id = session.id;
    let host = Arc::new(crate::host::state::HostLock::new(
        super::super::state::HostState::new(HostSessionProof {
            host_session_id: HostSessionId::new("announce-session"),
            boot_id: BootId::new("boot"),
            host_incarnation: HostIncarnation::new("incarnation"),
        }),
    ));
    let (creator, observer, unsubscribed) = {
        let mut state = host.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let creator = state.register_client(1);
        let observer = state.register_client(2);
        let unsubscribed = state.register_client(3);
        state.watch_terminals(1);
        state.watch_terminals(2);
        (creator, observer, unsubscribed)
    };

    let (response, _, _) = process_request(
        &host,
        1,
        ClientRequest::Spawn {
            session,
            options: crate::terminal_ipc::TerminalSpawnOptions::default(),
        },
    )
    .unwrap();
    assert!(matches!(response, ServerResponse::Spawned { .. }));

    let announced = |delivery: &Arc<DeliveryQueue>| {
        delivery.close_after_drain();
        std::iter::from_fn(|| delivery.pop())
            .filter_map(|queued| match queued.message {
                ServerMessage::TerminalCreated { terminal } => Some(terminal),
                _ => None,
            })
            .collect::<Vec<_>>()
    };
    let observed = announced(&observer);
    assert_eq!(observed.len(), 1);
    assert_eq!(observed[0].session.id, terminal_id);
    assert_eq!(
        observed[0].session.launch.working_directory,
        directory.path()
    );
    // The recipient is not attached, so it is owed the proof it needs to
    // attach rather than a claim that it already has.
    assert!(!observed[0].attached);
    assert!(matches!(
        observed[0].session.continuity,
        TerminalContinuity::DetachedSameBoot { .. }
    ));
    assert!(
        announced(&creator).is_empty(),
        "a spawning client already holds the descriptor its own spawn returned"
    );
    assert!(
        announced(&unsubscribed).is_empty(),
        "a client that never asked for inventory edges must not receive one"
    );

    // The terminal worker's wake callback takes the host lock, so joining
    // it while holding that lock would deadlock the shutdown.
    let handle = host
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .handle(1, terminal_id)
        .unwrap();
    handle.shutdown().unwrap();
}

#[test]
fn transfer_requests_refuse_invalid_membership_without_acknowledging_or_mutating_it() {
    use crate::{
        model::{
            BootId, BrowserId, BrowserSession, CardId, CardKind, ChannelId, HostIncarnation,
            HostSessionId,
        },
        workspace_model::{
            ModelEdit,
            views::{Change, Pane, Scope},
        },
    };
    let directory = tempfile::tempdir().unwrap();
    let repository = directory.path().join("project");
    let other = directory.path().join("other");
    let host = Arc::new(crate::host::state::HostLock::new(
        super::super::state::HostState::new(HostSessionProof {
            host_session_id: HostSessionId::new("transfer-session"),
            boot_id: BootId::new("boot"),
            host_incarnation: HostIncarnation::new("incarnation"),
        }),
    ));
    {
        let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
        state.install_model(
            super::super::model::Registry::at(&directory.path().join("model.sqlite3")).unwrap(),
        );
        state.register_client(1);
    }
    let edit = |edit| process_request(&host, 1, ClientRequest::EditWorkspaceModel { edit });
    for path in [&repository, &other] {
        std::fs::create_dir(path).unwrap();
        edit(ModelEdit::OpenProject {
            repository: path.clone(),
            name: "project".into(),
            kind: Default::default(),
        })
        .unwrap();
    }
    let source = ChannelId::new();
    let destination = ChannelId::new();
    let foreign = ChannelId::new();
    for (id, repository) in [
        (source, repository.clone()),
        (destination, repository),
        (foreign, other),
    ] {
        edit(ModelEdit::CreateChannel {
            id,
            repository,
            title: "channel".into(),
            title_source: Default::default(),
        })
        .unwrap();
    }
    let pane = Pane {
        id: CardId::new(),
        title: "browser".into(),
        user_title: None,
        pending_terminal_start: false,
        width_fraction: 0.5,
        scope: None,
        kind: CardKind::Browser(BrowserSession {
            id: BrowserId::new(),
            current_url: "https://example.com".parse().unwrap(),
            history: vec![],
            history_index: 0,
            scroll_x: 0.,
            scroll_y: 0.,
            fidelity: crate::model::BrowserRestoreFidelity::UrlOnly,
        }),
    };
    edit(ModelEdit::SeedView {
        scope: Scope::Channel(source),
        panes: vec![pane.clone()],
        closed: vec![],
    })
    .unwrap();
    let before = host
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .read_model()
        .unwrap();
    let revision = before
        .views
        .iter()
        .find(|view| view.scope == Scope::Channel(source))
        .unwrap()
        .revision;
    for (scope, id, to) in [
        (Scope::Channel(ChannelId::new()), pane.id, destination),
        (Scope::Channel(source), pane.id, ChannelId::new()),
        (Scope::Channel(source), CardId::new(), destination),
        (Scope::Channel(source), pane.id, foreign),
        (Scope::Channel(source), CardId::new(), source),
    ] {
        let error = edit(ModelEdit::ChangeView {
            scope,
            expected_revision: revision,
            operation: uuid::Uuid::new_v4(),
            change: Change::Transfer {
                id,
                destination: Scope::Channel(to),
                before: None,
            },
        })
        .unwrap_err();
        assert_eq!(error.code, ProtocolErrorCode::InvalidRequest);
        assert!(!error.retryable);
        assert_eq!(
            host.lock()
                .unwrap_or_else(|p| p.into_inner())
                .read_model()
                .unwrap(),
            before
        );
    }
    let valid = ModelEdit::ChangeView {
        scope: Scope::Channel(source),
        expected_revision: revision,
        operation: uuid::Uuid::new_v4(),
        change: Change::Transfer {
            id: pane.id,
            destination: Scope::Channel(destination),
            before: None,
        },
    };
    let moved = edit(valid.clone()).unwrap().0;
    let ServerResponse::WorkspaceModel { model: moved } = moved else {
        panic!("transfer must return its committed model")
    };
    let replayed = edit(valid).unwrap().0;
    let ServerResponse::WorkspaceModel { model: replayed } = replayed else {
        panic!("replay must return its committed model")
    };
    assert_eq!(
        moved, replayed,
        "successful replay must bypass the now-empty source pane check"
    );
}

#[test]
fn stale_pane_source_updates_return_retryable_conflict_without_partial_success() {
    use crate::{
        model::{
            BootId, BrowserId, BrowserSession, CardId, CardKind, ChannelId, HostIncarnation,
            HostSessionId,
        },
        workspace_model::{
            ModelEdit,
            views::{Change, Pane, Scope},
        },
    };
    let directory = tempfile::tempdir().unwrap();
    let repository = directory.path().join("project");
    std::fs::create_dir(&repository).unwrap();
    let host = Arc::new(crate::host::state::HostLock::new(
        super::super::state::HostState::new(HostSessionProof {
            host_session_id: HostSessionId::new("source-update"),
            boot_id: BootId::new("boot"),
            host_incarnation: HostIncarnation::new("incarnation"),
        }),
    ));
    {
        let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
        state.install_model(
            super::super::model::Registry::at(&directory.path().join("model.sqlite3")).unwrap(),
        );
        state.register_client(1);
    }
    let edit = |edit| process_request(&host, 1, ClientRequest::EditWorkspaceModel { edit });
    edit(ModelEdit::OpenProject {
        repository: repository.clone(),
        name: "project".into(),
        kind: Default::default(),
    })
    .unwrap();
    let channel = ChannelId::new();
    edit(ModelEdit::CreateChannel {
        id: channel,
        repository,
        title: "channel".into(),
        title_source: Default::default(),
    })
    .unwrap();
    let original = Pane {
        id: CardId::new(),
        title: "browser".into(),
        user_title: None,
        pending_terminal_start: false,
        width_fraction: 0.5,
        scope: None,
        kind: CardKind::Browser(BrowserSession {
            id: BrowserId::new(),
            current_url: "https://example.com".parse().unwrap(),
            history: vec![],
            history_index: 0,
            scroll_x: 0.,
            scroll_y: 0.,
            fidelity: crate::model::BrowserRestoreFidelity::UrlOnly,
        }),
    };
    edit(ModelEdit::SeedView {
        scope: Scope::Channel(channel),
        panes: vec![original.clone()],
        closed: vec![],
    })
    .unwrap();
    let read = || {
        host.lock()
            .unwrap_or_else(|p| p.into_inner())
            .read_model()
            .unwrap()
    };
    let revision = |model: &crate::workspace_model::WorkspaceModel| {
        model
            .views
            .iter()
            .find(|view| view.scope == Scope::Channel(channel))
            .unwrap()
            .revision
    };
    let mut current = original.clone();
    if let CardKind::Browser(browser) = &mut current.kind {
        browser.current_url = "https://example.com/newer".parse().unwrap();
    }
    edit(ModelEdit::ChangeView {
        scope: Scope::Channel(channel),
        expected_revision: revision(&read()),
        operation: uuid::Uuid::new_v4(),
        change: Change::Update {
            before: original.clone(),
            pane: current,
        },
    })
    .unwrap();
    let mut stale = original.clone();
    stale.title = "must not partly commit".into();
    stale.scope = Some(directory.path().join("stale"));
    let before = read();
    let error = edit(ModelEdit::ChangeView {
        scope: Scope::Channel(channel),
        expected_revision: revision(&before),
        operation: uuid::Uuid::new_v4(),
        change: Change::Update {
            before: original,
            pane: stale,
        },
    })
    .unwrap_err();
    assert_eq!(error.code, ProtocolErrorCode::ViewConflict);
    assert!(error.retryable);
    assert_eq!(read(), before);
}

/// Current windows receive an atomic subscription snapshot and canonical
/// delta edges. An originator that also watches gets only its correlated
/// reply; a read-only client receives no unsolicited model notification.
#[test]
fn editing_the_model_announces_to_watchers_but_never_to_its_originator() {
    use crate::{
        model::{
            BootId, BrowserId, BrowserRestoreFidelity, BrowserSession, CardId, CardKind,
            HostIncarnation, HostSessionId,
        },
        workspace_model::{
            ModelEdit,
            delta::{EditEffects, EditReceipt},
            views::{Change, Pane, Scope},
        },
    };

    struct ModelSocket {
        stream: UnixStream,
        codec: FrameCodec,
        proof: HostSessionProof,
        next_request: u64,
        server: Option<thread::JoinHandle<()>>,
    }

    impl ModelSocket {
        fn connect(host: &SharedHost, id: ClientId) -> Self {
            let proof = current_proof(host);
            let (stream, peer) = UnixStream::pair().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            stream
                .set_write_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let host = Arc::clone(host);
            let server = thread::spawn(move || serve(peer, id, host));
            let mut socket = Self {
                stream,
                codec: FrameCodec::new(MAX_FRAME_BYTES),
                proof: proof.clone(),
                next_request: 1,
                server: Some(server),
            };
            assert!(matches!(
                socket.request(ClientRequest::Connect {
                    expected_host: Some(proof.clone()),
                    resume: Vec::new(),
                }),
                ServerResponse::Connected { proof: connected, .. } if connected == proof
            ));
            socket
        }

        fn read(&mut self) -> ServerEnvelope {
            let envelope = self
                .codec
                .read::<ServerEnvelope>(&mut self.stream)
                .unwrap()
                .expect("the fixture connection must remain open");
            assert_eq!(envelope.version, PROTOCOL_VERSION);
            assert_eq!(envelope.proof, self.proof);
            envelope
        }

        fn request(&mut self, request: ClientRequest) -> ServerResponse {
            let request_id = self.next_request;
            self.next_request += 1;
            self.codec
                .write(
                    &mut self.stream,
                    &ClientEnvelope {
                        version: PROTOCOL_VERSION,
                        request_id,
                        proof: Some(self.proof.clone()),
                        request,
                    },
                )
                .unwrap();
            let envelope = self.read();
            assert_eq!(
                envelope.request_id,
                Some(request_id),
                "an originator or unsubscribed reader must not receive an unsolicited edge"
            );
            match envelope.message {
                ServerMessage::Response { response } => response,
                message => panic!("expected a correlated response, got {message:?}"),
            }
        }

        fn apply(&mut self, edit: ModelEdit) -> EditReceipt {
            match self.request(ClientRequest::ApplyWorkspaceEdit { edit }) {
                ServerResponse::WorkspaceEdit(receipt) => receipt,
                response => panic!("expected a delta receipt, got {response:?}"),
            }
        }

        fn edge(&mut self) -> (u64, ModelEdit, EditEffects) {
            let envelope = self.read();
            assert_eq!(envelope.request_id, None);
            match envelope.message {
                ServerMessage::WorkspaceModelEdited {
                    sequence,
                    edit,
                    effects,
                } => (sequence, edit, effects),
                message => panic!("expected an unsolicited delta edge, got {message:?}"),
            }
        }
    }

    impl Drop for ModelSocket {
        fn drop(&mut self) {
            let _ = self.stream.shutdown(Shutdown::Both);
            if let Some(server) = self.server.take() {
                server.join().unwrap();
            }
        }
    }

    let directory = tempfile::tempdir().unwrap();
    let mut state = super::super::state::HostState::new(HostSessionProof {
        host_session_id: HostSessionId::new("model-session"),
        boot_id: BootId::new("boot"),
        host_incarnation: HostIncarnation::new("incarnation"),
    });
    state.install_model(
        super::super::model::Registry::at(&directory.path().join("model.sqlite3")).unwrap(),
    );
    let host = Arc::new(crate::host::state::HostLock::new(state));
    let mut editor = ModelSocket::connect(&host, 1);
    let mut watcher = ModelSocket::connect(&host, 2);
    let mut unsubscribed = ModelSocket::connect(&host, 3);
    // Both windows watch, so the origin exclusion is exercised independently
    // of subscription. Each reply is the new atomic snapshot with closed ids.
    let mut identity = None;
    for window in [&mut editor, &mut watcher] {
        let ServerResponse::WorkspaceSnapshot { model, closed } =
            window.request(ClientRequest::WatchWorkspaceModel)
        else {
            panic!("subscribing must answer with the model and closed identities");
        };
        assert_eq!(model.sequence, 0);
        assert!(closed.is_empty());
        assert_eq!(*identity.get_or_insert(model.identity), model.identity);
    }

    let repository = directory.path().join("repo");
    std::fs::create_dir(&repository).unwrap();
    let open = ModelEdit::OpenProject {
        kind: Default::default(),
        repository: repository.clone(),
        name: "repo".into(),
    };
    // The explicit legacy edit retains its full reply while current writes
    // use delta receipts. Both routes publish the same canonical edge shape.
    let ServerResponse::WorkspaceModel { model: opened } =
        editor.request(ClientRequest::EditWorkspaceModel { edit: open.clone() })
    else {
        panic!("the legacy edit must retain its full-model response");
    };
    let channel = crate::model::ChannelId::new();
    let create = ModelEdit::CreateChannel {
        title_source: Default::default(),
        id: channel,
        repository,
        title: "shared".into(),
    };
    let created = editor.apply(create.clone());
    assert_eq!(created.identity, identity.unwrap());
    assert_eq!(created.effects.channels.len(), 1);
    assert_eq!(created.effects.channels[0].id, channel);
    assert_ne!(
        created.effects.channels[0].ordinal,
        crate::workspace_model::PENDING_ORDINAL
    );

    let scope = Scope::Channel(channel);
    let pane = Pane {
        id: CardId::new(),
        title: "Fixture browser".into(),
        user_title: None,
        pending_terminal_start: false,
        width_fraction: 0.5,
        scope: None,
        kind: CardKind::Browser(BrowserSession {
            id: BrowserId::new(),
            current_url: "https://example.com".parse().unwrap(),
            history: Vec::new(),
            history_index: 0,
            scroll_x: 0.,
            scroll_y: 0.,
            fidelity: BrowserRestoreFidelity::UrlOnly,
        }),
    };
    let seed = ModelEdit::SeedView {
        scope: scope.clone(),
        panes: vec![pane.clone()],
        closed: Vec::new(),
    };
    let seeded = editor.apply(seed.clone());
    assert_eq!(seeded.effects.views.len(), 1);
    assert_eq!(seeded.effects.views[0].scope, scope);
    assert_eq!(seeded.effects.views[0].before, None);
    let revision = seeded.effects.views[0].after.unwrap();
    let remove = ModelEdit::ChangeView {
        scope: scope.clone(),
        expected_revision: revision,
        operation: uuid::Uuid::new_v4(),
        change: Change::Remove { id: pane.id },
    };
    let removed = editor.apply(remove.clone());
    assert_eq!(removed.effects.views.len(), 1);
    assert_eq!(removed.effects.views[0].scope, scope);
    assert_eq!(removed.effects.views[0].before, Some(revision));
    assert!(removed.effects.views[0].after.unwrap() > revision);
    assert_eq!(removed.effects.closed, [pane.id]);

    let observed = (0..4).map(|_| watcher.edge()).collect::<Vec<_>>();
    assert_eq!(observed[0].0, opened.sequence);
    assert_eq!(observed[0].1, open);
    assert_eq!(observed[0].2.projects, opened.projects);
    for ((sequence, edit, effects), (expected_edit, receipt)) in
        observed[1..]
            .iter()
            .zip([(create, created), (seed, seeded), (remove, removed)])
    {
        assert_eq!(*sequence, receipt.sequence);
        assert_eq!(edit, &expected_edit);
        assert_eq!(effects, &receipt.effects);
    }
    assert!(observed.windows(2).all(|pair| pair[0].0 < pair[1].0));

    // These correlated reads are ordered after every publication above.
    // An unwanted origin/read-only echo would precede the response and fail
    // ModelSocket::request instead of being silently discarded by a client.
    for client in [&mut editor, &mut unsubscribed] {
        let ServerResponse::WorkspaceModel { model } =
            client.request(ClientRequest::ReadWorkspaceModel)
        else {
            panic!("an explicit model read must retain its full response");
        };
        let view = model.views.iter().find(|view| view.scope == scope).unwrap();
        assert!(view.panes.is_empty());
        assert_eq!(view.retired, [pane.id]);
    }
}
