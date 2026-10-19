//! User-authorized resident handle management; never attaches or interrupts work.
use super::*;
use crate::terminal_ipc::{ChatCapacity, ChatReleaseInspection};

#[derive(Debug, clap::Subcommand)]
pub enum ChatCommand {
    /// Show resident and spawning counts, or persist a new admission limit.
    Capacity {
        #[command(subcommand)]
        command: Option<CapacityCommand>,
    },
    /// Inspect or release one exact completed resident handle, preserving saved history.
    Release {
        /// Full native chat ID, as shown by sessions --all.
        id: TerminalId,
        /// Inspect current eligibility without releasing anything.
        #[arg(long, conflicts_with = "yes", required_unless_present = "yes")]
        dry_run: bool,
        /// Explicitly authorize releasing this inspected handle.
        #[arg(long, conflicts_with = "dry_run")]
        yes: bool,
    },
}

#[derive(Debug, clap::Subcommand)]
pub enum CapacityCommand {
    /// Persist a limit from 1 to 4096; existing residents are never evicted.
    Set { limit: u32 },
}

fn validate_command(command: &ChatCommand) -> Result<(), AppError> {
    match command {
        ChatCommand::Capacity {
            command: Some(CapacityCommand::Set { limit }),
        } => crate::native_chat_capacity::validate(*limit)
            .map_err(|e| AppError::new("chat_capacity", e)),
        ChatCommand::Release {
            dry_run: false,
            yes: false,
            ..
        } => Err(AppError::new(
            "confirmation",
            "Pass --dry-run to inspect, or --yes to authorize releasing one completed resident handle",
        )),
        _ => Ok(()),
    }
}

pub(super) fn execute(command: ChatCommand) -> Result<CommandOutput, AppError> {
    validate_command(&command)?;
    let socket = control_daemon_socket()?;
    let mut client =
        ControlClient::connect(&socket).map_err(|e| AppError::new("daemon_connection", e))?;
    client
        .authenticate(Authority::from_environment())
        .map_err(protocol_error)?;
    execute_requests(command, |request| client.request_strict(request))
}

fn execute_requests(
    command: ChatCommand,
    mut request: impl FnMut(ClientRequest) -> Result<ServerResponse, ControlError>,
) -> Result<CommandOutput, AppError> {
    validate_command(&command)?;
    match command {
        ChatCommand::Capacity { command } => {
            let request_value = match command {
                None => ClientRequest::ChatCapacityInspect,
                Some(CapacityCommand::Set { limit }) => ClientRequest::ChatCapacitySet { limit },
            };
            let response = request(request_value).map_err(protocol_error)?;
            let ServerResponse::ChatCapacity { capacity } = response else {
                return Err(protocol_error(unexpected_response(
                    "native chat capacity",
                    &response,
                )));
            };
            Ok(capacity_output(capacity))
        }
        ChatCommand::Release { id, dry_run, .. } => {
            let response = request(ClientRequest::ChatReleaseInspect { chat_id: id })
                .map_err(protocol_error)?;
            let ServerResponse::ChatReleaseInspected { inspection } = response else {
                return Err(protocol_error(unexpected_response(
                    "native chat release inspection",
                    &response,
                )));
            };
            if inspection.proof.session.id != id {
                return Err(AppError::new(
                    "chat_release",
                    "The inspected identity did not match the requested chat; nothing was released",
                ));
            }
            if dry_run {
                return inspection_output(inspection, true);
            }
            if let Some(reason) = &inspection.release_blocked {
                return Err(AppError::new(
                    "chat_release_blocked",
                    format!("Chat {id} cannot be released: {reason}"),
                ));
            }
            let proof = inspection.proof;
            let response = request(ClientRequest::ChatRelease {
                proof: proof.clone(),
                dry_run: false,
            })
            .map_err(protocol_error)?;
            let ServerResponse::ChatReleased {
                inspection,
                dry_run: false,
            } = response
            else {
                return Err(protocol_error(unexpected_response(
                    "native chat release",
                    &response,
                )));
            };
            if inspection.proof != proof || inspection.release_blocked.is_some() {
                return Err(AppError::new(
                    "chat_release",
                    "The daemon did not confirm release of the exact inspected handle; inspect its current state before retrying",
                ));
            }
            inspection_output(inspection, false)
        }
    }
}

fn capacity_output(capacity: ChatCapacity) -> CommandOutput {
    let suffix = if u64::from(capacity.resident) + u64::from(capacity.spawning)
        >= u64::from(capacity.limit)
    {
        "\nNew admissions are blocked. Existing work is unchanged. Inspect a completed chat with cagents chat release ID --dry-run."
    } else {
        ""
    };
    CommandOutput {
        command: "chat.capacity",
        human: format!(
            "Native chats: {} resident + {} spawning / {} limit{suffix}",
            capacity.resident, capacity.spawning, capacity.limit
        ),
        data: json!(capacity),
    }
}

fn inspection_output(
    inspection: ChatReleaseInspection,
    dry_run: bool,
) -> Result<CommandOutput, AppError> {
    let id = inspection.proof.session.id;
    let status = match (&inspection.release_blocked, dry_run) {
        (Some(reason), _) => format!("Cannot release {id}: {reason}"),
        (None, true) => format!(
            "Chat {id} is eligible now. To release its resident handle, use cagents chat release {id} --yes. Saved history is preserved; eligibility is checked again."
        ),
        (None, false) => format!("Released resident chat {id}; saved history is preserved"),
    };
    Ok(CommandOutput {
        command: "chat.release",
        human: status,
        data: json!({"dry_run":dry_run,"released":!dry_run,"inspection":inspection}),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn inspection(id: TerminalId, blocked: Option<&str>) -> ChatReleaseInspection {
        ChatReleaseInspection {
            proof: crate::terminal_ipc::ChatReleaseProof {
                session: crate::chat::Session {
                    id,
                    proof: None,
                    terminal: None,
                    owned_codex_listener: None,
                    native_continuity: Default::default(),
                    launch: crate::chat::Launch {
                        provider: crate::agent::Provider::Codex,
                        session_id: None,
                        recipe: LaunchRecipe {
                            program: "codex".into(),
                            arguments: Vec::new(),
                            working_directory: "/repo".into(),
                            environment: BTreeMap::new(),
                            unset_environment: BTreeSet::new(),
                        },
                    },
                },
                execution_instance: uuid::Uuid::new_v4(),
                generation: 7,
            },
            release_blocked: blocked.map(str::to_owned),
        }
    }
    #[test]
    fn release_requires_explicit_mode_and_dry_run_never_sends_effect() {
        assert!(
            Cli::try_parse_from(["cagents", "chat", "release", &TerminalId::new().to_string()])
                .is_err()
        );
        let id = TerminalId::new();
        for blocked in [None, Some("backend still owns work")] {
            let observed = inspection(id, blocked);
            let mut calls = 0;
            let output = execute_requests(
                ChatCommand::Release {
                    id,
                    dry_run: true,
                    yes: false,
                },
                |request| {
                    calls += 1;
                    assert_eq!(request, ClientRequest::ChatReleaseInspect { chat_id: id });
                    Ok(ServerResponse::ChatReleaseInspected {
                        inspection: observed.clone(),
                    })
                },
            )
            .unwrap();
            assert_eq!(calls, 1);
            assert_eq!(output.data["released"], false);
        }
    }
    #[test]
    fn release_uses_exact_inspected_proof_and_refuses_blocked_or_mismatched_identity() {
        let id = TerminalId::new();
        for observed in [
            inspection(id, Some("attached view")),
            inspection(TerminalId::new(), None),
        ] {
            let mut calls = 0;
            assert!(
                execute_requests(
                    ChatCommand::Release {
                        id,
                        dry_run: false,
                        yes: true
                    },
                    |request| {
                        calls += 1;
                        assert_eq!(request, ClientRequest::ChatReleaseInspect { chat_id: id });
                        Ok(ServerResponse::ChatReleaseInspected {
                            inspection: observed.clone(),
                        })
                    }
                )
                .is_err()
            );
            assert_eq!(calls, 1);
        }
        let observed = inspection(id, None);
        let mut calls = 0;
        let output = execute_requests(
            ChatCommand::Release {
                id,
                dry_run: false,
                yes: true,
            },
            |request| {
                calls += 1;
                if calls == 1 {
                    Ok(ServerResponse::ChatReleaseInspected {
                        inspection: observed.clone(),
                    })
                } else {
                    assert_eq!(
                        request,
                        ClientRequest::ChatRelease {
                            proof: observed.proof.clone(),
                            dry_run: false
                        }
                    );
                    Ok(ServerResponse::ChatReleased {
                        inspection: observed.clone(),
                        dry_run: false,
                    })
                }
            },
        )
        .unwrap();
        assert_eq!(calls, 2);
        assert_eq!(output.data["released"], true);
    }
    #[test]
    fn invalid_capacity_has_no_request_and_lower_than_usage_is_reported_truthfully() {
        assert!(
            execute_requests(
                ChatCommand::Capacity {
                    command: Some(CapacityCommand::Set { limit: 0 })
                },
                |_| panic!("invalid setting must not contact daemon")
            )
            .is_err()
        );
        let output = execute_requests(
            ChatCommand::Capacity {
                command: Some(CapacityCommand::Set { limit: 1 }),
            },
            |request| {
                assert_eq!(request, ClientRequest::ChatCapacitySet { limit: 1 });
                Ok(ServerResponse::ChatCapacity {
                    capacity: ChatCapacity {
                        resident: 128,
                        spawning: 2,
                        limit: 1,
                    },
                })
            },
        )
        .unwrap();
        assert_eq!(output.data["resident"], 128);
        assert!(output.human.contains("Existing work is unchanged"));
    }
}
