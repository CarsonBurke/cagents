//! Exercise the real nested coding-default helpers against a private framed peer.
use super::*;
use crate::{
    account::readiness::PhysicalIdentity,
    agent_execution::Authority,
    device_service::{Operation, Reply},
    model::{BootId, HostIncarnation, HostSessionId},
    terminal_ipc::{
        ClientEnvelope, ClientRequest, PROTOCOL_VERSION, ProtocolError, ProtocolErrorCode,
        ServerEnvelope, ServerMessage, ServerResponse, read_frame, write_frame,
    },
};
use std::{
    os::unix::net::{UnixListener, UnixStream},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

fn helper_reply(
    peer: &mut UnixStream,
    request: &ClientEnvelope,
    proof: &HostSessionProof,
    message: ServerMessage,
) {
    write_frame(
        peer,
        &ServerEnvelope {
            version: PROTOCOL_VERSION,
            request_id: Some(request.request_id),
            proof: proof.clone(),
            message,
        },
    )
    .unwrap();
}

/// The peer is private fixture transport, never a daemon. Construct the
/// Context directly so this regression neither reads nor changes caller
/// authority in the environment, including when it exercises User scope.
fn framed_actual_helper(authority: Authority, creating: bool, deny_auth: bool) {
    let directory = tempfile::tempdir().unwrap();
    let socket = directory.path().join("s");
    let listener = UnixListener::bind(&socket).unwrap();
    listener.set_nonblocking(true).unwrap();
    let proof = HostSessionProof {
        host_session_id: HostSessionId::new("private-coding-peer"),
        boot_id: BootId::new("private-coding-boot"),
        host_incarnation: HostIncarnation::new("private-coding-incarnation"),
    };
    let account = crate::account::Account::linked(
        crate::agent::Provider::Codex,
        uuid::Uuid::from_u128(1),
        PathBuf::from("/private-fixture/codex"),
    );
    let context = Context {
        form: 1,
        fence: Fence {
            account: account.clone(),
            owner: 1,
            registered: None,
        },
        target: Target {
            socket,
            proof: proof.clone(),
            authority: authority.clone(),
        },
        request: InspectRequest {
            account: account.id(),
            expected_home: PhysicalIdentity {
                device: 4,
                inode: 9,
            },
            expected_email: "private-fixture@example.test".into(),
        },
    };
    let observation = Observation {
        account: context.request.account.clone(),
        host: proof.clone(),
        home: context.request.expected_home,
        presence: Presence::Absent,
        expected_email: context.request.expected_email.clone(),
    };
    let request = CreateRequest {
        observation: observation.clone(),
        network_access: true,
    };
    let expected_operation = if creating {
        Operation::AccountCodingConfigCreate(request.clone())
    } else {
        Operation::AccountCodingConfigInspect(context.request.clone())
    };
    let (finished, settled) = mpsc::channel();
    let peer = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(error)
                    if error.kind() == std::io::ErrorKind::WouldBlock
                        && Instant::now() < deadline =>
                {
                    thread::sleep(Duration::from_millis(5))
                }
                Err(_) => panic!("private coding peer did not accept the helper connection"),
            }
        };
        stream.set_nonblocking(false).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let connect: ClientEnvelope = read_frame(&mut stream).unwrap().unwrap();
        assert!(matches!(&connect.request, ClientRequest::Connect { .. }));
        helper_reply(
            &mut stream,
            &connect,
            &proof,
            ServerMessage::Response {
                response: ServerResponse::Connected {
                    proof: proof.clone(),
                    resumed: vec![],
                },
            },
        );
        let authenticate: ClientEnvelope = read_frame(&mut stream).unwrap().unwrap();
        assert_eq!(authenticate.proof.as_ref(), Some(&proof));
        match &authenticate.request {
            ClientRequest::Authenticate { authority: actual } => assert_eq!(actual, &authority),
            _ => panic!("coding helper dispatched business work before authentication"),
        }
        if deny_auth {
            helper_reply(
                &mut stream,
                &authenticate,
                &proof,
                ServerMessage::Error(ProtocolError::new(
                    ProtocolErrorCode::PermissionDenied,
                    "private fixture refusal",
                    false,
                )),
            );
        } else {
            helper_reply(
                &mut stream,
                &authenticate,
                &proof,
                ServerMessage::Response {
                    response: ServerResponse::Accepted,
                },
            );
            let business: ClientEnvelope = read_frame(&mut stream).unwrap().unwrap();
            assert_eq!(business.proof.as_ref(), Some(&proof));
            match &business.request {
                ClientRequest::DeviceOperation { operation } => {
                    assert_eq!(operation, &expected_operation)
                }
                _ => panic!("coding helper did not send its exact captured request"),
            }
            let reply = if creating {
                Reply::AccountCodingConfigCreate(Box::new(CreateReport {
                    account: observation.account,
                    host: proof.clone(),
                    home: observation.home,
                    network_access: true,
                    outcome: Outcome::Created,
                }))
            } else {
                Reply::AccountCodingConfigInspect(Box::new(observation))
            };
            helper_reply(
                &mut stream,
                &business,
                &proof,
                ServerMessage::Response {
                    response: ServerResponse::DeviceOperation { reply },
                },
            );
        }
        // Refused authentication permits zero business requests. Accepted
        // authentication permits exactly one, with no resend or fallback.
        assert!(read_frame::<ClientEnvelope>(&mut stream).unwrap().is_none());
        // Keep the listener alive until the synchronous helper returns. A
        // fallback connection must remain observable, rather than failing only
        // because this peer dropped its listener immediately after the EOF.
        settled
            .recv_timeout(Duration::from_secs(3))
            .expect("coding helper did not settle after closing its connection");
        assert!(
            matches!(listener.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock)
        );
    });
    let result = if creating {
        create(context, request).map(|_| ())
    } else {
        inspect(context).map(|_| ())
    };
    let _ = finished.send(());
    peer.join().unwrap();
    assert_eq!(result.is_ok(), !deny_auth);
}

#[test]
fn coding_defaults_actual_helpers_authenticate_captured_authority_before_rpc() {
    for authority in [
        Authority::default(),
        Authority::session("private-test-only".into()),
        Authority::session(String::new()),
    ] {
        for creating in [false, true] {
            framed_actual_helper(authority.clone(), creating, false);
        }
    }
}

#[test]
fn coding_defaults_actual_helpers_refused_auth_never_dispatches_or_reconnects() {
    for authority in [
        Authority::default(),
        Authority::session("private-test-only".into()),
        Authority::session(String::new()),
    ] {
        for creating in [false, true] {
            framed_actual_helper(authority.clone(), creating, true);
        }
    }
}
