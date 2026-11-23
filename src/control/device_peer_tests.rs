//! A connection proved on another device proves itself again before its
//! capability lapses, so this computer's daemon vouches for the session as
//! it is now. The device and this computer's daemon are private fakes.
use super::*;
use crate::{
    agent_execution::{Lineage, permission::StatedCeiling},
    device_trust::{CAPABILITY_RENEWAL, Claim, SECRET_BYTES},
    model::{BootId, HostIncarnation, HostSessionId},
    terminal::ipc::ProtocolError,
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

/// The device: answers `Connect`, then each request in turn with `reply`,
/// and checks that nothing follows.
fn device(
    mut reply: impl FnMut(ClientRequest) -> ServerMessage + Send + 'static,
    requests: usize,
) -> (ControlClient, thread::JoinHandle<()>) {
    let (client, mut server) = UnixStream::pair().unwrap();
    server
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let served = thread::spawn(move || {
        let connect: ClientEnvelope = read_frame(&mut server).unwrap().unwrap();
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
    });
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
fn vouches(request: ClientRequest) -> Result<ServerResponse, ControlError> {
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
fn refuses(_: ClientRequest) -> Result<ServerResponse, ControlError> {
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
            .for_device_with(ClientRequest::List, vouches)
            .unwrap(),
        ClientRequest::List
    );
    // Due, even before a request that carries no authority, such as the
    // snapshots a terminal wait polls with.
    proved(&mut client, CAPABILITY_RENEWAL);
    let before = Instant::now();
    assert_eq!(
        client
            .for_device_with(ClientRequest::List, vouches)
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
        .for_device_with(ClientRequest::List, refuses)
        .unwrap_err();
    assert!(
        matches!(&error, ControlError::DeviceAccess(message) if message == "Sessions on this computer may not use devx"),
        "{error:?}"
    );
    assert!(client.device_peer.is_none());
    drop(client);
    served.join().unwrap();
}
