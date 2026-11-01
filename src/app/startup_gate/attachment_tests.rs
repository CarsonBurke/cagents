//! Framed transport fixtures qualify the startup predicate, without a provider,
//! a daemon subprocess, native UI, or a background peer thread.
use super::*;
use crate::{
    model::{BootId, HostIncarnation, HostSessionId},
    terminal::{
        HostedTerminalHandle,
        ipc::{
            ClientEnvelope, ClientRequest, PROTOCOL_VERSION, ProtocolError, ProtocolErrorCode,
            ServerEnvelope, ServerMessage, ServerResponse, TerminalDescriptor, read_frame,
            write_frame,
        },
    },
};
use std::{
    io,
    os::unix::net::{UnixListener, UnixStream},
    thread,
};

fn proof() -> HostSessionProof {
    HostSessionProof {
        host_session_id: HostSessionId::new("startup-fixture"),
        boot_id: BootId::new("boot"),
        host_incarnation: HostIncarnation::new("current"),
    }
}

fn session() -> TerminalSession {
    let CardKind::Terminal(mut session) = crate::app::tests::terminal_card_state().kind else {
        unreachable!();
    };
    let proof = proof();
    session.continuity = TerminalContinuity::Live {
        host_session_id: proof.host_session_id,
        boot_id: proof.boot_id,
        host_incarnation: proof.host_incarnation,
    };
    session
}

fn descriptor(session: &TerminalSession) -> TerminalDescriptor {
    TerminalDescriptor {
        screen_lost_after_crash: false,
        session: session.clone(),
        generation: 0,
        event_sequence: 0,
        activity: None,
        agent_activity: None,
        upgrade_sources: Vec::new(),
        upgrade_relaunches: Vec::new(),
        title: None,
        oldest_event_sequence: 1,
        attached: true,
        transcript: None,
    }
}

fn accept(listener: &UnixListener) -> UnixStream {
    listener.set_nonblocking(true).unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                stream
                    .set_write_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                return stream;
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                assert!(Instant::now() < deadline, "original client did not connect");
                thread::sleep(Duration::from_millis(1));
            }
            Err(error) => panic!("accept original client: {error}"),
        }
    }
}

fn read(stream: &mut UnixStream) -> ClientEnvelope {
    read_frame(stream).unwrap().expect("client request")
}

fn send(stream: &mut UnixStream, request_id: u64, message: ServerMessage) {
    write_frame(
        stream,
        &ServerEnvelope {
            version: PROTOCOL_VERSION,
            request_id: Some(request_id),
            proof: proof(),
            message,
        },
    )
    .unwrap();
}

fn connect(stream: &mut UnixStream, resumed: Vec<TerminalDescriptor>) {
    let request = read(stream);
    assert!(matches!(request.request, ClientRequest::Connect { .. }));
    send(
        stream,
        request.request_id,
        ServerMessage::Response {
            response: ServerResponse::Connected {
                proof: proof(),
                resumed,
            },
        },
    );
}

fn attach(stream: &mut UnixStream, session: &TerminalSession) -> ClientEnvelope {
    let request = read(stream);
    assert_eq!(request.proof, Some(proof()));
    assert!(
        matches!(request.request, ClientRequest::Attach { terminal_id, .. }
        if terminal_id == session.id)
    );
    request
}

fn acknowledge(stream: &mut UnixStream, request: &ClientEnvelope, session: &TerminalSession) {
    send(
        stream,
        request.request_id,
        ServerMessage::Response {
            response: ServerResponse::Attached {
                terminal: descriptor(session),
            },
        },
    );
}

fn wait_ready(surface: &TerminalSurface, session: &TerminalSession) {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match terminal_surface_ready(session, surface) {
            Ok(true) => return,
            Ok(false) => {
                assert!(
                    Instant::now() < deadline,
                    "accepted terminal did not become ready"
                );
                thread::sleep(Duration::from_millis(1));
            }
            Err(reason) => panic!("accepted terminal failed: {reason:?}"),
        }
    }
}

#[test]
fn hibernated_startup_waits_for_fresh_exact_ack_after_socket_eof() {
    let directory = tempfile::tempdir().unwrap();
    let listener = UnixListener::bind(directory.path().join("daemon.sock")).unwrap();
    let session = session();
    let handle = HostedTerminalHandle::connect_at(
        directory.path().join("daemon.sock"),
        session.id,
        Some(proof()),
        |_| {},
    )
    .unwrap();
    // Keep one original handle across every real connection below. The old
    // historical API remains an explicit control, never a reconnect waiter.
    let surface = TerminalSurface::from(handle.clone());
    surface.hibernate().unwrap();
    assert!(surface.snapshot().detached);
    let mut first = accept(&listener);
    assert_eq!(surface.current_attachment(), Ok(None));
    connect(&mut first, Vec::new());
    let request = attach(&mut first, &session);
    assert_eq!(terminal_surface_ready(&session, &surface), Ok(false));
    assert_eq!(surface.host_proof(), None);
    acknowledge(&mut first, &request, &session);
    wait_ready(&surface, &session);
    assert_eq!(surface.current_attachment(), Ok(Some(proof())));
    assert!(
        surface.snapshot().detached,
        "hibernation itself stays detached"
    );

    drop(first); // Actual EOF/reset causes the existing worker to retry.
    let mut second = accept(&listener);
    assert_eq!(
        surface.host_proof(),
        Some(proof()),
        "historical identity survives"
    );
    assert_eq!(
        surface.attachment_failure(),
        None,
        "retry is not a permanent refusal"
    );
    handle.wait_until_attached(Duration::ZERO).unwrap();
    assert_eq!(surface.current_attachment(), Ok(None));
    assert_eq!(terminal_surface_ready(&session, &surface), Ok(false));
    connect(&mut second, Vec::new());
    let request = attach(&mut second, &session);
    assert_eq!(
        surface.current_attachment(),
        Ok(None),
        "Connected is not an exact Attach ACK"
    );
    assert_eq!(terminal_surface_ready(&session, &surface), Ok(false));
    acknowledge(&mut second, &request, &session);
    wait_ready(&surface, &session);

    let mut other = session.clone();
    other.id = TerminalId::new();
    assert!(terminal_surface_ready(&other, &surface).is_err());
    let mut replaced = session.clone();
    replaced.continuity = TerminalContinuity::Live {
        host_session_id: HostSessionId::new("replacement"),
        boot_id: BootId::new("boot"),
        host_incarnation: HostIncarnation::new("different"),
    };
    assert_eq!(
        terminal_surface_ready(&replaced, &surface),
        Err(Unready::Mismatch { attached: proof() }),
        "the exact terminal is attached, under a service its record does not name"
    );

    drop(second);
    let mut third = accept(&listener);
    assert_eq!(terminal_surface_ready(&session, &surface), Ok(false));
    connect(&mut third, vec![descriptor(&session)]);
    wait_ready(&surface, &session); // Real exact-ID resumed acceptance is sufficient.
    surface.stop_for_test();
    assert!(surface.current_attachment().is_err());
    drop(third);
}

#[test]
fn another_resumed_terminal_cannot_hide_the_actual_attach_refusal() {
    let directory = tempfile::tempdir().unwrap();
    let listener = UnixListener::bind(directory.path().join("daemon.sock")).unwrap();
    let session = session();
    let handle = HostedTerminalHandle::connect_at(
        directory.path().join("daemon.sock"),
        session.id,
        Some(proof()),
        |_| {},
    )
    .unwrap();
    let surface = TerminalSurface::from(handle);
    surface.hibernate().unwrap();
    let mut stream = accept(&listener);
    let mut other = session.clone();
    other.id = TerminalId::new();
    connect(&mut stream, vec![descriptor(&other)]);
    let request = attach(&mut stream, &session);
    assert_eq!(surface.current_attachment(), Ok(None));
    assert_eq!(terminal_surface_ready(&session, &surface), Ok(false));
    send(
        &mut stream,
        request.request_id,
        ServerMessage::Error(ProtocolError::new(
            ProtocolErrorCode::TerminalNotFound,
            "exact terminal refused",
            false,
        )),
    );
    let deadline = Instant::now() + Duration::from_secs(2);
    while surface.attachment_failure().is_none() {
        assert!(
            Instant::now() < deadline,
            "actual refusal was not published"
        );
        thread::sleep(Duration::from_millis(1));
    }
    let reason = surface.attachment_failure().unwrap();
    assert!(reason.contains("exact terminal refused"));
    assert_eq!(surface.current_attachment(), Err(reason.clone()));
    assert_eq!(
        terminal_surface_ready(&session, &surface),
        Err(Unready::Failed(reason.clone()))
    );
    surface.stop_for_test();
    assert_eq!(
        surface.current_attachment(),
        Err(reason),
        "first refusal survives stop"
    );
}

#[test]
fn oversized_terminal_refusal_fits_the_startup_handshake_without_losing_recovery() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let session = session();
    let mut card = crate::app::tests::terminal_card_state();
    card.kind = CardKind::Terminal(session.clone());
    app.cards.insert(card.id, CardRuntime::Loading);
    Arc::make_mut(&mut app.durable).cards = vec![card.clone()];
    let raw = format!("exact terminal refused\n\t{}", "多字\r\n  ".repeat(3000));
    app.startup_gate.opened = Some(Instant::now());
    for _ in 0..2 {
        app.startup_attach_failed(card.id, session.id, raw.clone());
        app.startup_gate.retries.expire_window();
    }
    let reason = app
        .startup_pane_check()
        .expect_err("exhausted attachment must fail recovery");
    assert!(reason.starts_with("recovery-incomplete: "));
    assert!(reason.contains("exact terminal refused"));
    assert!(
        reason.contains("Open it to reconnect or recover it"),
        "{reason}"
    );
    assert!(
        reason.contains(&format!("\u{201c}{}\u{201d}", card.title)),
        "named as its tab is: {reason}"
    );
    assert!(!reason.contains(&card.id.to_string()));
    assert!(
        reason.len() < 4096 - 128,
        "leave room for launch-handshake framing"
    );
    assert!(!reason.contains(['\r', '\n', '\t']));
    assert_eq!(
        app.startup_attach_exhausted(card.id),
        Some(raw.as_str()),
        "only the transport summary is bounded; original recovery detail remains"
    );
}
