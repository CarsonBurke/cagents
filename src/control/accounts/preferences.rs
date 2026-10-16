use super::super::*;
use crate::{
    account::preferences::DefaultSelection,
    agent::Provider,
    device_service::{Operation, Reply},
};
use serde::Deserialize;

#[derive(Deserialize)]
struct Revision {
    revision: u64,
}
#[derive(Deserialize)]
struct Snapshot {
    registry: Revision,
}

pub(super) fn execute(
    provider: Provider,
    selection: DefaultSelection,
) -> Result<CommandOutput, AppError> {
    let socket = control_daemon_socket()?;
    let mut client = ControlClient::connect(&socket)
        .map_err(|error| AppError::new("daemon_connection", error))?;
    // Bind the real caller before preferences reads/writes. Device operations
    // remain user-only under the existing daemon token policy.
    execute_with(&mut client, provider, selection)
}

fn execute_with(
    client: &mut ControlClient,
    provider: Provider,
    selection: DefaultSelection,
) -> Result<CommandOutput, AppError> {
    client
        .authenticate(Authority::from_environment())
        .map_err(protocol_error)?;
    let Reply::Accounts(snapshot) = client
        .device_operation(Operation::AccountsRead)
        .map_err(protocol_error)?
    else {
        return Err(AppError::new(
            "account_default",
            "Unexpected account registry response",
        ));
    };
    // Decode only revision metadata; no client registry/home becomes authority.
    let snapshot: Snapshot = serde_json::from_value(snapshot).map_err(|error| {
        AppError::new(
            "account_default",
            format!("Invalid account revision: {error}"),
        )
    })?;
    let expected_revision = snapshot.registry.revision;
    let Reply::AccountRegistry(registry) = client
        .device_operation(Operation::AccountDefault {
            expected_revision,
            provider,
            selection: selection.clone(),
        })
        .map_err(protocol_error)?
    else {
        return Err(AppError::new(
            "account_default",
            "Unexpected saved account registry response",
        ));
    };
    let saved: Revision = serde_json::from_value(registry).map_err(|error| {
        AppError::new(
            "account_default",
            format!("Invalid saved revision: {error}"),
        )
    })?;
    if saved.revision != expected_revision
        && expected_revision.checked_add(1) != Some(saved.revision)
    {
        return Err(AppError::new(
            "account_default",
            "Unexpected saved account revision; inspect account settings before retrying",
        ));
    }
    Ok(output(provider, &selection, saved.revision))
}

fn output(provider: Provider, selection: &DefaultSelection, revision: u64) -> CommandOutput {
    let selected = match selection {
        DefaultSelection::Automatic => "automatic".to_owned(),
        DefaultSelection::Account(id) => id.to_string(),
    };
    CommandOutput {
        command: "account.default",
        human: format!("Default for {}: {selected}", provider.executable()),
        data: json!({"provider": provider.executable(), "selection": selected, "revision": revision}),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn account_default_grammar_accepts_dynamic_ids_and_automatic() {
        for choice in [
            "automatic",
            "AUTOMATIC",
            "claude/system",
            "claude/00000000-0000-4000-8000-000000000001",
        ] {
            assert!(
                Cli::try_parse_from(["cagents", "account", "default", "Claude", choice]).is_ok()
            );
        }
        assert!(
            Cli::try_parse_from(["cagents", "account", "default", "claude", "/home/foreign"])
                .is_err()
        );
        assert!(
            Cli::try_parse_from(["cagents", "account", "default", "unknown", "automatic"]).is_err()
        );
    }

    #[test]
    fn account_default_output_contains_only_preference_metadata() {
        let automatic = output(Provider::Claude, &DefaultSelection::Automatic, 7);
        assert_eq!(automatic.human, "Default for claude: automatic");
        assert_eq!(
            automatic.data,
            json!({"provider":"claude","selection":"automatic","revision":7})
        );
        let explicit = output(
            Provider::Codex,
            &DefaultSelection::Account("codex/system".parse().unwrap()),
            8,
        );
        assert_eq!(
            explicit.data,
            json!({"provider":"codex","selection":"codex/system","revision":8})
        );
    }

    #[test]
    fn account_default_authenticates_and_sends_only_the_original_revision_and_id() {
        use crate::{
            model::{BootId, HostIncarnation, HostSessionId, HostSessionProof},
            terminal_ipc::{
                ClientEnvelope, ClientRequest, PROTOCOL_VERSION, ProtocolError, ProtocolErrorCode,
                ServerEnvelope, ServerMessage, ServerResponse, read_frame, write_frame,
            },
        };
        use std::{os::unix::net::UnixStream, thread};
        for refused in [false, true] {
            let (stream, mut peer) = UnixStream::pair().unwrap();
            peer.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
            peer.set_write_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let worker = thread::spawn(move || {
                let proof = HostSessionProof {
                    host_session_id: HostSessionId::new("default-peer"),
                    boot_id: BootId::new("private-boot"),
                    host_incarnation: HostIncarnation::new("private-incarnation"),
                };
                for step in 0..4 {
                    let request: ClientEnvelope = read_frame(&mut peer).unwrap().unwrap();
                    if step > 0 {
                        assert_eq!(request.proof.as_ref(), Some(&proof));
                    }
                    let response = match (step, request.request) {
                        (0, ClientRequest::Connect { .. }) => ServerResponse::Connected {
                            proof: proof.clone(),
                            resumed: vec![],
                        },
                        (1, ClientRequest::Authenticate { .. }) => ServerResponse::Accepted,
                        (
                            2,
                            ClientRequest::DeviceOperation {
                                operation: Operation::AccountsRead,
                            },
                        ) => ServerResponse::DeviceOperation {
                            reply: Reply::Accounts(
                                json!({"registry":{"revision":7,"account":[{"linked_home":"/metadata-is-not-authority"}]},"homes":"/must-not-be-used"}),
                            ),
                        },
                        (
                            3,
                            ClientRequest::DeviceOperation {
                                operation:
                                    Operation::AccountDefault {
                                        expected_revision: 7,
                                        provider: Provider::Codex,
                                        selection: DefaultSelection::Account(id),
                                    },
                            },
                        ) if id.as_str() == "codex/system" => ServerResponse::DeviceOperation {
                            reply: Reply::AccountRegistry(
                                json!({"revision":8,"defaults":{"codex":"system"}}),
                            ),
                        },
                        _ => panic!("unexpected request or client-supplied home authority"),
                    };
                    let message = if refused && step == 3 {
                        ServerMessage::Error(ProtocolError::new(
                            ProtocolErrorCode::InvalidRequest,
                            "accounts changed on the device; reload before saving",
                            false,
                        ))
                    } else {
                        ServerMessage::Response { response }
                    };
                    write_frame(
                        &mut peer,
                        &ServerEnvelope {
                            version: PROTOCOL_VERSION,
                            request_id: Some(request.request_id),
                            proof: proof.clone(),
                            message,
                        },
                    )
                    .unwrap();
                }
                assert!(
                    read_frame::<ClientEnvelope>(&mut peer).unwrap().is_none(),
                    "a refused preference must never retry automatically"
                );
            });
            let mut client = ControlClient::from_stream(stream);
            let result = match &mut client {
                Ok(client) => execute_with(
                    client,
                    Provider::Codex,
                    DefaultSelection::Account("codex/system".parse().unwrap()),
                ),
                Err(_) => Err(AppError::new("fixture", "private handshake failed")),
            };
            drop(client);
            let joined = worker.join();
            joined.unwrap();
            if refused {
                let Err(error) = result else {
                    panic!("stale revision was accepted");
                };
                assert!(error.message.contains("reload before saving"));
            } else {
                assert_eq!(
                    result.unwrap().data,
                    json!({"provider":"codex","selection":"codex/system","revision":8})
                );
            }
        }
    }
}
