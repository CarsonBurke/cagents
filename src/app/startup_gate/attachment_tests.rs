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
            Err(reason) => panic!("accepted terminal failed: {reason}"),
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
    assert!(terminal_surface_ready(&replaced, &surface).is_err());

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
        Err(reason.clone())
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
    for _ in 0..ATTACH_ATTEMPTS {
        app.startup_gate
            .terminal_detached(card.id, session.id, raw.clone());
    }
    let reason = app
        .startup_pane_check()
        .expect_err("exhausted attachment must fail recovery");
    assert!(reason.starts_with("recovery-incomplete: "));
    assert!(reason.contains("exact terminal refused"));
    assert!(reason.contains("use their recovery controls"));
    assert!(reason.contains(&card.id.to_string()));
    assert!(
        reason.len() < 4096 - 128,
        "leave room for launch-handshake framing"
    );
    assert!(!reason.contains(['\r', '\n', '\t']));
    assert_eq!(
        app.startup_gate.detached_reason(card.id, &session),
        Some(raw.as_str()),
        "only the transport summary is bounded; original recovery detail remains"
    );
}

/// Add the same accepted handle to an opened gate that still waits for another
/// pane. Inspection replies below are typed component inputs, not daemon RPCs.
fn waiting_beside(surface: TerminalSurface, session: &TerminalSession) -> (Cagents, CardId) {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let mut connected = crate::app::tests::terminal_card_state();
    connected.kind = CardKind::Terminal(session.clone());
    let pending = crate::app::tests::terminal_card_state();
    app.cards
        .insert(connected.id, CardRuntime::Terminal(surface));
    app.cards.insert(pending.id, CardRuntime::Loading);
    Arc::make_mut(&mut app.durable).cards = vec![connected.clone(), pending];
    app.startup_gate = State::fixture_opened();
    app.startup_gate.pending = app.startup_pending().unwrap();
    (app, connected.id)
}

fn reserved_answer(app: &mut Cagents, state: StartupState, proof: HostSessionProof) -> Inspection {
    app.startup_gate.inspected = None; // A fixture dispatch without sleeping through its rate limit.
    app.startup_gate.pending = app.startup_pending().unwrap();
    let context = app
        .reserve_startup_inspection()
        .expect("one eligible component dispatch");
    Inspection {
        context,
        outcome: Ok(Observation {
            socket: app.startup_gate.fixture_endpoint.clone().unwrap(),
            proof,
            state,
        }),
    }
}

#[test]
fn hibernated_current_owner_fences_inspection_even_when_the_saved_proof_lags() {
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
    let surface = TerminalSurface::from(handle.clone());
    surface.hibernate().unwrap();
    let mut stream = accept(&listener);
    connect(&mut stream, Vec::new());
    let request = attach(&mut stream, &session);
    acknowledge(&mut stream, &request, &session);
    wait_ready(&surface, &session);
    let (mut app, card) = waiting_beside(TerminalSurface::from(handle), &session);
    assert!(surface.snapshot().detached);
    assert_eq!(app.startup_connected_owners(), vec![(card, proof())]);
    let pending = app.startup_pending().unwrap().unwrap();
    let current = reserved_answer(&mut app, StartupState::Ready, proof());
    let context = current.context.clone();
    drop(app.startup_state_inspected(current));
    assert!(
        app.startup_gate.daemon.is_some(),
        "current matching observation is useful"
    );

    let old = HostSessionProof {
        host_session_id: HostSessionId::new("previous-host"),
        boot_id: BootId::new("previous-boot"),
        host_incarnation: HostIncarnation::new("previous-incarnation"),
    };
    let CardKind::Terminal(stored) = &mut app.card_state_mut(card).unwrap().kind else {
        unreachable!();
    };
    stored.continuity = TerminalContinuity::Live {
        host_session_id: old.host_session_id.clone(),
        boot_id: old.boot_id.clone(),
        host_incarnation: old.host_incarnation.clone(),
    };
    assert_eq!(
        app.startup_connected_owners(),
        vec![(card, proof())],
        "actual accepted owner stays observable while saved metadata lags"
    );
    for state in [
        StartupState::Ready,
        StartupState::Retained {
            detail: "old owner retained".into(),
        },
    ] {
        let stale = Observed {
            context: context.clone(),
            daemon: Observation {
                socket: app.startup_gate.fixture_endpoint.clone().unwrap(),
                proof: old.clone(),
                state,
            },
        };
        assert!(!app.startup_observation_current(&stale, &pending));
    }
    let CardKind::Terminal(stored) = &app.card_state(card).unwrap().kind else {
        unreachable!();
    };
    assert_eq!(stored.continuity.live_proof(), Some(old));
    assert!(
        terminal_surface_ready(stored, &surface).is_err(),
        "owner evidence does not waive the separate readiness mismatch"
    );
    surface.stop_for_test();
}

#[test]
fn actual_reconnect_retires_cached_inspection_until_fresh_acceptance() {
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
    let surface = TerminalSurface::from(handle.clone());
    surface.hibernate().unwrap();
    let mut first = accept(&listener);
    connect(&mut first, Vec::new());
    let request = attach(&mut first, &session);
    acknowledge(&mut first, &request, &session);
    wait_ready(&surface, &session);
    let (mut app, card) = waiting_beside(TerminalSurface::from(handle), &session);
    let ready = reserved_answer(&mut app, StartupState::Ready, proof());
    drop(app.startup_state_inspected(ready));
    assert!(app.startup_gate.daemon.is_some());
    let delayed = reserved_answer(
        &mut app,
        StartupState::Retained {
            detail: "late old-connection retained answer".into(),
        },
        proof(),
    );
    assert_eq!(delayed.context.owners, vec![(card, proof())]);

    drop(first);
    let mut second = accept(&listener); // Original client really left the first connection.
    assert_eq!(surface.host_proof(), Some(proof()));
    assert_eq!(surface.attachment_failure(), None);
    assert!(app.startup_connected_owners().is_empty());
    assert!(
        app.startup_pending()
            .unwrap()
            .unwrap()
            .panes
            .contains(&card)
    );
    // Suppress a new unexecuted producer while delivering this component answer.
    app.startup_gate.inspected = Some(Instant::now() + Duration::from_secs(60));
    drop(app.verify_startup_gate()); // Tasks are component outputs and are not executed here.
    assert!(app.startup_gate.daemon.is_none());
    assert!(!app.startup_gate.reported);
    drop(app.startup_state_inspected(delayed));
    assert!(app.startup_gate.daemon.is_none());
    assert!(
        !app.startup_gate.reported,
        "late retained answer cannot fail a new waiting phase"
    );

    connect(&mut second, Vec::new());
    let request = attach(&mut second, &session);
    assert!(
        app.startup_connected_owners().is_empty(),
        "Connected alone is insufficient"
    );
    acknowledge(&mut second, &request, &session);
    wait_ready(&surface, &session);
    assert_eq!(app.startup_connected_owners(), vec![(card, proof())]);
    let fresh = reserved_answer(&mut app, StartupState::Ready, proof());
    drop(app.startup_state_inspected(fresh));
    let observed = app.startup_gate.daemon.as_ref().unwrap().clone();
    let pending = app.startup_pending().unwrap().unwrap();
    assert!(app.startup_observation_current(&observed, &pending));

    surface.stop_for_test();
    assert!(app.startup_connected_owners().is_empty());
    assert_eq!(
        app.startup_pending().unwrap(),
        Some(pending.clone()),
        "the other pending pane masks the stopped terminal's failure in the aggregate"
    );
    assert!(
        !app.startup_observation_current(&observed, &pending),
        "loss of current ownership invalidates Ready even with identical Pending IDs"
    );
    assert!(terminal_surface_ready(&session, &surface).is_err());
}
