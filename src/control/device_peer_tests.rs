//! Framed peer capability exchange and renewal with private test daemons.
use super::*;
use crate::{
    agent_execution::{Lineage, Operation, Reply, permission::StatedCeiling},
    automations::{Request as AutomationRequest, Response as AutomationResponse},
    device_trust::{CAPABILITY_RENEWAL, Claim, SECRET_BYTES},
    model::{BootId, HostIncarnation, HostSessionId},
    terminal::ipc::{ProtocolError, local_daemon_socket_path},
};

fn proof() -> HostSessionProof {
    HostSessionProof {
        host_session_id: HostSessionId::new("device"),
        boot_id: BootId::new("boot"),
        host_incarnation: HostIncarnation::new("host"),
    }
}

fn respond(stream: &mut UnixStream, request_id: u64, message: ServerMessage) {
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

/// Answer the handshake, serve the expected requests and require EOF.
fn serve(
    mut server: UnixStream,
    mut reply: impl FnMut(ClientRequest) -> ServerMessage,
    requests: usize,
) {
    server
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let connect: ClientEnvelope = read_frame(&mut server).unwrap().unwrap();
    assert!(matches!(connect.request, ClientRequest::Connect { .. }));
    respond(
        &mut server,
        connect.request_id,
        ServerMessage::Response {
            response: ServerResponse::Connected {
                proof: proof(),
                resumed: vec![],
            },
        },
    );
    for _ in 0..requests {
        let request: ClientEnvelope = read_frame(&mut server).unwrap().unwrap();
        let message = reply(request.request);
        respond(&mut server, request.request_id, message);
    }
    assert!(
        read_frame::<ClientEnvelope>(&mut server).unwrap().is_none(),
        "the connection sent a request it was not expected to"
    );
}

fn device(
    reply: impl FnMut(ClientRequest) -> ServerMessage + Send + 'static,
    requests: usize,
) -> (ControlClient, thread::JoinHandle<()>) {
    let (client, server) = UnixStream::pair().unwrap();
    let served = thread::spawn(move || serve(server, reply, requests));
    (ControlClient::from_stream(client).unwrap(), served)
}

fn challenge_then_capability(token: &'static str) -> impl FnMut(ClientRequest) -> ServerMessage {
    move |request| ServerMessage::Response {
        response: match request {
            ClientRequest::PeerChallenge => ServerResponse::PeerChallenge {
                nonce: [3; SECRET_BYTES],
            },
            ClientRequest::PeerAuthenticate { .. } => ServerResponse::PeerAuthenticated {
                authority: Authority::session(token.into()),
            },
            other => panic!("only a proof was expected, not {other:?}"),
        },
    }
}

/// This computer's daemon, vouching for the session.
fn vouches(request: ClientRequest, _: Option<Instant>) -> Result<ServerResponse, ControlError> {
    let ClientRequest::SignDeviceChallenge {
        authority,
        device,
        nonce,
    } = request
    else {
        panic!("only a signature was expected");
    };
    assert_eq!(authority, Authority::session("own-session".into()));
    assert_eq!(device, "devx");
    assert_eq!(nonce, [3; SECRET_BYTES]);
    Ok(ServerResponse::DeviceProof {
        peer: uuid::Uuid::nil(),
        claim: Claim {
            lineage: Lineage(TerminalId::new()),
            ceiling: StatedCeiling {
                provider: None,
                mode: None,
            },
        },
        proof: [4; SECRET_BYTES],
    })
}

/// This computer's daemon, after a person stopped sessions using the device.
fn refuses(_: ClientRequest, _: Option<Instant>) -> Result<ServerResponse, ControlError> {
    let error = ProtocolError::new(
        ProtocolErrorCode::PermissionDenied,
        "Sessions on this computer may not use devx",
        false,
    );
    Err(ControlError::Remote {
        code: error.code,
        message: error.message,
        retryable: error.retryable,
    })
}

fn proved(client: &mut ControlClient, age: Duration) {
    client.device_peer = Some(DevicePeer {
        capability: Authority::session("peer-first".into()),
        proved: Instant::now().checked_sub(age).unwrap(),
        session: Authority::session("own-session".into()),
        device: "devx".into(),
    });
}

#[test]
fn a_proved_connection_proves_itself_again_before_any_request_once_due() {
    let (mut client, served) = device(challenge_then_capability("peer-renewed"), 2);
    // Not yet due: the request goes as it is, with no proof.
    proved(&mut client, Duration::ZERO);
    assert_eq!(
        client
            .for_device_with(ClientRequest::List, vouches, None)
            .unwrap(),
        ClientRequest::List
    );
    // Due, even before a request that carries no authority, such as the
    // snapshots a terminal wait polls with.
    proved(&mut client, CAPABILITY_RENEWAL);
    let before = Instant::now();
    assert_eq!(
        client
            .for_device_with(ClientRequest::List, vouches, None)
            .unwrap(),
        ClientRequest::List
    );
    let peer = client.device_peer.as_ref().unwrap();
    assert_eq!(peer.capability, Authority::session("peer-renewed".into()));
    assert!(peer.proved >= before);
    drop(client);
    served.join().unwrap();
}

#[test]
fn a_refused_renewal_ends_the_connections_use_of_the_device() {
    let (mut client, served) = device(challenge_then_capability("unused"), 1);
    proved(&mut client, CAPABILITY_RENEWAL);
    let error = client
        .for_device_with(ClientRequest::List, refuses, None)
        .unwrap_err();
    assert!(
        matches!(&error, ControlError::DeviceAccess(message) if message == "Sessions on this computer may not use devx"),
        "{error:?}"
    );
    assert!(client.device_peer.is_none());
    drop(client);
    served.join().unwrap();
}

#[test]
fn a_terminal_listing_uses_its_deadline_during_device_renewal() {
    let (client, mut peer) = UnixStream::pair().unwrap();
    thread::scope(|scope| {
        let (finished, result) = std::sync::mpsc::channel();
        scope.spawn(move || {
            let connect: ClientEnvelope = read_frame(&mut peer).unwrap().unwrap();
            respond(
                &mut peer,
                connect.request_id,
                ServerMessage::Response {
                    response: ServerResponse::Connected {
                        proof: proof(),
                        resumed: vec![],
                    },
                },
            );
            let renew: ClientEnvelope = read_frame(&mut peer).unwrap().unwrap();
            assert_eq!(renew.request, ClientRequest::PeerChallenge);
            result.recv_timeout(Duration::from_secs(4)).unwrap();
        });
        let mut client = ControlClient::from_stream(client).unwrap();
        proved(&mut client, CAPABILITY_RENEWAL);
        let result = client.list_until(Instant::now() + Duration::from_secs(2));
        finished.send(()).unwrap();
        assert!(matches!(result, Err(ControlError::TerminalListTimeout)));
        assert!(client.device_peer.is_none());
    });
}

const PRIVATE_CHILD: &str = "CAGENTS_TEST_DEVICE_PEER_CHILD";
const LOCAL_TOKEN: &str = "synthetic-local-session";

#[test]
fn translates_session_authority_over_framed_connections() {
    if std::env::var_os(PRIVATE_CHILD).is_some() {
        exercise_framed_translation();
        return;
    }

    // Device selection is process-wide, and the signer discovers its socket
    // through the environment. Keep both private to this test.
    let root = tempfile::Builder::new().prefix("peer-").tempdir().unwrap();
    let root = root.path().canonicalize().unwrap();
    let selector = thread::current().name().unwrap().to_owned();
    let output = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", &selector, "--nocapture"])
        .env_clear()
        .env(PRIVATE_CHILD, "1")
        .env("HOME", &root)
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_RUNTIME_DIR", root.join("run"))
        .current_dir(&root)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed; 0 failed"));
}

fn exercise_framed_translation() {
    use std::{fs, os::unix::fs::DirBuilderExt};

    let config = crate::device::configuration_path().unwrap();
    fs::create_dir_all(config.parent().unwrap()).unwrap();
    fs::write(
        config,
        br#"{"devices":[{"name":"devx","host":"peer-fixture.invalid"}]}"#,
    )
    .unwrap();
    crate::device::select(Some("devx")).unwrap();

    let socket = local_daemon_socket_path().unwrap();
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(socket.parent().unwrap())
        .unwrap();
    let listener = crate::descriptor::bind(socket).unwrap();

    let authority = Authority::session(LOCAL_TOKEN.into());
    let peer_authority = Authority::session("peer-framed-fixture".into());
    let automation = AutomationRequest::List {
        after: None,
        limit: 7,
    };
    let peer = uuid::Uuid::new_v4();
    let claim = Claim {
        lineage: Lineage(TerminalId::new()),
        ceiling: StatedCeiling {
            provider: None,
            mode: None,
        },
    };
    let signature = [9; SECRET_BYTES];
    let response = |response| ServerMessage::Response { response };
    let nonce = [5; SECRET_BYTES];
    let signer = local_signer(&listener, nonce, peer, claim.clone(), signature);
    let (mut client, served) = framed_device([
        (
            ClientRequest::PeerChallenge,
            response(ServerResponse::PeerChallenge { nonce }),
        ),
        (
            ClientRequest::PeerAuthenticate {
                peer,
                claim: claim.clone(),
                proof: signature,
            },
            response(ServerResponse::PeerAuthenticated {
                authority: peer_authority.clone(),
            }),
        ),
        (
            ClientRequest::Agent {
                authority: peer_authority.clone(),
                operation: Operation::List,
            },
            response(ServerResponse::Agent(Reply::Listed(vec![]))),
        ),
        (
            ClientRequest::Automation {
                authority: peer_authority.clone(),
                operation: automation.clone(),
            },
            response(ServerResponse::Automation(AutomationResponse::Listed(
                vec![],
            ))),
        ),
        (
            ClientRequest::Authenticate {
                authority: peer_authority,
            },
            response(ServerResponse::Accepted),
        ),
    ]);
    assert!(matches!(
        client.agent(authority.clone(), Operation::List).unwrap(),
        Reply::Listed(entries) if entries.is_empty()
    ));
    assert!(matches!(
        client.automation(authority.clone(), automation).unwrap(),
        AutomationResponse::Listed(entries) if entries.is_empty()
    ));
    client.authenticate(authority.clone()).unwrap();
    drop(client);
    served.join().unwrap();
    signer.join().unwrap();

    // A fresh connection needs its own proof. A refused proof must stop
    // the business request, even after a successful connection.
    let nonce = [6; SECRET_BYTES];
    let signature = [10; SECRET_BYTES];
    let signer = local_signer(&listener, nonce, peer, claim.clone(), signature);
    let refusal = "The device refused this computer's proof";
    let (mut client, served) = framed_device([
        (
            ClientRequest::PeerChallenge,
            response(ServerResponse::PeerChallenge { nonce }),
        ),
        (
            ClientRequest::PeerAuthenticate {
                peer,
                claim,
                proof: signature,
            },
            ServerMessage::Error(ProtocolError::new(
                ProtocolErrorCode::PermissionDenied,
                refusal,
                false,
            )),
        ),
    ]);
    let error = client.authenticate(authority).unwrap_err();
    assert!(
        matches!(&error, ControlError::DeviceAccess(message) if message == refusal),
        "{error:?}"
    );
    drop(client);
    served.join().unwrap();
    signer.join().unwrap();
}

fn framed_device<const N: usize>(
    exchanges: [(ClientRequest, ServerMessage); N],
) -> (ControlClient, thread::JoinHandle<()>) {
    let mut exchanges = exchanges.into_iter();
    device(
        move |actual| {
            let (expected, response) = exchanges.next().unwrap();
            assert_eq!(actual, expected);
            response
        },
        N,
    )
}

fn local_signer(
    listener: &std::os::unix::net::UnixListener,
    nonce: [u8; SECRET_BYTES],
    peer: uuid::Uuid,
    claim: Claim,
    proof: [u8; SECRET_BYTES],
) -> thread::JoinHandle<()> {
    let listener = listener.try_clone().unwrap();
    thread::spawn(move || {
        assert!(
            crate::descriptor::wait_for_activity(
                listener.as_fd(),
                libc::POLLIN,
                Some(Instant::now() + Duration::from_secs(5)),
            )
            .unwrap(),
            "the client never connected to the local signer"
        );
        let (stream, _) = listener.accept().unwrap();
        serve(
            stream,
            |request| {
                assert_eq!(
                    request,
                    ClientRequest::SignDeviceChallenge {
                        authority: Authority::session(LOCAL_TOKEN.into()),
                        device: "devx".into(),
                        nonce,
                    }
                );
                ServerMessage::Response {
                    response: ServerResponse::DeviceProof {
                        peer,
                        claim: claim.clone(),
                        proof,
                    },
                }
            },
            1,
        );
    })
}
