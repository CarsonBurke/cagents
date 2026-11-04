//! Native chat management and bounded same-channel cache inspection.
use super::*;
use crate::terminal_ipc::{ChatCapacity, ChatReleaseInspection};
pub(super) mod image_assets;
mod inspection;
pub use image_assets::ImageAssetCommand;

#[derive(Debug, clap::Subcommand)]
pub enum ChatCommand {
    /// Read one bounded page of a same-channel peer's cached conversation and tool output.
    Read(inspection::ReadArgs),
    /// Inspect or explicitly remove images stored on this physical device, without a daemon.
    ImageAssets {
        #[command(subcommand)]
        command: ImageAssetCommand,
    },
    /// Show resident and spawning counts, or persist a new admission limit.
    Capacity {
        #[command(subcommand)]
        command: Option<CapacityCommand>,
    },
    /// Inspect or release one eligible resident; idle or finished alone is insufficient.
    Release {
        /// Full resident native chat ID, as shown by chat capacity.
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
    Set {
        /// Maximum distinct native resident and spawning slots (1..=4096).
        limit: u32,
    },
}

fn validate_command(command: &ChatCommand) -> Result<(), AppError> {
    match command {
        ChatCommand::ImageAssets { .. } => Err(AppError::new(
            "image_assets_local_only",
            "Image assets require the local physical-store command route",
        )),
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
            "Pass --dry-run to inspect, or --yes to authorize a guarded release of this resident handle",
        )),
        _ => Ok(()),
    }
}

pub(super) fn execute(command: ChatCommand) -> Result<CommandOutput, AppError> {
    // Peer inspection carries its issued authority in the Agent request. It
    // must not enter the terminal-management Authenticate connection below.
    let command = match command {
        ChatCommand::Read(arguments) => return inspection::execute(arguments),
        command => command,
    };
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
        ChatCommand::Read(_) => Err(AppError::new(
            "chat_read_route",
            "Chat reads require their separate authority-bearing request route",
        )),
        ChatCommand::ImageAssets { .. } => Err(AppError::new(
            "image_assets_local_only",
            "Image assets require the local physical-store command route",
        )),
        ChatCommand::Capacity { command } => {
            let request_value = match command {
                None => ClientRequest::ChatCapacityInspect,
                Some(CapacityCommand::Set { limit }) => ClientRequest::ChatCapacitySet { limit },
            };
            let response = request(request_value).map_err(protocol_error)?;
            let ServerResponse::ChatCapacity {
                capacity,
                persistence_warning,
            } = response
            else {
                return Err(protocol_error(unexpected_response(
                    "native chat capacity",
                    &response,
                )));
            };
            Ok(capacity_output(capacity, persistence_warning))
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

fn capacity_output(capacity: ChatCapacity, persistence_warning: Option<String>) -> CommandOutput {
    let suffix = if capacity.used >= capacity.limit {
        "\nNew admissions are blocked. Existing work is unchanged. Check eligibility with cagents chat release ID --dry-run. Release requires supported, verified saved history and no protected work."
    } else {
        ""
    };
    let warning = persistence_warning
        .as_ref()
        .map(|warning| format!("\nWarning: {warning}"))
        .unwrap_or_default();
    let rows = capacity
        .residents
        .iter()
        .map(|row| {
            [
                row.chat_id.to_string(),
                row.provider.label().to_owned(),
                row.status.clone(),
                row.release_blocked
                    .clone()
                    .unwrap_or_else(|| "Inspect with --dry-run to verify".to_owned()),
            ]
        })
        .collect();
    let inventory = format::table(
        ["CHAT", "PROVIDER", "STATUS", "RELEASE"],
        rows,
        "No resident chats",
    );
    let truncation = if capacity.omitted_residents > 0 {
        format!(
            "\nShowing {} resident rows; {} omitted by the bounded inventory limit",
            capacity.residents.len(),
            capacity.omitted_residents
        )
    } else {
        String::new()
    };
    let mut data = json!(capacity);
    data["persistence_warning"] = json!(persistence_warning);
    CommandOutput {
        command: "chat.capacity",
        human: format!(
            "Native chats: {} used / {} limit ({} resident, {} spawning){suffix}{warning}\n\n{inventory}{truncation}",
            capacity.used, capacity.limit, capacity.resident, capacity.spawning
        ),
        data,
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
        // The wire proof includes launch arguments and environment for exact
        // identity checks. Keep it inside this process; CLI reports expose only
        // the fields needed to identify the inspected resident.
        data: json!({
            "dry_run": dry_run,
            "released": !dry_run,
            "inspection": {
                "chat_id": id,
                "provider": inspection.proof.session.launch.provider,
                "generation": inspection.proof.generation,
                "execution_instance": inspection.proof.execution_instance,
                "release_blocked": inspection.release_blocked,
            },
        }),
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
    fn release_reports_redact_launch_credentials_while_wire_keeps_exact_proof() {
        let id = TerminalId::new();
        let mut observed = inspection(id, None);
        let recipe = &mut observed.proof.session.launch.recipe;
        recipe.arguments = vec!["--token=ARGUMENT_CREDENTIAL_SENTINEL".into()];
        recipe.environment.insert(
            "PROVIDER_TOKEN".into(),
            "ENVIRONMENT_CREDENTIAL_SENTINEL".into(),
        );
        recipe.working_directory = "/PRIVATE_WORKING_DIRECTORY_SENTINEL".into();
        observed.proof.session.launch.session_id = Some("PRIVATE_PROVIDER_THREAD_SENTINEL".into());
        for dry_run in [true, false] {
            let mut calls = 0;
            let output = execute_requests(
                ChatCommand::Release {
                    id,
                    dry_run,
                    yes: !dry_run,
                },
                |request| {
                    calls += 1;
                    if calls == 1 {
                        assert_eq!(request, ClientRequest::ChatReleaseInspect { chat_id: id });
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
            assert_eq!(calls, if dry_run { 1 } else { 2 });
            let rendered = format!("{} {}", output.human, output.data);
            for private in [
                "ARGUMENT_CREDENTIAL_SENTINEL",
                "ENVIRONMENT_CREDENTIAL_SENTINEL",
                "PRIVATE_WORKING_DIRECTORY_SENTINEL",
                "PRIVATE_PROVIDER_THREAD_SENTINEL",
            ] {
                assert!(
                    !rendered.contains(private),
                    "CLI output exposed launch identity data"
                );
            }
            assert_eq!(output.data["inspection"]["chat_id"], id.to_string());
            assert_eq!(output.data["inspection"]["generation"], 7);
            assert_eq!(
                output.data["inspection"]["execution_instance"],
                observed.proof.execution_instance.to_string()
            );
            assert!(output.data["inspection"].get("proof").is_none());
            assert!(output.data["inspection"].get("session").is_none());
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
    fn compact_inventory_exposes_completed_ids_without_claiming_eligibility() {
        let id = TerminalId::new();
        let output = capacity_output(
            ChatCapacity {
                used: 4097,
                resident: 4097,
                spawning: 0,
                limit: 128,
                residents: vec![crate::terminal_ipc::ChatCapacityResident {
                    chat_id: id,
                    provider: crate::agent::Provider::Codex,
                    status: "completed".into(),
                    release_blocked: None,
                }],
                omitted_residents: 4096,
            },
            None,
        );
        assert!(output.human.contains(&id.to_string()));
        assert!(output.human.contains("Inspect with --dry-run to verify"));
        assert!(output.human.contains("4096 omitted"));
        assert_eq!(output.data["residents"][0]["chat_id"], id.to_string());
        assert_eq!(output.data["omitted_residents"], 4096);
    }

    #[test]
    fn overlapping_recovery_slot_is_not_reported_as_full_and_warning_is_visible() {
        let output = capacity_output(
            ChatCapacity {
                used: 127,
                resident: 127,
                spawning: 1,
                limit: 128,
                residents: Vec::new(),
                omitted_residents: 0,
            },
            Some("sync failed after publication".into()),
        );
        assert!(!output.human.contains("admissions are blocked"));
        assert!(
            output
                .human
                .contains("Warning: sync failed after publication")
        );
        assert_eq!(output.data["used"], 127);
        assert_eq!(
            output.data["persistence_warning"],
            "sync failed after publication"
        );
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
                        used: 130,
                        resident: 128,
                        spawning: 2,
                        limit: 1,
                        residents: Vec::new(),
                        omitted_residents: 0,
                    },
                    persistence_warning: None,
                })
            },
        )
        .unwrap();
        assert_eq!(output.data["resident"], 128);
        assert!(output.human.contains("Existing work is unchanged"));
    }
}
