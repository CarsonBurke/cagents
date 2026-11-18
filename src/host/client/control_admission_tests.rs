//! Exclusive control requests wait beside terminal input, never ahead of it.
use super::{
    ClientRequest, ProtocolErrorCode, SHUTDOWN_VERSION, ServerResponse, SharedHost,
    ShutdownOperation, control_gate, process_request,
};
use crate::{
    host::state::{HostLock, HostState},
    model::{
        BootId, HostIncarnation, HostSessionId, HostSessionProof, LaunchRecipe, TerminalContinuity,
        TerminalId, TerminalSession,
    },
};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

const TYPIST: u64 = 1;
const OPERATOR: u64 = 2;

struct Fixture {
    host: SharedHost,
    terminal: TerminalId,
    _directory: tempfile::TempDir,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        // The worker's wake callback takes HostState; never join it under that lock.
        let handle = self.host.lock().unwrap().handle(TYPIST, self.terminal);
        if let Ok(handle) = handle {
            let _ = handle.shutdown();
        }
    }
}

/// A host with a live `cat` terminal attached to the typist's connection.
fn fixture(name: &str) -> Fixture {
    let directory = tempfile::tempdir().unwrap();
    let mut recipe = LaunchRecipe::shell(directory.path().to_owned()).unwrap();
    recipe.program = "/bin/cat".into();
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
    let terminal = session.id;
    let mut state = HostState::new(HostSessionProof {
        host_session_id: HostSessionId::new(name),
        boot_id: BootId::new(name),
        host_incarnation: HostIncarnation::new(name),
    });
    state.register_client(TYPIST);
    state.register_client(OPERATOR);
    let host = Arc::new(HostLock::new(state));
    let (response, _, _) = process_request(
        &host,
        TYPIST,
        ClientRequest::Spawn {
            session,
            options: Default::default(),
        },
    )
    .unwrap();
    assert!(matches!(response, ServerResponse::Spawned { .. }));
    Fixture {
        host,
        terminal,
        _directory: directory,
    }
}

fn preflight() -> ClientRequest {
    ClientRequest::ShutdownV1 {
        version: SHUTDOWN_VERSION,
        operation: ShutdownOperation::Preflight,
    }
}

/// Runs `request` on its own connection thread and reports its outcome.
fn submit(
    host: &SharedHost,
    client: u64,
    request: ClientRequest,
) -> mpsc::Receiver<Result<ServerResponse, super::ProtocolError>> {
    let (sender, receiver) = mpsc::channel();
    let host = Arc::clone(host);
    thread::spawn(move || {
        let _ = sender.send(process_request(&host, client, request).map(|(r, _, _)| r));
    });
    receiver
}

/// Waits until an exclusive request is queued behind the held shared request.
fn wait_until_exclusive_waits(host: &SharedHost) {
    let gate = control_gate(host);
    let deadline = Instant::now() + Duration::from_secs(5);
    while gate.try_shared().is_some() {
        assert!(
            Instant::now() < deadline,
            "the exclusive request never started waiting for admission"
        );
        thread::sleep(Duration::from_millis(2));
    }
}

#[test]
fn a_waiting_exclusive_request_does_not_hold_back_another_clients_input() {
    let fixture = fixture("exclusive-beside-input");
    let gate = control_gate(&fixture.host);
    // What a slow shared request (a GitHub read, a provider probe) holds.
    let slow_request = gate.shared();
    let exclusive = submit(&fixture.host, OPERATOR, preflight());
    wait_until_exclusive_waits(&fixture.host);

    let input = submit(
        &fixture.host,
        TYPIST,
        input(&fixture, b"typed while an update waits\n"),
    );
    let typed = input.recv_timeout(Duration::from_secs(2));
    drop(slow_request);
    assert!(
        matches!(typed, Ok(Ok(ServerResponse::Accepted))),
        "terminal input waited behind a queued exclusive request: {typed:?}"
    );
    let admitted = exclusive.recv_timeout(Duration::from_secs(10)).unwrap();
    assert!(
        matches!(admitted, Ok(ServerResponse::ShutdownPreflight(_))),
        "the exclusive request was not admitted once the shared request ended: {admitted:?}"
    );
}

fn input(fixture: &Fixture, bytes: &[u8]) -> ClientRequest {
    ClientRequest::Input {
        terminal_id: fixture.terminal,
        bytes: bytes.to_vec(),
    }
}

#[test]
fn an_exclusive_request_refuses_retryably_at_its_deadline() {
    let fixture = fixture("exclusive-deadline");
    let gate = control_gate(&fixture.host);
    let slow_request = gate.shared();
    let started = Instant::now();
    let refusal = process_request(&fixture.host, OPERATOR, preflight())
        .map(|(response, _, _)| response)
        .expect_err("an exclusive request was admitted beside a shared holder");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "the refusal arrived after the control client stops reading"
    );
    assert_eq!(refusal.code, ProtocolErrorCode::Internal);
    assert!(refusal.retryable);
    // Clients match this exact sentence; the update retries only on it.
    assert_eq!(refusal.message, crate::terminal_ipc::DAEMON_BUSY);
    assert!(
        crate::control::ControlError::Remote {
            code: refusal.code,
            message: refusal.message.clone(),
            retryable: refusal.retryable,
        }
        .is_daemon_busy()
    );

    // Once it has given up, ordinary requests no longer wait for it.
    assert!(gate.try_shared().is_some());
    assert!(process_request(&fixture.host, TYPIST, ClientRequest::List).is_ok());
    drop(slow_request);
    assert!(matches!(
        process_request(&fixture.host, OPERATOR, preflight()),
        Ok((ServerResponse::ShutdownPreflight(_), _, _))
    ));
}

/// A confirmed stop refused as busy was refused before the daemon read its
/// capability, so the update can offer the same capability again: the daemon
/// accepts it once the operation it was finishing ends.
#[test]
fn a_confirmed_stop_refused_as_busy_is_accepted_with_the_same_capability() {
    let fixture = fixture("busy-confirm");
    let Ok((ServerResponse::ShutdownPreflight(preflight), _, _)) =
        process_request(&fixture.host, OPERATOR, self::preflight())
    else {
        panic!("the preflight was not issued");
    };
    let confirm = ClientRequest::ShutdownV1 {
        version: SHUTDOWN_VERSION,
        operation: ShutdownOperation::Confirm {
            capability: preflight.capability,
        },
    };
    let gate = control_gate(&fixture.host);
    let operation = gate.shared();
    let refusal = process_request(&fixture.host, OPERATOR, confirm.clone())
        .map(|(response, _, _)| response)
        .expect_err("a confirmed stop was admitted beside a shared holder");
    assert_eq!(refusal.message, crate::terminal_ipc::DAEMON_BUSY);
    assert!(refusal.retryable);

    drop(operation);
    let accepted = process_request(&fixture.host, OPERATOR, confirm);
    assert!(
        matches!(
            accepted,
            Ok((
                ServerResponse::ShutdownOutcome(crate::terminal_ipc::ShutdownOutcome::Accepted {
                    live_terminals_before: 1,
                    termination_requested: true,
                }),
                _,
                true,
            ))
        ),
        "the same capability was not accepted after the busy refusal: {accepted:?}"
    );
}

#[test]
fn an_exclusive_request_is_admitted_under_steady_request_traffic() {
    let fixture = fixture("exclusive-under-traffic");
    let gate = control_gate(&fixture.host);
    let running = Arc::new(AtomicBool::new(true));
    // Staggered shared requests keep the gate held at every instant, while
    // the typist keeps sending input through the same dispatcher.
    let requests = (0..3)
        .map(|offset| {
            let (gate, running) = (Arc::clone(&gate), Arc::clone(&running));
            thread::spawn(move || {
                thread::sleep(Duration::from_millis(offset * 10));
                while running.load(Ordering::Relaxed) {
                    let _shared = gate.shared();
                    thread::sleep(Duration::from_millis(30));
                }
            })
        })
        .collect::<Vec<_>>();
    let typist = {
        let (host, running) = (Arc::clone(&fixture.host), Arc::clone(&running));
        let request = input(&fixture, b"x");
        thread::spawn(move || {
            let mut accepted = 0_u32;
            while running.load(Ordering::Relaxed) {
                let reply = process_request(&host, TYPIST, request.clone());
                assert!(matches!(reply, Ok((ServerResponse::Accepted, _, _))));
                accepted += 1;
                thread::sleep(Duration::from_millis(2));
            }
            accepted
        })
    };
    let admitted = process_request(&fixture.host, OPERATOR, preflight());
    assert!(
        matches!(admitted, Ok((ServerResponse::ShutdownPreflight(_), _, _))),
        "steady request traffic starved the exclusive request: {admitted:?}"
    );

    running.store(false, Ordering::Relaxed);
    for request in requests {
        request.join().unwrap();
    }
    assert!(typist.join().unwrap() > 0);
}

/// One connection's requests run in order, as the UI's hosted terminal sends
/// them. A redraw makes it ask for a snapshot just before the next keystroke.
mod pipelined {
    use super::*;
    use crate::terminal_ipc::{
        ClientEnvelope, FrameCodec, MAX_CLIENT_FRAME_BYTES, MAX_FRAME_BYTES, PROTOCOL_VERSION,
        ServerEnvelope, ServerMessage,
    };
    use std::os::unix::net::UnixStream;

    const CONNECTION: u64 = 3;

    struct Connection {
        stream: UnixStream,
        proof: HostSessionProof,
        replies: FrameCodec,
        serving: Option<thread::JoinHandle<()>>,
    }

    impl Connection {
        fn open(host: &SharedHost, terminal: TerminalId) -> Self {
            let proof = host.lock().unwrap().proof().clone();
            let (stream, server) = UnixStream::pair().unwrap();
            let serving_host = Arc::clone(host);
            let serving =
                thread::spawn(move || super::super::serve(server, CONNECTION, serving_host));
            let mut connection = Self {
                stream,
                proof: proof.clone(),
                replies: FrameCodec::new(MAX_FRAME_BYTES),
                serving: Some(serving),
            };
            connection.send(
                1,
                ClientRequest::Connect {
                    expected_host: Some(proof),
                    resume: Vec::new(),
                },
            );
            assert!(matches!(
                connection.reply(1, Duration::from_secs(5)),
                Some(ServerMessage::Response {
                    response: ServerResponse::Connected { .. }
                })
            ));
            connection.send(
                2,
                ClientRequest::Attach {
                    terminal_id: terminal,
                    after_generation: 0,
                    after_event_sequence: 0,
                },
            );
            assert!(matches!(
                connection.reply(2, Duration::from_secs(5)),
                Some(ServerMessage::Response { .. })
            ));
            connection
        }

        fn send(&mut self, request_id: u64, request: ClientRequest) {
            FrameCodec::new(MAX_CLIENT_FRAME_BYTES)
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
        }

        /// The reply to `request_id`, skipping pushed updates; None if it
        /// does not arrive within `patience`.
        fn reply(&mut self, request_id: u64, patience: Duration) -> Option<ServerMessage> {
            let deadline = Instant::now() + patience;
            loop {
                let remaining = deadline.checked_duration_since(Instant::now())?;
                self.stream
                    .set_read_timeout(Some(remaining.max(Duration::from_millis(1))))
                    .unwrap();
                match self.replies.read::<ServerEnvelope>(&mut self.stream) {
                    Ok(Some(envelope)) if envelope.request_id == Some(request_id) => {
                        return Some(envelope.message);
                    }
                    Ok(Some(_)) => {}
                    Ok(None) | Err(_) => return None,
                }
            }
        }
    }

    impl Drop for Connection {
        fn drop(&mut self) {
            let _ = self.stream.shutdown(std::net::Shutdown::Both);
            if let Some(serving) = self.serving.take() {
                let _ = serving.join();
            }
        }
    }

    #[test]
    fn terminal_requests_queued_ahead_of_input_on_its_connection_do_not_hold_it_back() {
        let fixture = fixture("pipelined-input");
        let mut connection = Connection::open(&fixture.host, fixture.terminal);
        let gate = control_gate(&fixture.host);
        let slow_request = gate.shared();
        let exclusive = submit(&fixture.host, OPERATOR, preflight());
        wait_until_exclusive_waits(&fixture.host);

        // What the hosted terminal sends for a redraw, a search box and the
        // Clear action, each followed on the same connection by a keystroke.
        let terminal_id = fixture.terminal;
        let ahead = [
            ClientRequest::Snapshot {
                terminal_id,
                after_generation: 0,
            },
            ClientRequest::SearchTerminal {
                terminal_id,
                request: 1,
                query: "typed".into(),
                case_sensitive: false,
            },
            ClientRequest::ClearTerminalScrollback { terminal_id },
        ];
        let mut request_id = 10;
        let mut typed = Vec::new();
        for request in ahead {
            let kind = format!("{request:?}");
            connection.send(request_id, request);
            connection.send(request_id + 1, input(&fixture, b"x"));
            typed.push((kind, request_id + 1));
            request_id += 2;
        }
        let replies = typed
            .into_iter()
            .map(|(kind, id)| (kind, connection.reply(id, Duration::from_secs(2))))
            .collect::<Vec<_>>();
        drop(slow_request);
        for (kind, reply) in replies {
            assert!(
                matches!(
                    reply,
                    Some(ServerMessage::Response {
                        response: ServerResponse::Accepted
                    })
                ),
                "input queued behind {kind} waited for the exclusive request: {reply:?}"
            );
        }
        let admitted = exclusive.recv_timeout(Duration::from_secs(10)).unwrap();
        assert!(matches!(admitted, Ok(ServerResponse::ShutdownPreflight(_))));
    }
}

#[test]
fn a_busy_account_operation_refuses_as_the_gate_does() {
    use crate::account::remote::LifecycleError;
    let busy = super::lifecycle_refusal(LifecycleError::Busy);
    assert_eq!(busy, super::control_gate::busy());
    assert!(
        crate::control::ControlError::Remote {
            code: busy.code,
            message: busy.message,
            retryable: busy.retryable,
        }
        .is_daemon_busy()
    );
    let failed = super::lifecycle_refusal(LifecycleError::Failed("no CLI".into()));
    assert_eq!(failed.code, ProtocolErrorCode::InvalidRequest);
    assert_eq!(failed.message, "no CLI");
    assert!(!failed.retryable);
}

#[test]
fn a_held_account_list_refuses_retryably_in_its_own_words() {
    use crate::account::remote::{LifecycleError, ListHeld};
    for held in [ListHeld::Registry, ListHeld::Lifecycle] {
        let refusal = super::lifecycle_refusal(LifecycleError::ListHeld(held));
        assert_eq!(refusal.code, ProtocolErrorCode::InvalidRequest);
        assert_eq!(refusal.message, held.to_string());
        assert!(refusal.retryable);
        assert!(
            !crate::control::ControlError::Remote {
                code: refusal.code,
                message: refusal.message,
                retryable: refusal.retryable,
            }
            .is_daemon_busy(),
            "{held:?} is not the daemon being busy"
        );
    }
}
