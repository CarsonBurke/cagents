//! Account UI connections retain the caller's scope at the selected daemon.
use std::path::{Path, PathBuf};

use crate::{
    agent_execution::Authority,
    control::{ControlClient, ControlError},
    device_service::{Operation, Reply},
};

pub(super) fn current(authority: Authority) -> Result<(PathBuf, ControlClient), String> {
    let socket = socket_for(
        &authority,
        crate::device::is_remote(),
        || crate::terminal::ipc::daemon_socket_path().map_err(|error| error.to_string()),
        || crate::terminal::ensure_daemon_ready().map_err(|error| error.to_string()),
    )?;
    let client = at(&socket, authority).map_err(|error| error.to_string())?;
    Ok((socket, client))
}

fn socket_for(
    authority: &Authority,
    remote: bool,
    selected: impl FnOnce() -> Result<PathBuf, String>,
    start: impl FnOnce() -> Result<PathBuf, String>,
) -> Result<PathBuf, String> {
    require_local_scope(authority, remote)?;
    if authority.token().is_some() {
        selected()
    } else {
        start()
    }
}

pub(super) fn at(socket: &Path, authority: Authority) -> Result<ControlClient, ControlError> {
    connect_as(authority, crate::device::is_remote(), || {
        ControlClient::connect(socket)
    })
}

pub(super) fn selected_socket(authority: &Authority) -> Result<PathBuf, String> {
    require_local_scope(authority, crate::device::is_remote())?;
    crate::terminal::ipc::daemon_socket_path().map_err(|error| error.to_string())
}

fn require_local_scope(authority: &Authority, remote: bool) -> Result<(), String> {
    if remote && authority.token().is_some() {
        Err("Account management on another device is unavailable from an agent session".into())
    } else {
        Ok(())
    }
}

fn connect_as(
    authority: Authority,
    remote: bool,
    connect: impl FnOnce() -> Result<ControlClient, ControlError>,
) -> Result<ControlClient, ControlError> {
    require_local_scope(&authority, remote).map_err(|message| ControlError::Remote {
        code: crate::terminal_ipc::ProtocolErrorCode::PermissionDenied,
        message,
        retryable: false,
    })?;
    authenticated(connect()?, authority)
}

fn authenticated(
    mut client: ControlClient,
    authority: Authority,
) -> Result<ControlClient, ControlError> {
    client.authenticate(authority)?;
    Ok(client)
}

pub(super) fn request(authority: Authority, operation: Operation) -> Result<Reply, String> {
    let (_, client) = current(authority)?;
    send(client, operation)
}

fn send(mut client: ControlClient, operation: Operation) -> Result<Reply, String> {
    client
        .device_operation(operation)
        .map_err(|error| error.to_string())
}

/// Account cleanup is a user action. Never enter the shared user close helper
/// from an inherited session capability, including an invalid or empty one.
pub(super) fn require_user(authority: &Authority) -> Result<(), String> {
    if authority.token().is_some() {
        Err("Account management is unavailable from an agent session".into())
    } else {
        Ok(())
    }
}

pub(super) async fn probe(
    authority: Authority,
    operation: impl std::future::Future<Output = crate::account::probe::Outcome>,
) -> crate::account::probe::Outcome {
    match require_user(&authority) {
        Ok(()) => operation.await,
        Err(error) => crate::account::probe::Outcome::Failed(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        account::import::{ImportOperation, ImportReply},
        model::{BootId, HostIncarnation, HostSessionId, HostSessionProof},
        terminal_ipc::{
            ClientEnvelope, ClientRequest, PROTOCOL_VERSION, ProtocolError, ProtocolErrorCode,
            ServerEnvelope, ServerMessage, ServerResponse, read_frame, write_frame,
        },
    };
    use std::{cell::Cell, os::unix::net::UnixStream, thread, time::Duration};

    fn proof() -> HostSessionProof {
        HostSessionProof {
            host_session_id: HostSessionId::new("account-connection-fixture"),
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

    fn handshake(peer: &mut UnixStream) {
        peer.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
        peer.set_write_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let hello: ClientEnvelope = read_frame(peer).unwrap().unwrap();
        assert!(matches!(hello.request, ClientRequest::Connect { .. }));
        reply(
            peer,
            &hello,
            ServerMessage::Response {
                response: ServerResponse::Connected {
                    proof: proof(),
                    resumed: vec![],
                },
            },
        );
    }

    fn denied() -> ServerMessage {
        ServerMessage::Error(ProtocolError::new(
            ProtocolErrorCode::PermissionDenied,
            "The fixture refused this scope",
            false,
        ))
    }

    #[test]
    fn scoped_account_connections_never_start_a_daemon_and_user_keeps_startup() {
        for authority in [
            Authority::session("fixture-session-scope".into()),
            Authority::session(String::new()),
        ] {
            let starts = Cell::new(0);
            let result = socket_for(
                &authority,
                false,
                || Err("selected daemon unavailable".into()),
                || {
                    starts.set(starts.get() + 1);
                    Ok("/must-not-start".into())
                },
            );
            assert_eq!(result, Err("selected daemon unavailable".into()));
            assert_eq!(starts.get(), 0);
        }
        let starts = Cell::new(0);
        let socket = socket_for(
            &Authority::default(),
            false,
            || panic!("ordinary user keeps the existing ready/start path"),
            || {
                starts.set(starts.get() + 1);
                Ok("/fixture/user-selected.socket".into())
            },
        )
        .unwrap();
        assert_eq!(socket, PathBuf::from("/fixture/user-selected.socket"));
        assert_eq!(starts.get(), 1);
    }

    #[test]
    fn scoped_remote_accounts_refuse_before_path_start_or_connect() {
        for authority in [
            Authority::session("fixture-scope".into()),
            Authority::session(String::new()),
        ] {
            assert!(
                socket_for(
                    &authority,
                    true,
                    || panic!("a scoped remote request must not establish a bridge"),
                    || panic!("a scoped remote request must not start a daemon"),
                )
                .is_err()
            );
            assert!(
                connect_as(authority, true, || panic!(
                    "a pinned remote target must not connect"
                ),)
                .is_err()
            );
        }
        let selected = Cell::new(false);
        let result = socket_for(
            &Authority::default(),
            true,
            || panic!("ordinary User retains the existing ready path"),
            || {
                selected.set(true);
                Ok("/fixture/remote.socket".into())
            },
        )
        .unwrap();
        assert!(selected.get());
        assert_eq!(result, PathBuf::from("/fixture/remote.socket"));
    }

    #[test]
    fn user_account_connection_authenticates_before_its_single_device_request() {
        let (stream, mut peer) = UnixStream::pair().unwrap();
        let worker = thread::spawn(move || {
            handshake(&mut peer);
            let auth: ClientEnvelope = read_frame(&mut peer).unwrap().unwrap();
            assert_eq!(auth.proof, Some(proof()));
            assert!(
                matches!(&auth.request, ClientRequest::Authenticate { authority } if authority == &Authority::default())
            );
            reply(
                &mut peer,
                &auth,
                ServerMessage::Response {
                    response: ServerResponse::Accepted,
                },
            );
            let request: ClientEnvelope = read_frame(&mut peer).unwrap().unwrap();
            assert_eq!(request.proof, Some(proof()));
            assert!(matches!(
                &request.request,
                ClientRequest::DeviceOperation {
                    operation: Operation::AccountImport(ImportOperation::List)
                }
            ));
            reply(
                &mut peer,
                &request,
                ServerMessage::Response {
                    response: ServerResponse::DeviceOperation {
                        reply: Reply::AccountImport(Box::new(ImportReply::Listed(Vec::new()))),
                    },
                },
            );
            assert!(read_frame::<ClientEnvelope>(&mut peer).unwrap().is_none());
        });
        let client = authenticated(
            ControlClient::from_stream(stream).unwrap(),
            Authority::default(),
        )
        .unwrap();
        assert_eq!(client.proof(), &proof());
        assert!(
            matches!(send(client, Operation::AccountImport(ImportOperation::List)).unwrap(), Reply::AccountImport(result) if *result == ImportReply::Listed(Vec::new()))
        );
        worker.join().unwrap();
    }

    #[test]
    fn refused_account_authentication_never_exposes_a_client_or_dispatches() {
        for authority in [
            Authority::session("unregistered-fixture-scope".into()),
            Authority::session(String::new()),
        ] {
            let expected = authority.clone();
            let (stream, mut peer) = UnixStream::pair().unwrap();
            let worker = thread::spawn(move || {
                handshake(&mut peer);
                let auth: ClientEnvelope = read_frame(&mut peer).unwrap().unwrap();
                assert!(
                    matches!(&auth.request, ClientRequest::Authenticate { authority } if authority == &expected)
                );
                reply(&mut peer, &auth, denied());
                assert!(
                    read_frame::<ClientEnvelope>(&mut peer).unwrap().is_none(),
                    "no account request or user retry after authentication refusal"
                );
            });
            let result = authenticated(ControlClient::from_stream(stream).unwrap(), authority);
            assert!(matches!(result, Err(ControlError::Remote { .. })));
            worker.join().unwrap();
        }
    }

    #[test]
    fn authenticated_account_device_refusal_never_retries_as_user() {
        let authority = Authority::session("registered-fixture-scope".into());
        let expected = authority.clone();
        let (stream, mut peer) = UnixStream::pair().unwrap();
        let worker = thread::spawn(move || {
            handshake(&mut peer);
            let auth: ClientEnvelope = read_frame(&mut peer).unwrap().unwrap();
            assert!(
                matches!(&auth.request, ClientRequest::Authenticate { authority } if authority == &expected)
            );
            reply(
                &mut peer,
                &auth,
                ServerMessage::Response {
                    response: ServerResponse::Accepted,
                },
            );
            let request: ClientEnvelope = read_frame(&mut peer).unwrap().unwrap();
            assert!(matches!(
                &request.request,
                ClientRequest::DeviceOperation {
                    operation: Operation::AccountImport(ImportOperation::List)
                }
            ));
            reply(&mut peer, &request, denied());
            assert!(
                read_frame::<ClientEnvelope>(&mut peer).unwrap().is_none(),
                "a scoped account refusal never opens a user retry"
            );
        });
        let client = authenticated(ControlClient::from_stream(stream).unwrap(), authority).unwrap();
        assert!(send(client, Operation::AccountImport(ImportOperation::List)).is_err());
        worker.join().unwrap();
    }

    #[test]
    fn account_cleanup_refuses_any_session_scope_before_the_user_helper() {
        assert!(require_user(&Authority::default()).is_ok());
        for authority in [
            Authority::session("fixture-session-scope".into()),
            Authority::session(String::new()),
        ] {
            assert!(require_user(&authority).is_err());
        }
    }
    #[tokio::test]
    async fn scoped_local_probe_refuses_without_polling_provider_work() {
        use crate::account::probe::Outcome;
        for authority in [
            Authority::session("fixture-scope".into()),
            Authority::session(String::new()),
        ] {
            let polled = Cell::new(false);
            let result = probe(authority, async {
                polled.set(true);
                Outcome::Busy
            })
            .await;
            assert!(matches!(result, Outcome::Failed(_)));
            assert!(!polled.get());
        }
        let polled = Cell::new(false);
        assert!(matches!(
            probe(Authority::default(), async {
                polled.set(true);
                Outcome::Busy
            })
            .await,
            Outcome::Busy
        ));
        assert!(polled.get());
    }
}
