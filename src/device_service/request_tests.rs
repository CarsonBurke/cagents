use super::{Operation, Reply, request_as};
use crate::{
    agent_execution::Authority,
    model::{BootId, HostIncarnation, HostSessionId, HostSessionProof},
    terminal_ipc::{
        ClientEnvelope, ClientRequest, PROTOCOL_VERSION, ProtocolError, ProtocolErrorCode,
        ServerEnvelope, ServerMessage, ServerResponse, read_frame, write_frame,
    },
};
use std::{
    cell::{Cell, RefCell},
    io,
    os::unix::net::{UnixListener, UnixStream},
    path::PathBuf,
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

fn proof() -> HostSessionProof {
    HostSessionProof {
        host_session_id: HostSessionId::new("device-request-fixture"),
        boot_id: BootId::new("fixture-boot"),
        host_incarnation: HostIncarnation::new("fixture-incarnation"),
    }
}

fn reply(peer: &mut UnixStream, request: &ClientEnvelope, message: ServerMessage) {
    write_frame(
        peer,
        &ServerEnvelope {
            version: PROTOCOL_VERSION,
            request_id: Some(request.request_id),
            proof: proof(),
            message,
        },
    )
    .unwrap();
}

fn accepted(peer: &mut UnixStream, request: &ClientEnvelope) {
    reply(
        peer,
        request,
        ServerMessage::Response {
            response: ServerResponse::Accepted,
        },
    );
}

fn authenticate(peer: &mut UnixStream, expected: &Authority) -> ClientEnvelope {
    let auth: ClientEnvelope = read_frame(peer).unwrap().unwrap();
    assert_eq!(auth.proof, Some(proof()));
    assert!(
        matches!(&auth.request, ClientRequest::Authenticate { authority } if authority == expected),
        "authentication must precede the device operation"
    );
    auth
}

fn denied() -> ServerMessage {
    ServerMessage::Error(ProtocolError::new(
        ProtocolErrorCode::PermissionDenied,
        "The fixture refused this scope",
        false,
    ))
}

fn home() -> Reply {
    Reply::Home {
        path: "/selected-device/home".into(),
        shell: "/bin/sh".into(),
        environment: Default::default(),
    }
}

/// A private framed peer, never a daemon/provider or global runtime socket.
/// The listener and every stream read are bounded even if a regression sends
/// nothing. Returning the listener lets the caller check for another connection
/// after the production helper has returned, not at an earlier race-prone cut.
fn server(
    serve: impl FnOnce(&mut UnixStream) + Send + 'static,
) -> (tempfile::TempDir, PathBuf, JoinHandle<UnixListener>) {
    server_with_handshake(
        ServerMessage::Response {
            response: ServerResponse::Connected {
                proof: proof(),
                resumed: vec![],
            },
        },
        serve,
    )
}

fn server_with_handshake(
    handshake: ServerMessage,
    serve: impl FnOnce(&mut UnixStream) + Send + 'static,
) -> (tempfile::TempDir, PathBuf, JoinHandle<UnixListener>) {
    let directory = tempfile::Builder::new().prefix("dv-").tempdir().unwrap();
    let socket = directory.path().join("selected.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    listener.set_nonblocking(true).unwrap();
    let worker = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut peer = loop {
            match listener.accept() {
                Ok((peer, _)) => break peer,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < deadline, "selected peer was not connected");
                    thread::sleep(Duration::from_millis(2));
                }
                Err(error) => panic!("private listener: {error}"),
            }
        };
        peer.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
        peer.set_write_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let hello: ClientEnvelope = read_frame(&mut peer).unwrap().unwrap();
        assert!(matches!(&hello.request, ClientRequest::Connect { .. }));
        reply(&mut peer, &hello, handshake);
        serve(&mut peer);
        assert!(
            read_frame::<ClientEnvelope>(&mut peer).unwrap().is_none(),
            "no additional device request or User authentication after the result"
        );
        listener
    });
    (directory, socket, worker)
}

fn finished(worker: JoinHandle<UnixListener>) {
    let listener = worker.join().unwrap();
    assert!(
        matches!(listener.accept(), Err(error) if error.kind() == io::ErrorKind::WouldBlock),
        "the helper must not open a retry connection"
    );
}

fn successful_user(peer: &mut UnixStream) {
    let auth = authenticate(peer, &Authority::default());
    accepted(peer, &auth);
    let operation: ClientEnvelope = read_frame(peer).unwrap().unwrap();
    assert_eq!(operation.proof, Some(proof()));
    assert!(matches!(
        &operation.request,
        ClientRequest::DeviceOperation {
            operation: Operation::Home
        }
    ));
    reply(
        peer,
        &operation,
        ServerMessage::Response {
            response: ServerResponse::DeviceOperation { reply: home() },
        },
    );
}

#[test]
fn user_request_keeps_readiness_and_authenticates_before_one_device_operation() {
    for remote in [false, true] {
        let (_directory, socket, worker) = server(successful_user);
        let starts = Cell::new(0);
        assert_eq!(
            request_as(
                Operation::Home,
                Authority::default(),
                false,
                remote,
                || panic!("ordinary User retains the existing readiness/start path"),
                || {
                    starts.set(starts.get() + 1);
                    Ok(socket)
                },
            )
            .unwrap(),
            home()
        );
        assert_eq!(starts.get(), 1);
        finished(worker);
    }
}

#[test]
fn explicit_no_start_user_request_uses_the_selected_socket_without_readiness() {
    for remote in [false, true] {
        let (_directory, socket, worker) = server(successful_user);
        let selections = Cell::new(0);
        assert_eq!(
            request_as(
                Operation::Home,
                Authority::default(),
                true,
                remote,
                || {
                    selections.set(selections.get() + 1);
                    Ok(socket)
                },
                || panic!("explicit no-start must never enter daemon readiness"),
            )
            .unwrap(),
            home()
        );
        assert_eq!(selections.get(), 1);
        finished(worker);
    }
}

#[test]
fn remote_present_authority_is_refused_before_socket_resolution_or_startup() {
    for authority in [
        Authority::session("fixture-remote-device-capability".into()),
        Authority::session(String::new()),
    ] {
        for no_start in [false, true] {
            let error = request_as(
                Operation::Home,
                authority.clone(),
                no_start,
                true,
                || panic!("scoped remote selection must not launch an SSH bridge"),
                || panic!("scoped remote requests must not enter daemon readiness"),
            )
            .unwrap_err();
            assert!(error.contains("require a user terminal on the remote device"));
            if let Some(token) = authority.token().filter(|token| !token.is_empty()) {
                assert!(!error.contains(token));
            }
        }
    }
}

#[test]
fn scoped_request_keeps_captured_authority_through_selection_and_device_refusal() {
    let authority = Authority::session("fixture-captured-device-capability".into());
    let expected = authority.clone();
    let (_directory, socket, worker) = server(move |peer| {
        let auth = authenticate(peer, &expected);
        accepted(peer, &auth);
        let operation: ClientEnvelope = read_frame(peer).unwrap().unwrap();
        assert_eq!(operation.proof, Some(proof()));
        assert!(matches!(
            &operation.request,
            ClientRequest::DeviceOperation {
                operation: Operation::Home
            }
        ));
        reply(peer, &operation, denied());
    });
    // Model a different later environment sample without changing process
    // environment: the authority passed to the real helper is already captured.
    let later_sample = RefCell::new(authority.clone());
    let captured = later_sample.borrow().clone();
    let error = request_as(
        Operation::Home,
        captured,
        false,
        false,
        || {
            *later_sample.borrow_mut() = Authority::default();
            Ok(socket)
        },
        || panic!("a scoped request must never start a daemon"),
    )
    .unwrap_err();
    assert!(error.contains("PermissionDenied"));
    assert!(!error.contains(authority.token().unwrap()));
    finished(worker);
}

#[test]
fn refused_present_authority_never_dispatches_or_falls_back_to_user() {
    for authority in [
        Authority::session("fixture-invalid-device-capability".into()),
        // from_environment preserves an empty credential for non-Unicode
        // presence too; exercise that representation without env mutation.
        Authority::session(String::new()),
    ] {
        let expected = authority.clone();
        let (_directory, socket, worker) = server(move |peer| {
            let auth = authenticate(peer, &expected);
            reply(peer, &auth, denied());
        });
        let error = request_as(
            Operation::Home,
            authority.clone(),
            false,
            false,
            || Ok(socket),
            || panic!("even invalid presence must not become User startup"),
        )
        .unwrap_err();
        assert!(error.contains("PermissionDenied"));
        if let Some(token) = authority.token().filter(|token| !token.is_empty()) {
            assert!(!error.contains(token));
            assert!(!format!("{authority:?}").contains(token));
        }
        finished(worker);
    }
}

#[test]
fn scoped_and_no_start_requests_preserve_selected_socket_errors_without_startup() {
    for (authority, no_start) in [
        (Authority::session("fixture-selected-scope".into()), false),
        (Authority::session(String::new()), false),
        (Authority::default(), true),
    ] {
        let selections = Cell::new(0);
        let result = request_as(
            Operation::Home,
            authority,
            no_start,
            false,
            || {
                selections.set(selections.get() + 1);
                Err("selected device unavailable".into())
            },
            || panic!("a selected-socket error must not fall back to startup"),
        );
        assert_eq!(result, Err("selected device unavailable".into()));
        assert_eq!(selections.get(), 1);
    }
    let starts = Cell::new(0);
    assert_eq!(
        request_as(
            Operation::Home,
            Authority::default(),
            false,
            false,
            || panic!("ordinary User uses readiness, not a selected-only fallback"),
            || {
                starts.set(starts.get() + 1);
                Err("User daemon readiness failed".into())
            },
        ),
        Err("User daemon readiness failed".into())
    );
    assert_eq!(starts.get(), 1);
}

#[test]
fn handshake_refusal_and_proof_mismatch_never_authenticate_dispatch_or_retry() {
    let mut other_proof = proof();
    other_proof.host_incarnation = HostIncarnation::new("other-fixture-incarnation");
    for (authority, no_start, handshake, expected_error) in [
        (
            Authority::session("fixture-handshake-scope".into()),
            false,
            denied(),
            "PermissionDenied",
        ),
        (
            Authority::default(),
            true,
            ServerMessage::Response {
                response: ServerResponse::Connected {
                    proof: other_proof,
                    resumed: vec![],
                },
            },
            "proof does not match",
        ),
    ] {
        let (_directory, socket, worker) = server_with_handshake(handshake, |_| {});
        let selections = Cell::new(0);
        let error = request_as(
            Operation::Home,
            authority,
            no_start,
            false,
            || {
                selections.set(selections.get() + 1);
                Ok(socket)
            },
            || panic!("a failed selected handshake must not trigger startup"),
        )
        .unwrap_err();
        assert!(error.contains(expected_error), "{error}");
        assert_eq!(selections.get(), 1);
        finished(worker);
    }
}

#[test]
fn missing_selected_socket_never_starts_or_retries_as_user() {
    let directory = tempfile::Builder::new().prefix("dv-").tempdir().unwrap();
    for (authority, no_start) in [
        (Authority::session("fixture-missing-scope".into()), false),
        (Authority::session(String::new()), false),
        (Authority::default(), true),
    ] {
        let selections = Cell::new(0);
        let error = request_as(
            Operation::Home,
            authority,
            no_start,
            false,
            || {
                selections.set(selections.get() + 1);
                Ok(directory.path().join("missing.sock"))
            },
            || panic!("a missing selected daemon must not trigger startup"),
        )
        .unwrap_err();
        assert!(error.starts_with("could not connect to the daemon:"));
        assert_eq!(selections.get(), 1);
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
    }
}
