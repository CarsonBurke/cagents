use super::super::*;
use crate::{
    account::{
        AccountId,
        remote::{Archived, Operation as Lifecycle, RemovalPlan},
    },
    device_service::{Operation, Reply},
};
use serde::Deserialize;

#[derive(Deserialize)]
struct Revision {
    revision: u64,
}

pub(super) fn execute(account: AccountId, confirmed: bool) -> Result<CommandOutput, AppError> {
    confirm(&account, confirmed)?;
    let mut client = control_daemon_client()?;
    execute_with(&mut client, account)
}

fn confirm(account: &AccountId, confirmed: bool) -> Result<(), AppError> {
    if !confirmed {
        return Err(AppError::new(
            "confirmation",
            format!(
                "Pass --yes to remove {account} from the selected device. Linked files are kept; managed file deletion is unavailable."
            ),
        ));
    }
    Ok(())
}

fn execute_with(client: &mut ControlClient, account: AccountId) -> Result<CommandOutput, AppError> {
    let value = request(
        client,
        Lifecycle::Remove {
            id: account.clone(),
        },
        "account_remove",
    )?;
    let (registry, error): (Revision, Option<String>) = serde_json::from_value(value)
        .map_err(|error| AppError::new("account_remove", format!("Invalid account removal response: {error}; inspect account list before retrying")))?;
    if let Some(error) = error {
        return Err(AppError::new(
            "account_remove",
            format!(
                "The daemon reported incomplete removal: {error}; inspect account list before retrying"
            ),
        ));
    }
    Ok(CommandOutput {
        command: "account.remove",
        human: format!("Removed account registration: {account}"),
        data: json!({"account": account, "registration_removed": true, "revision": registry.revision}),
    })
}

pub(super) fn plan(account: AccountId) -> Result<CommandOutput, AppError> {
    let mut client = control_daemon_client()?;
    let RemovalPlan { conversations } = read_plan(&mut client, &account)?;
    Ok(CommandOutput {
        command: "account.removal-plan",
        human: format!("{account} has {conversations} saved conversations."),
        data: json!({"account": account, "conversations": conversations}),
    })
}

fn read_plan(client: &mut ControlClient, account: &AccountId) -> Result<RemovalPlan, AppError> {
    let value = request(
        client,
        Lifecycle::RemovalPlan {
            id: account.clone(),
        },
        "account_removal_plan",
    )?;
    serde_json::from_value(value).map_err(|error| {
        AppError::new(
            "account_removal_plan",
            format!("Invalid removal plan response: {error}"),
        )
    })
}

pub(super) fn archive(
    account: AccountId,
    move_to: Option<AccountId>,
    confirmed: bool,
) -> Result<CommandOutput, AppError> {
    if !confirmed {
        return Err(AppError::new(
            "confirmation",
            "Pass --yes to confirm signing out and archiving the account",
        ));
    }
    let mut client = control_daemon_client()?;
    let value = request(
        &mut client,
        Lifecycle::Archive {
            id: account.clone(),
            move_to: move_to.clone(),
        },
        "account_archive",
    )?;
    let Archived { revision, moved } = serde_json::from_value(value).map_err(|error| {
        AppError::new(
            "account_archive",
            format!("Invalid archive response: {error}. Inspect account list before retrying."),
        )
    })?;
    let human = match &move_to {
        Some(target) => format!("Copied {moved} conversations to {target} and archived {account}."),
        None => format!("Archived {account} and kept its conversation history."),
    };
    Ok(CommandOutput {
        command: "account.archive",
        human,
        data: json!({"account": account, "archived": true, "moved": moved, "move_to": move_to, "revision": revision}),
    })
}

fn request(
    client: &mut ControlClient,
    operation: Lifecycle,
    error_kind: &'static str,
) -> Result<serde_json::Value, AppError> {
    client
        .authenticate(Authority::from_environment())
        .map_err(protocol_error)?;
    match client
        .device_operation(Operation::AccountLifecycle { operation })
        .map_err(protocol_error)?
    {
        Reply::AccountLifecycle(value) => Ok(value),
        _ => Err(AppError::new(
            error_kind,
            "Unexpected account lifecycle response. Inspect account list before retrying.",
        )),
    }
}

#[cfg(all(test, not(any(feature = "host", feature = "ui"))))]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn account_remove_requires_a_stable_id_and_explicit_confirmation() {
        let id: AccountId = "claude/00000000-0000-4000-8000-000000000001"
            .parse()
            .unwrap();
        assert!(
            Cli::try_parse_from(["cagents", "account", "remove", id.as_str(), "--yes"]).is_ok()
        );
        assert!(Cli::try_parse_from(["cagents", "account", "remove", id.as_str()]).is_err());
        assert!(Cli::try_parse_from(["cagents", "account", "remove", "/a/home", "--yes"]).is_err());
        let Err(error) = execute(id, false) else {
            panic!("unconfirmed removal was accepted");
        };
        assert_eq!(error.kind, "confirmation");
        assert!(error.message.contains("--yes"));
    }

    #[test]
    fn archive_requires_confirmation_and_stable_account_ids() {
        let id = "codex/00000000-0000-4000-8000-000000000001";
        assert!(Cli::try_parse_from(["cagents", "account", "removal-plan", id]).is_ok());
        assert!(Cli::try_parse_from(["cagents", "account", "archive", id]).is_err());
        assert!(
            Cli::try_parse_from([
                "cagents",
                "account",
                "archive",
                id,
                "--move-to",
                "codex/system",
                "--yes"
            ])
            .is_ok()
        );
        assert!(
            Cli::try_parse_from([
                "cagents",
                "account",
                "archive",
                id,
                "--move-to",
                "/home",
                "--yes"
            ])
            .is_err()
        );
    }

    #[test]
    fn account_remove_uses_authenticated_lifecycle_without_retry_or_registry_replacement() {
        use crate::{
            model::{BootId, HostIncarnation, HostSessionId, HostSessionProof},
            terminal_ipc::{
                ClientEnvelope, ClientRequest, PROTOCOL_VERSION, ProtocolError, ProtocolErrorCode,
                ServerEnvelope, ServerMessage, ServerResponse, read_frame, write_frame,
            },
        };
        use std::{os::unix::net::UnixStream, thread};
        // Success is the existing linked-account unregister response. Every
        // refusal/ambiguous reply stays an error rather than a local fallback.
        for outcome in 0..5 {
            let id: AccountId = "claude/00000000-0000-4000-8000-000000000001"
                .parse()
                .unwrap();
            let original = id.clone();
            let (stream, mut peer) = UnixStream::pair().unwrap();
            peer.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
            peer.set_write_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let worker = thread::spawn(move || {
                let proof = HostSessionProof {
                    host_session_id: HostSessionId::new("remove-peer"),
                    boot_id: BootId::new("private-boot"),
                    host_incarnation: HostIncarnation::new("private-incarnation"),
                };
                for step in 0..3 {
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
                                operation:
                                    Operation::AccountLifecycle {
                                        operation: crate::account::remote::Operation::Remove { id },
                                    },
                            },
                        ) if id == original => ServerResponse::DeviceOperation {
                            reply: Reply::AccountLifecycle(match outcome {
                                3 => json!([{"revision": 8}, "incomplete cleanup"]),
                                4 => json!({"unexpected": true}),
                                _ => {
                                    json!([{"revision": 8, "account": [{"linked_home": "/metadata-only"}]}, null])
                                }
                            }),
                        },
                        _ => panic!("unexpected lifecycle request"),
                    };
                    let message = if step == 2 && (outcome == 1 || outcome == 2) {
                        ServerMessage::Error(ProtocolError::new(
                            if outcome == 1 {
                                ProtocolErrorCode::PermissionDenied
                            } else {
                                ProtocolErrorCode::InvalidRequest
                            },
                            if outcome == 1 {
                                "agent tokens cannot remove accounts"
                            } else {
                                "File deletion is not available in this build. Your account and its files were kept."
                            },
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
                    "removal must not automatically retry"
                );
            });
            let mut client = ControlClient::from_stream(stream);
            let result = match &mut client {
                Ok(client) => execute_with(client, id.clone()),
                Err(_) => Err(AppError::new("fixture", "handshake failed")),
            };
            drop(client);
            worker.join().unwrap();
            if outcome == 0 {
                let output = result.unwrap();
                assert_eq!(
                    output.data,
                    json!({"account": id, "registration_removed": true, "revision": 8})
                );
                assert_eq!(output.human, format!("Removed account registration: {id}"));
                assert!(!output.data.to_string().contains("linked_home"));
            } else {
                let Err(error) = result else {
                    panic!("refused or invalid removal was accepted");
                };
                assert!(
                    error.message.contains(match outcome {
                        1 => "agent tokens",
                        2 => "files were kept",
                        3 => "incomplete removal",
                        _ => "Invalid account removal response",
                    }),
                    "{}",
                    error.message
                );
            }
        }
    }
}
