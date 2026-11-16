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
    assert_eq!(refusal.message, super::control_gate::busy().message);

    // Once it has given up, ordinary requests no longer wait for it.
    assert!(gate.try_shared().is_some());
    assert!(process_request(&fixture.host, TYPIST, ClientRequest::List).is_ok());
    drop(slow_request);
    assert!(matches!(
        process_request(&fixture.host, OPERATOR, preflight()),
        Ok((ServerResponse::ShutdownPreflight(_), _, _))
    ));
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
    thread::sleep(Duration::from_millis(60));
    assert!(
        gate.try_exclusive().is_none(),
        "the shared requests overlap"
    );

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
