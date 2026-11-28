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
    assert_eq!(refusal.code, ProtocolErrorCode::DaemonBusy);
    assert!(refusal.retryable);
    // Keep the wording carried clients expect.
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
    // Each traffic thread reports once it is under way, so the exclusive
    // request arrives into the traffic rather than before it starts.
    let (flowing, started) = mpsc::channel();
    // Staggered shared requests keep the gate held at every instant, while
    // the typist keeps sending input through the same dispatcher.
    let requests = (0..3)
        .map(|offset| {
            let (gate, running, flowing) =
                (Arc::clone(&gate), Arc::clone(&running), flowing.clone());
            thread::spawn(move || {
                thread::sleep(Duration::from_millis(offset * 10));
                let mut flowing = Some(flowing);
                while running.load(Ordering::Relaxed) {
                    let _shared = gate.shared();
                    if let Some(flowing) = flowing.take() {
                        let _ = flowing.send(());
                    }
                    thread::sleep(Duration::from_millis(30));
                }
            })
        })
        .collect::<Vec<_>>();
    let typist = {
        let (host, running) = (Arc::clone(&fixture.host), Arc::clone(&running));
        let request = input(&fixture, b"x");
        let mut flowing = Some(flowing);
        thread::spawn(move || {
            let mut accepted = 0_u32;
            while running.load(Ordering::Relaxed) {
                let reply = process_request(&host, TYPIST, request.clone());
                assert!(matches!(reply, Ok((ServerResponse::Accepted, _, _))));
                accepted += 1;
                if let Some(flowing) = flowing.take() {
                    let _ = flowing.send(());
                }
                thread::sleep(Duration::from_millis(2));
            }
            accepted
        })
    };
    for _ in 0..4 {
        started
            .recv_timeout(Duration::from_secs(5))
            .expect("request traffic did not start");
    }
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
    assert_eq!(busy, crate::terminal_ipc::ProtocolError::daemon_busy());
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
    for (held, code) in [
        (ListHeld::Registry, ProtocolErrorCode::AccountRegistryBusy),
        (ListHeld::Lifecycle, ProtocolErrorCode::AccountLifecycleBusy),
    ] {
        let refusal = super::lifecycle_refusal(LifecycleError::ListHeld(held));
        assert_eq!(refusal.code, code);
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

// A private child gives the real shell resolver its own home and cache.
#[cfg(target_os = "linux")]
#[test]
fn accounts_refresh_rediscovers_a_provider_while_shutdown_preflight_can_run() {
    use crate::{
        agent::Provider,
        device_service::{AccountSnapshot, Operation, Reply},
        process::{CancellationToken, CommandSpec, ProcessExecutor, SystemProcessExecutor},
    };
    use std::{fs, io::Write, os::unix::fs::PermissionsExt, path::Path};

    const ROOT: &str = "CAGENTS_TEST_ACCOUNTS_REFRESH_ROOT";
    let Some(root) = std::env::var_os(ROOT) else {
        let root = tempfile::tempdir().unwrap();
        let root = root.path();
        for path in ["home", "config", "data", "state", "cache", "r"] {
            fs::create_dir(root.join(path)).unwrap();
        }
        let executable = std::env::current_exe().unwrap();
        let spec = CommandSpec::new(&executable)
            .args([
                "--exact",
                "host::client::control_admission_tests::accounts_refresh_rediscovers_a_provider_while_shutdown_preflight_can_run",
                "--nocapture",
            ])
            .env(ROOT, root)
            .env("HOME", root.join("home"))
            .env("ENV", root.join("home/.profile"))
            .env("XDG_CONFIG_HOME", root.join("config"))
            .env("ZDOTDIR", root.join("home"))
            .env("XDG_DATA_HOME", root.join("data"))
            .env("XDG_STATE_HOME", root.join("state"))
            .env("XDG_CACHE_HOME", root.join("cache"))
            .env("XDG_RUNTIME_DIR", root.join("r"))
            .env("CLAUDE_CONFIG_DIR", root.join("home/.claude"))
            .env("CODEX_HOME", root.join("home/.codex"))
            .env("PATH", "/usr/bin:/bin")
            .timeout(Duration::from_secs(25));
        let output = SystemProcessExecutor
            .execute(&spec, &CancellationToken::new())
            .unwrap();
        assert!(
            output.success && String::from_utf8_lossy(&output.stdout).contains("1 passed"),
            "private account discovery failed: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        return;
    };

    let root = Path::new(&root);
    let shell = LaunchRecipe::shell(root.to_owned()).unwrap().program;
    let shell_name = Path::new(&shell).file_name().and_then(|name| name.to_str());
    let (profile, startup) = match shell_name {
        Some("fish") => (
            "config/fish/config.fish",
            r#"
            set -gx PATH $HOME/base
            if test -d "$HOME/new-bin"
                if not test -f "$HOME/entered"
                    printf '' > "$HOME/entered"
                    read -l resume < "$HOME/release"
                end
                set -gx PATH $HOME/new-bin $PATH
            end
        "#,
        ),
        Some("bash" | "zsh" | "sh" | "dash" | "ksh") => {
            let profile = match shell_name {
                Some("bash") => "home/.bashrc",
                Some("zsh") => "home/.zshrc",
                _ => "home/.profile",
            };
            (
                profile,
                r#"
                export PATH="$HOME/base"
                if [ -d "$HOME/new-bin" ]; then
                    if [ ! -f "$HOME/entered" ]; then
                        : > "$HOME/entered"
                        read -r resume < "$HOME/release"
                    fi
                    export PATH="$HOME/new-bin:$PATH"
                fi
            "#,
            )
        }
        _ => {
            eprintln!("private startup fixture has no profile for {shell}");
            return;
        }
    };

    let mut state = HostState::new(HostSessionProof {
        host_session_id: HostSessionId::new("accounts-refresh"),
        boot_id: BootId::new("accounts-refresh"),
        host_incarnation: HostIncarnation::new("accounts-refresh"),
    });
    state.register_client(TYPIST);
    state.register_client(OPERATOR);
    let host = Arc::new(HostLock::new(state));
    let snapshot = |response| {
        let ServerResponse::DeviceOperation {
            reply: Reply::Accounts(value),
        } = response
        else {
            panic!("expected the device accounts snapshot, received {response:?}");
        };
        serde_json::from_value::<AccountSnapshot>(value).unwrap()
    };
    let cached = || {
        let request = ClientRequest::DeviceOperation {
            operation: Operation::AccountsRead,
        };
        snapshot(process_request(&host, TYPIST, request).unwrap().0)
    };

    let profile = root.join(profile);
    fs::create_dir_all(profile.parent().unwrap()).unwrap();
    fs::write(profile, startup).unwrap();
    let initial = cached();
    let provider = Provider::Antigravity;
    assert_eq!(initial.installed.get(&provider), Some(&false));

    let bin = root.join("home/new-bin");
    fs::create_dir(&bin).unwrap();
    let executable = bin.join(provider.executable());
    fs::write(&executable, "#!/bin/sh\nexit 0\n").unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(cached().installed.get(&provider), Some(&false));

    let release = root.join("home/release");
    let fifo = std::ffi::CString::new(release.as_os_str().as_encoded_bytes()).unwrap();
    // SAFETY: the terminated path lives for the call and is in the private home.
    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
    let mut release = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(release)
        .unwrap();
    let refresh = submit(
        &host,
        TYPIST,
        ClientRequest::DeviceOperation {
            operation: Operation::AccountsRefresh,
        },
    );
    let deadline = Instant::now() + Duration::from_secs(4);
    let entered = root.join("home/entered");
    while !entered.exists() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    let preflight = entered
        .exists()
        .then(|| process_request(&host, OPERATOR, preflight()));
    release.write_all(b"continue\n").unwrap();
    let refreshed = snapshot(
        refresh
            .recv_timeout(Duration::from_secs(10))
            .unwrap()
            .unwrap(),
    );
    assert!(matches!(
        preflight.expect("refresh did not enter shell startup"),
        Ok((ServerResponse::ShutdownPreflight(_), _, _))
    ));
    assert_eq!(refreshed.installed.get(&provider), Some(&true));
    assert_eq!(refreshed.registry, initial.registry);
    assert_eq!(cached().installed.get(&provider), Some(&true));
}
