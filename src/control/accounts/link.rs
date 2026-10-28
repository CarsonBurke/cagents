//! Registration reuses the selected daemon's lifecycle admission; the client
//! never opens a provider home or submits a registry replacement.
use super::super::*;
use crate::{
    account::AccountId,
    agent::Provider,
    device_service::{Operation, Reply},
};
use serde::Deserialize;

#[derive(Deserialize)]
struct Revision {
    revision: u64,
}

pub(super) fn parse_provider(value: &str) -> Result<Provider, String> {
    let provider = crate::control::agents::parse_provider(value)
        .map_err(|_| "Choose claude or codex".to_owned())?;
    if !crate::account::Account::PROVIDERS.contains(&provider) {
        return Err("Omp manages its own logins; link supports claude and codex".into());
    }
    Ok(provider)
}

pub(super) fn parse_home(value: &str) -> Result<String, String> {
    // The destination's established linker trims text-field input. Refuse an
    // ambiguous CLI operand rather than silently selecting a different home.
    if value.trim() != value {
        return Err("Use an account home without leading or trailing whitespace".into());
    }
    if value
        .strip_prefix("~/")
        .is_some_and(|relative| Path::new(relative).is_absolute())
    {
        return Err("Use one slash after ~ so the account home stays relative to the selected device's home".into());
    }
    if value.contains('\0') || !(Path::new(value).is_absolute() || value.starts_with("~/")) {
        return Err(
            "Use an absolute account home on the selected device, or a quoted '~/path'".into(),
        );
    }
    // Expansion and physical-home validation belong to the selected daemon.
    // In particular, a remote ~/ path must not use this client's HOME.
    Ok(value.into())
}

pub(super) fn execute(provider: Provider, home: String) -> Result<CommandOutput, AppError> {
    parse_provider(provider.executable()).map_err(|error| AppError::new("account_link", error))?;
    let home = parse_home(&home).map_err(|error| AppError::new("account_link", error))?;
    let socket = control_daemon_socket()?;
    let mut client = ControlClient::connect(&socket)
        .map_err(|error| AppError::new("daemon_connection", error))?;
    execute_with(&mut client, provider, home)
}

fn execute_with(
    client: &mut ControlClient,
    provider: Provider,
    home: String,
) -> Result<CommandOutput, AppError> {
    // Retain the caller's authority: hosted agent tokens must remain subject
    // to the existing user-only lifecycle gate.
    client
        .authenticate(Authority::from_environment())
        .map_err(protocol_error)?;
    let reply = client
        .device_operation(Operation::AccountLifecycle {
            operation: crate::account::remote::Operation::Link {
                provider,
                key: uuid::Uuid::new_v4(),
                path: home,
            },
        })
        .map_err(|error| {
            let mut error = protocol_error(error);
            error
                .message
                .push_str("; inspect account list on the same device before retrying the link");
            error
        })?;
    let Reply::AccountLifecycle(value) = reply else {
        return Err(AppError::new(
            "account_link",
            "Unexpected account link response; inspect account list on the same device before retrying",
        ));
    };
    // Decode only receipt metadata. Homes, account defaults and identities in
    // the registry reply are neither client authority nor public CLI output.
    let (registry, key): (Revision, uuid::Uuid) = serde_json::from_value(value).map_err(|_| {
        AppError::new(
            "account_link",
            "Invalid account link response; inspect account list on the same device before retrying",
        )
    })?;
    let id: AccountId = format!("{}/{key}", provider.executable())
        .parse()
        .map_err(|error| AppError::new("account_link", error))?;
    Ok(CommandOutput {
        command: "account.link",
        human: format!(
            "Linked account registration: {id}\nAuthentication and execution readiness have not been checked."
        ),
        data: json!({
            "account": id,
            "revision": registry.revision,
            "status": "registered",
            "authentication": "not_checked",
            "execution_readiness": "not_checked",
        }),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn link_grammar_preserves_destination_paths_and_rejects_unsupported_inputs() {
        for (device, provider, home) in [
            ("local", "codex", "/private/account home"),
            ("fw16", "Claude", "~/account homes/selected"),
        ] {
            let cli = Cli::try_parse_from([
                "cagents", "--device", device, "account", "link", provider, home,
            ])
            .unwrap();
            assert_eq!(cli.device.as_deref(), Some(device));
            let TopCommand::Account {
                command: AccountCommand::Link { home: parsed, .. },
            } = cli.command
            else {
                panic!("wrong account command");
            };
            assert_eq!(parsed, home);
        }
        // The execution entry point also refuses before socket lookup, even
        // when called without clap (for example by another command).
        for (provider, home) in [
            (Provider::Omp, "/private/home"),
            (Provider::Codex, "relative/home"),
            (Provider::Codex, "~//selected"),
            (Provider::Codex, "/private/selected "),
            (Provider::Codex, " ~/selected"),
        ] {
            let Err(error) = execute(provider, home.into()) else {
                panic!("invalid input reached account registration");
            };
            assert_eq!(error.kind, "account_link");
        }
        for (provider, home) in [
            ("omp", "/private/home"),
            ("unknown", "/private/home"),
            ("codex", "relative/home"),
            ("codex", "~another/home"),
            ("codex", "~//selected"),
            ("codex", "~///selected"),
            ("codex", "/private/selected "),
            ("codex", " ~/selected"),
            ("codex", "/private/selected\u{2003}"),
            ("codex", "~"),
            ("codex", ""),
            ("codex", "/private/\0home"),
        ] {
            assert!(
                Cli::try_parse_from(["cagents", "account", "link", provider, home]).is_err(),
                "accepted {provider} {home:?}"
            );
        }
    }

    #[test]
    fn link_uses_one_authenticated_lifecycle_request_and_returns_only_registration_metadata() {
        use crate::{
            model::{BootId, HostIncarnation, HostSessionId, HostSessionProof},
            terminal_ipc::{
                ClientEnvelope, ClientRequest, PROTOCOL_VERSION, ProtocolError, ProtocolErrorCode,
                ServerEnvelope, ServerMessage, ServerResponse, read_frame, write_frame,
            },
        };
        use std::{os::unix::net::UnixStream, thread};
        // Rejected authority, live-home refusal, incompatible and uncertain
        // replies must stay errors. No retry or local-registry fallback.
        for outcome in 0..6 {
            let (stream, mut peer) = UnixStream::pair().unwrap();
            peer.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
            peer.set_write_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let returned_key = uuid::Uuid::new_v4();
            let worker = thread::spawn(move || {
                let proof = HostSessionProof {
                    host_session_id: HostSessionId::new("link-peer"),
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
                        (1, ClientRequest::Authenticate { authority }) => {
                            assert_eq!(authority, Authority::from_environment());
                            ServerResponse::Accepted
                        }
                        (
                            2,
                            ClientRequest::DeviceOperation {
                                operation:
                                    Operation::AccountLifecycle {
                                        operation:
                                            crate::account::remote::Operation::Link {
                                                provider: Provider::Codex,
                                                key,
                                                path,
                                            },
                                    },
                            },
                        ) => {
                            assert!(!key.is_nil());
                            assert_eq!(path, "~/selected account");
                            // A restored archived registration can return its
                            // original key, rather than the client's fresh key.
                            ServerResponse::DeviceOperation {
                                reply: if outcome == 4 {
                                    Reply::AccountRegistry(json!({"revision": 8}))
                                } else {
                                    Reply::AccountLifecycle(if outcome == 3 {
                                        json!([{"revision": "private invalid value"}, returned_key])
                                    } else {
                                        json!([{"revision": 8, "defaults": {"codex": "unchanged"},
                                            "account": [{"linked_home": "/private/metadata-only"}]}, returned_key])
                                    })
                                },
                            }
                        }
                        _ => panic!("unexpected request"),
                    };
                    if step == 2 && outcome == 5 {
                        // Simulate a lost reply after a possibly committed link.
                        return;
                    }
                    let message = if step == 2 && matches!(outcome, 1 | 2) {
                        ServerMessage::Error(ProtocolError::new(
                            if outcome == 1 {
                                ProtocolErrorCode::PermissionDenied
                            } else {
                                ProtocolErrorCode::InvalidRequest
                            },
                            if outcome == 1 {
                                "agent authority cannot link accounts"
                            } else {
                                "the selected home has a live owner"
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
                    "link must not retry or read/write a replacement registry"
                );
            });
            let mut client = ControlClient::from_stream(stream).unwrap();
            let result = execute_with(&mut client, Provider::Codex, "~/selected account".into());
            drop(client);
            worker.join().unwrap();
            if outcome == 0 {
                let output = result.unwrap();
                assert_eq!(output.command, "account.link");
                assert_eq!(
                    output.data,
                    json!({
                        "account": format!("codex/{returned_key}"), "revision": 8,
                        "status": "registered", "authentication": "not_checked",
                        "execution_readiness": "not_checked",
                    })
                );
                assert!(output.human.contains("have not been checked"));
                assert!(!output.data.to_string().contains("metadata-only"));
            } else {
                let Err(error) = result else {
                    panic!("refused or uncertain link was accepted");
                };
                assert!(
                    error.message.contains(match outcome {
                        1 => "agent authority",
                        2 => "live owner",
                        3 => "Invalid account link response",
                        4 => "Unexpected account link response",
                        _ => "inspect account list",
                    }),
                    "{}",
                    error.message
                );
                assert!(!error.message.contains("private invalid value"));
            }
        }
    }
}
