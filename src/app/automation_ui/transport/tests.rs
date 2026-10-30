use super::*;
use crate::{
    model::{BootId, HostIncarnation, HostSessionId, HostSessionProof},
    terminal_ipc::{
        ClientEnvelope, ClientRequest, PROTOCOL_VERSION, ProtocolError, ProtocolErrorCode,
        ServerEnvelope, ServerMessage, ServerResponse, read_frame, write_frame,
    },
};
use std::{cell::Cell, os::unix::net::UnixStream, thread, time::Duration};

fn context(authority: Authority, remote: bool, no_start: bool) -> Context {
    Context {
        authority,
        remote,
        no_start,
    }
}

fn proof() -> HostSessionProof {
    HostSessionProof {
        host_session_id: HostSessionId::new("automation-transport-fixture"),
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

fn bounded(peer: &UnixStream) {
    peer.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
    peer.set_write_timeout(Some(Duration::from_secs(3)))
        .unwrap();
}

fn handshake(peer: &mut UnixStream) {
    bounded(peer);
    let hello: ClientEnvelope = read_frame(peer).unwrap().unwrap();
    assert!(matches!(hello.request, ClientRequest::Connect { .. }));
    reply(
        peer,
        &hello,
        ServerMessage::Response {
            response: ServerResponse::Connected {
                proof: proof(),
                resumed: Vec::new(),
            },
        },
    );
}

fn request() -> Request {
    Request::Show {
        id: uuid::Uuid::new_v4(),
    }
}

#[test]
fn scoped_remote_automation_refuses_before_socket_bridge_start_or_connect() {
    for authority in [
        Authority::session("synthetic-session".into()),
        Authority::session(String::new()),
        Authority::session("invalid-synthetic-capability".into()),
    ] {
        for no_start in [false, true] {
            assert!(
                context(authority.clone(), true, no_start)
                    .request_with(
                        request(),
                        || panic!("scoped remote must not resolve a bridge"),
                        || panic!("scoped remote must not start a daemon"),
                        |_| panic!("scoped remote must not connect"),
                    )
                    .is_err()
            );
        }
    }
}

#[test]
fn scoped_and_no_start_requests_use_selected_socket_without_startup_fallback() {
    for authority in [
        Authority::session("synthetic-session".into()),
        Authority::session(String::new()),
        Authority::default(),
    ] {
        let no_start = authority.token().is_none();
        let error = context(authority, false, no_start)
            .request_with(
                request(),
                || Err("selected socket unavailable".into()),
                || panic!("must not start after selected socket failure"),
                |_| panic!("must not connect after selected socket failure"),
            )
            .unwrap_err();
        assert_eq!(error, "selected socket unavailable");
    }
    for remote in [false, true] {
        let starts = Cell::new(0);
        let connects = Cell::new(0);
        let error = context(Authority::default(), remote, false)
            .request_with(
                request(),
                || panic!("ordinary User keeps ready/start behavior"),
                || {
                    starts.set(starts.get() + 1);
                    Ok("/fixture/user-selected.socket".into())
                },
                |socket| {
                    assert_eq!(socket, Path::new("/fixture/user-selected.socket"));
                    connects.set(connects.get() + 1);
                    Err("connection refused".into())
                },
            )
            .unwrap_err();
        assert_eq!(error, "connection refused");
        assert_eq!((starts.get(), connects.get()), (1, 1));
    }
}

#[test]
fn original_automation_request_and_scope_reach_the_handshaken_selected_socket() {
    // Invalid/empty capabilities must reach the normal host validator intact;
    // this peer attests transport fidelity, not real host authorization.
    for authority in [
        Authority::session("synthetic-session".into()),
        Authority::session(String::new()),
        Authority::session("invalid-synthetic-capability".into()),
        Authority::default(),
    ] {
        let (stream, mut peer) = UnixStream::pair().unwrap();
        let expected = request();
        let original = expected.clone();
        let expected_authority = authority.clone();
        let server = thread::spawn(move || {
            handshake(&mut peer);
            let envelope: ClientEnvelope = read_frame(&mut peer).unwrap().unwrap();
            assert_eq!(envelope.proof.as_ref(), Some(&proof()));
            assert!(
                matches!(&envelope.request, ClientRequest::Automation { authority, operation }
                if authority == &expected_authority && operation == &expected)
            );
            reply(
                &mut peer,
                &envelope,
                ServerMessage::Response {
                    response: ServerResponse::Automation(Response::Removed),
                },
            );
            assert!(read_frame::<ClientEnvelope>(&mut peer).unwrap().is_none());
        });
        let calls = Cell::new(0);
        let result = context(authority, false, true)
            .request_with(
                original,
                || Ok("/fixture/captured.socket".into()),
                || panic!("no-start must not start"),
                |socket| {
                    assert_eq!(socket, Path::new("/fixture/captured.socket"));
                    calls.set(calls.get() + 1);
                    ControlClient::from_stream(stream).map_err(|error| error.to_string())
                },
            )
            .unwrap();
        assert_eq!(result, Response::Removed);
        assert_eq!(calls.get(), 1);
        server.join().unwrap();
    }
}

#[test]
fn denied_automation_drops_connection_without_user_retry() {
    let (stream, mut peer) = UnixStream::pair().unwrap();
    let authority = Authority::session("synthetic-denied-session".into());
    let expected_authority = authority.clone();
    let expected = request();
    let original = expected.clone();
    let server = thread::spawn(move || {
        handshake(&mut peer);
        let envelope: ClientEnvelope = read_frame(&mut peer).unwrap().unwrap();
        assert!(
            matches!(&envelope.request, ClientRequest::Automation { authority, operation }
            if authority == &expected_authority && operation == &expected)
        );
        reply(
            &mut peer,
            &envelope,
            ServerMessage::Error(ProtocolError::new(
                ProtocolErrorCode::PermissionDenied,
                "fixture scope refused",
                false,
            )),
        );
        assert!(read_frame::<ClientEnvelope>(&mut peer).unwrap().is_none());
    });
    let result = context(authority, false, false).request_with(
        original,
        || Ok("/fixture/scoped.socket".into()),
        || panic!("scope refusal must not start a daemon"),
        |_| ControlClient::from_stream(stream).map_err(|error| error.to_string()),
    );
    assert!(result.unwrap_err().contains("fixture scope refused"));
    server.join().unwrap();
}

#[test]
fn handshake_refusal_drops_connection_before_any_automation_request() {
    let (stream, mut peer) = UnixStream::pair().unwrap();
    let server = thread::spawn(move || {
        bounded(&peer);
        let hello: ClientEnvelope = read_frame(&mut peer).unwrap().unwrap();
        assert!(matches!(hello.request, ClientRequest::Connect { .. }));
        reply(
            &mut peer,
            &hello,
            ServerMessage::Error(ProtocolError::new(
                ProtocolErrorCode::PermissionDenied,
                "fixture handshake refused",
                false,
            )),
        );
        assert!(read_frame::<ClientEnvelope>(&mut peer).unwrap().is_none());
    });
    let calls = Cell::new(0);
    let result = context(Authority::session("synthetic-session".into()), false, true).request_with(
        request(),
        || Ok("/fixture/captured.socket".into()),
        || panic!("handshake refusal must not start"),
        |_| {
            calls.set(calls.get() + 1);
            ControlClient::from_stream(stream).map_err(|error| error.to_string())
        },
    );
    assert!(result.is_err());
    assert_eq!(calls.get(), 1);
    server.join().unwrap();
}
