//! Explicit missing-file setup through one selected, authenticated daemon.
//! The check is context for setup, never permission for another actor to run.
use super::super::*;
use crate::{
    account::{
        AccountId,
        coding_config::{CreateRequest, InspectRequest, Outcome, Presence},
        readiness::{CheckRequest, CheckStatus},
    },
    device_service::{Operation, Reply},
};

#[derive(Debug, Clone, clap::Args)]
pub struct Args {
    /// Exact registered Codex account ID on the selected device; System is unsupported.
    #[arg(long, value_parser = parse_account)]
    pub account: AccountId,
    /// Absolute workspace on that device, or quoted '~/path', for a fresh login check.
    #[arg(long, value_parser = super::check::parse_workspace)]
    pub workspace: PathBuf,
    /// Provider email that must match the fresh official account observation.
    #[arg(long, value_parser = parse_email)]
    pub expect_email: String,
    /// Include network access in the new workspace-write/on-request defaults.
    #[arg(long)]
    pub allow_network: bool,
}

fn parse_account(value: &str) -> Result<AccountId, String> {
    let account = super::check::parse_account(value)?;
    if account.as_str() == "codex/system" {
        return Err("Choose one registered Codex account; System defaults are preserved".into());
    }
    Ok(account)
}

fn parse_email(value: &str) -> Result<String, String> {
    // Refuse controls before the common parser trims ordinary surrounding spaces.
    if value.chars().any(char::is_control) {
        return Err("Provide the expected provider email explicitly".into());
    }
    super::check::parse_email(value)
}

impl Args {
    fn check_request(&self) -> Result<CheckRequest, AppError> {
        // Repeat grammar validation before socket lookup for non-clap callers.
        let request = CheckRequest {
            account: parse_account(self.account.as_str()).map_err(invalid_input)?,
            workspace: super::check::parse_workspace(
                self.workspace
                    .to_str()
                    .ok_or_else(|| invalid_input("The workspace must be a valid device path"))?,
            )
            .map_err(invalid_input)?,
            expected_email: parse_email(&self.expect_email).map_err(invalid_input)?,
            write: false,
            github_repository: None,
        };
        request.validate().map_err(invalid_input)?;
        Ok(request)
    }
}

fn invalid_input(message: impl std::fmt::Display) -> AppError {
    AppError::new("account_coding_defaults", message)
}

fn stopped(message: &'static str) -> AppError {
    invalid_input(format!(
        "{message}. Configuration creation was not requested."
    ))
}

pub(super) fn execute(args: Args) -> Result<CommandOutput, AppError> {
    let check = args.check_request()?;
    let socket = control_daemon_socket()?;
    let mut client = ControlClient::connect(&socket)
        .map_err(|_| stopped("The selected daemon connection could not be established"))?;
    execute_with(&mut client, check, args.allow_network)
}

fn execute_with(
    client: &mut ControlClient,
    check: CheckRequest,
    network_access: bool,
) -> Result<CommandOutput, AppError> {
    // No user fallback: inherited hosted authority remains subject to the
    // selected daemon's existing user-only lifecycle admission.
    client
        .authenticate(Authority::from_environment())
        .map_err(|_| stopped("The selected daemon did not authenticate the caller"))?;
    let proof = client.proof().clone();
    let Reply::AccountCheck(report) = client
        .device_operation(Operation::AccountCheck(check.clone()))
        .map_err(|_| stopped("The fresh account check could not be confirmed"))?
    else {
        return Err(stopped(
            "The fresh account check returned an incompatible reply",
        ));
    };
    if !report.matches_request(&check, &proof)
        || report.authentication.status != CheckStatus::Passed
        || !report.provider_cleanup_confirmed
    {
        return Err(stopped(
            "The selected login, account context or provider cleanup could not be confirmed",
        ));
    }
    let home = report
        .account_home_identity
        .ok_or_else(|| stopped("The selected account's physical home could not be confirmed"))?;
    let inspect = InspectRequest {
        account: check.account.clone(),
        expected_home: home,
        expected_email: check.expected_email,
    };
    let Reply::AccountCodingConfigInspect(observation) = client
        .device_operation(Operation::AccountCodingConfigInspect(inspect.clone()))
        .map_err(|_| stopped("Configuration absence could not be inspected"))?
    else {
        return Err(stopped(
            "Configuration inspection returned an incompatible reply",
        ));
    };
    if observation.host != proof || observation.request() != inspect {
        return Err(stopped(
            "The configuration inspection did not match the selected account context",
        ));
    }
    match observation.presence {
        Presence::Absent => {}
        Presence::Present => {
            return Err(stopped(
                "A configuration entry already exists and is preserved",
            ));
        }
        Presence::Unknown => return Err(stopped("Configuration absence could not be confirmed")),
    }
    let request = CreateRequest {
        observation: *observation,
        network_access,
    };
    // From here even a rejected/malformed/lost response cannot establish that
    // publication had no effect. Never reconnect, resend or roll back.
    let Ok(Reply::AccountCodingConfigCreate(receipt)) =
        client.device_operation(Operation::AccountCodingConfigCreate(request))
    else {
        return Ok(output(&inspect.account, network_access, None));
    };
    if receipt.account != inspect.account
        || receipt.home != home
        || receipt.host != proof
        || receipt.network_access != network_access
    {
        return Ok(output(&inspect.account, network_access, None));
    }
    Ok(output(
        &inspect.account,
        network_access,
        Some(receipt.outcome),
    ))
}

fn output(account: &AccountId, network_access: bool, result: Option<Outcome>) -> CommandOutput {
    let (creation, reason, detail) = match result {
        Some(Outcome::Created) => (
            "created",
            "created",
            "New workspace-write defaults with on-request approvals were created for new Codex processes.",
        ),
        Some(Outcome::NotCreated(reason)) => ("not_created", "refused", reason.description()),
        Some(Outcome::CreationUncertain(_)) | None => (
            "uncertain",
            "creation_uncertain",
            "Creation could not be confirmed. Inspect this account's configuration on the same device before another request; nothing was retried or rolled back.",
        ),
    };
    CommandOutput {
        command: "account.create-coding-defaults",
        data: json!({
            "account": account,
            "network_access": network_access,
            "outcome": if creation == "created" { "succeeded" } else { "failed" },
            "creation": creation,
            "reason": reason,
            "result": result,
            "execution_readiness": "not_measured",
        }),
        human: format!(
            "{detail}\nRequested network in new defaults: {}\nActual write and network access have not been measured. Existing conversations and other actors keep their own permissions.",
            if network_access {
                "enabled"
            } else {
                "disabled"
            },
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        account::{
            Identity,
            coding_config::{CreateReport, Observation, Refusal, Uncertainty},
            readiness::{CheckReason, CheckReport, Measurement, PhysicalIdentity},
        },
        model::{BootId, HostIncarnation, HostSessionId},
        terminal_ipc::{
            ClientEnvelope, ClientRequest, PROTOCOL_VERSION, ProtocolError, ProtocolErrorCode,
            ServerEnvelope, ServerMessage, ServerResponse, read_frame, write_frame,
        },
    };
    use clap::Parser;
    use std::{net::Shutdown, os::unix::net::UnixStream, thread};

    const ACCOUNT: &str = "codex/00000000-0000-4000-8000-000000000001";

    fn args() -> Args {
        Args {
            account: ACCOUNT.parse().unwrap(),
            workspace: "~/workspace with space ".into(),
            expect_email: "selected@example.test".into(),
            allow_network: false,
        }
    }

    #[test]
    fn coding_defaults_grammar_refuses_before_routing_and_preserves_device_paths() {
        let values = [
            "cagents",
            "--device",
            "destination",
            "account",
            "create-coding-defaults",
            "--account",
            ACCOUNT,
            "--workspace",
            "~/workspace with space ",
            "--expect-email",
            "selected@example.test",
        ];
        let cli = Cli::try_parse_from(values).unwrap();
        assert_eq!(cli.device.as_deref(), Some("destination"));
        let TopCommand::Account {
            command: AccountCommand::CreateCodingDefaults(parsed),
        } = cli.command
        else {
            panic!("wrong command")
        };
        assert!(!parsed.allow_network);
        assert_eq!(
            parsed.check_request().unwrap().workspace,
            Path::new(values[8])
        );
        let mut online = values.to_vec();
        online.push("--allow-network");
        let cli = Cli::try_parse_from(online).unwrap();
        assert!(matches!(
            cli.command,
            TopCommand::Account {
                command: AccountCommand::CreateCodingDefaults(Args {
                    allow_network: true,
                    ..
                })
            }
        ));
        for (index, invalid) in [
            (6, "codex/system"),
            (6, "claude/system"),
            (6, "omp/system"),
            (6, "codex/not-a-uuid"),
            (6, "/account"),
            (8, ""),
            (8, "relative"),
            (8, "~//other"),
            (8, "~another/path"),
            (8, "/path\0other"),
            (8, "/path\nother"),
            (10, ""),
            (10, "not-email"),
            (10, "a@b@c"),
            (10, "\na@b"),
            (10, "a@b\n"),
            (10, "a b@c"),
        ] {
            let mut invalid_values = values;
            invalid_values[index] = invalid;
            assert!(
                Cli::try_parse_from(invalid_values).is_err(),
                "accepted {invalid:?}"
            );
        }
        for flag in [
            "--home",
            "--command",
            "--write",
            "--github-repository",
            "--sandbox",
            "--config",
        ] {
            let mut invalid_values = values.to_vec();
            invalid_values.extend([flag, "arbitrary"]);
            assert!(
                Cli::try_parse_from(invalid_values).is_err(),
                "accepted {flag}"
            );
        }
        // Programmatic callers also refuse before looking up a daemon socket.
        for (account, workspace, email) in [
            ("codex/system", "/workspace", "a@b"),
            (ACCOUNT, "relative", "a@b"),
            (ACCOUNT, "/workspace", "a@b\n"),
        ] {
            let invalid = Args {
                account: account.parse().unwrap(),
                workspace: workspace.into(),
                expect_email: email.into(),
                allow_network: false,
            };
            assert_eq!(
                execute(invalid).err().unwrap().kind,
                "account_coding_defaults"
            );
        }
    }

    #[derive(Debug, Clone, Copy)]
    enum Case {
        Created,
        NotCreated,
        PublishedUncertain,
        AuthRefused,
        CheckRefused,
        CheckWrongReply,
        WrongIdentity,
        AuthFailed,
        CleanupUnconfirmed,
        HomeMissing,
        CheckWrongProof,
        InspectWrongReply,
        InspectLost,
        InspectWrongAccount,
        InspectWrongHome,
        InspectWrongEmail,
        InspectWrongProof,
        Existing,
        UnknownPresence,
        CreateLost,
        CreateRefused,
        CreateWrongReply,
        CreateWrongAccount,
        CreateWrongHome,
        CreateWrongProof,
        CreateWrongNetwork,
        CreateEnvelopeWrongProof,
    }

    fn proof() -> HostSessionProof {
        HostSessionProof {
            host_session_id: HostSessionId::new("coding-defaults-peer"),
            boot_id: BootId::new("private-boot"),
            host_incarnation: HostIncarnation::new("private-incarnation"),
        }
    }

    fn send(
        peer: &mut UnixStream,
        envelope: &ClientEnvelope,
        proof: HostSessionProof,
        message: ServerMessage,
    ) {
        write_frame(
            peer,
            &ServerEnvelope {
                version: PROTOCOL_VERSION,
                request_id: Some(envelope.request_id),
                proof,
                message,
            },
        )
        .unwrap();
    }

    fn framed(case: Case, network_access: bool) -> Result<CommandOutput, AppError> {
        let request = args().check_request().unwrap();
        let expected = request.clone();
        let (stream, mut peer) = UnixStream::pair().unwrap();
        peer.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
        peer.set_write_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let worker = thread::spawn(move || {
            let host = proof();
            let home = PhysicalIdentity {
                device: 4,
                inode: 9,
            };
            let observation = Observation {
                account: expected.account.clone(),
                host: host.clone(),
                home,
                presence: Presence::Absent,
                expected_email: expected.expected_email.clone(),
            };
            for step in 0..5 {
                let envelope: ClientEnvelope = read_frame(&mut peer).unwrap().unwrap();
                if step > 0 {
                    assert_eq!(envelope.proof.as_ref(), Some(&host));
                }
                let response = match (step, &envelope.request) {
                    (0, ClientRequest::Connect { .. }) => ServerResponse::Connected {
                        proof: host.clone(),
                        resumed: vec![],
                    },
                    (1, ClientRequest::Authenticate { authority }) => {
                        assert_eq!(
                            *authority,
                            Authority::from_environment(),
                            "never replace hosted authority with User"
                        );
                        ServerResponse::Accepted
                    }
                    (
                        2,
                        ClientRequest::DeviceOperation {
                            operation: Operation::AccountCheck(actual),
                        },
                    ) => {
                        assert_eq!(actual, &expected);
                        assert!(!actual.write);
                        assert!(actual.github_repository.is_none());
                        let mut report = CheckReport::new(actual);
                        report.workspace = "/destination/workspace with space ".into();
                        report.host = Some(host.clone());
                        report.authentication =
                            Measurement::new(CheckStatus::Passed, CheckReason::IdentityConfirmed);
                        // An implicit/unknown account policy must not prevent
                        // explicitly setting up missing defaults.
                        report.execution_profile_configured =
                            Measurement::unknown(CheckReason::ImplicitPolicy);
                        report.provider_cleanup_confirmed = true;
                        report.account_home_identity = Some(home);
                        report.observed_identity = Some(Identity {
                            email: Some("Selected@Example.Test".into()),
                            organization: None,
                            plan: None,
                        });
                        match case {
                            Case::WrongIdentity => {
                                report.observed_identity.as_mut().unwrap().email =
                                    Some("other@example.test".into())
                            }
                            Case::AuthFailed => {
                                report.authentication = Measurement::unknown(CheckReason::SignedOut)
                            }
                            Case::CleanupUnconfirmed => report.provider_cleanup_confirmed = false,
                            Case::HomeMissing => report.account_home_identity = None,
                            Case::CheckWrongProof => {
                                report.host.as_mut().unwrap().host_incarnation =
                                    HostIncarnation::new("different-host")
                            }
                            _ => {}
                        }
                        ServerResponse::DeviceOperation {
                            reply: if matches!(case, Case::CheckWrongReply) {
                                Reply::AccountRegistry(json!({"raw": "not public"}))
                            } else {
                                Reply::AccountCheck(Box::new(report))
                            },
                        }
                    }
                    (
                        3,
                        ClientRequest::DeviceOperation {
                            operation: Operation::AccountCodingConfigInspect(actual),
                        },
                    ) => {
                        assert_eq!(actual, &observation.request());
                        let mut observed = observation.clone();
                        match case {
                            Case::InspectWrongAccount => {
                                observed.account = "codex/00000000-0000-4000-8000-000000000002"
                                    .parse()
                                    .unwrap()
                            }
                            Case::InspectWrongHome => observed.home.inode += 1,
                            Case::InspectWrongEmail => {
                                observed.expected_email = "other@example.test".into()
                            }
                            Case::InspectWrongProof => {
                                observed.host.host_incarnation =
                                    HostIncarnation::new("different-host")
                            }
                            Case::Existing => observed.presence = Presence::Present,
                            Case::UnknownPresence => observed.presence = Presence::Unknown,
                            _ => {}
                        }
                        ServerResponse::DeviceOperation {
                            reply: if matches!(case, Case::InspectWrongReply) {
                                Reply::AccountRegistry(json!({"raw": "not public"}))
                            } else {
                                Reply::AccountCodingConfigInspect(Box::new(observed))
                            },
                        }
                    }
                    (
                        4,
                        ClientRequest::DeviceOperation {
                            operation: Operation::AccountCodingConfigCreate(actual),
                        },
                    ) => {
                        assert_eq!(
                            actual.observation, observation,
                            "use original inspected home and identity"
                        );
                        assert_eq!(
                            actual.network_access, network_access,
                            "network requires explicit consent"
                        );
                        let mut receipt = CreateReport {
                            account: expected.account.clone(),
                            host: host.clone(),
                            home,
                            network_access,
                            outcome: match case {
                                Case::NotCreated => Outcome::NotCreated(Refusal::SharedConfig),
                                Case::PublishedUncertain => {
                                    Outcome::CreationUncertain(Uncertainty::DirectorySyncFailed)
                                }
                                _ => Outcome::Created,
                            },
                        };
                        match case {
                            Case::CreateWrongAccount => {
                                receipt.account = "codex/system".parse().unwrap()
                            }
                            Case::CreateWrongHome => receipt.home.inode += 1,
                            Case::CreateWrongProof => {
                                receipt.host.host_incarnation =
                                    HostIncarnation::new("different-host")
                            }
                            Case::CreateWrongNetwork => receipt.network_access = !network_access,
                            _ => {}
                        }
                        ServerResponse::DeviceOperation {
                            reply: if matches!(case, Case::CreateWrongReply) {
                                Reply::AccountRegistry(json!({"raw": "not public"}))
                            } else {
                                Reply::AccountCodingConfigCreate(Box::new(receipt))
                            },
                        }
                    }
                    _ => panic!("unexpected {case:?} step {step}: request kind changed"),
                };
                let lost = matches!((case, step), (Case::InspectLost, 3) | (Case::CreateLost, 4));
                if lost {
                    peer.shutdown(Shutdown::Write).unwrap();
                    break;
                }
                let refused = matches!(
                    (case, step),
                    (Case::AuthRefused, 1) | (Case::CheckRefused, 2) | (Case::CreateRefused, 4)
                );
                let message = if refused {
                    ServerMessage::Error(ProtocolError::new(
                        ProtocolErrorCode::PermissionDenied,
                        "PRIVATE provider diagnostic must not be printed",
                        false,
                    ))
                } else {
                    ServerMessage::Response { response }
                };
                let mut envelope_host = host.clone();
                if step == 4 && matches!(case, Case::CreateEnvelopeWrongProof) {
                    envelope_host.host_incarnation = HostIncarnation::new("different-host");
                }
                send(&mut peer, &envelope, envelope_host, message);
                let stop = refused
                    || matches!(
                        (case, step),
                        (
                            Case::CheckWrongReply
                                | Case::WrongIdentity
                                | Case::AuthFailed
                                | Case::CleanupUnconfirmed
                                | Case::HomeMissing
                                | Case::CheckWrongProof,
                            2
                        ) | (
                            Case::InspectWrongReply
                                | Case::InspectWrongAccount
                                | Case::InspectWrongHome
                                | Case::InspectWrongEmail
                                | Case::InspectWrongProof
                                | Case::Existing
                                | Case::UnknownPresence,
                            3
                        )
                    );
                if stop {
                    break;
                }
            }
            assert!(
                read_frame::<ClientEnvelope>(&mut peer).unwrap().is_none(),
                "{case:?}: no retry, reconnect, automatic recheck or extra request on this connection"
            );
        });
        let mut client = ControlClient::from_stream(stream).unwrap();
        let result = execute_with(&mut client, request, network_access);
        drop(client);
        worker.join().unwrap();
        result
    }

    #[test]
    fn coding_defaults_framed_prerequisites_stop_before_create() {
        for case in [
            Case::AuthRefused,
            Case::CheckRefused,
            Case::CheckWrongReply,
            Case::WrongIdentity,
            Case::AuthFailed,
            Case::CleanupUnconfirmed,
            Case::HomeMissing,
            Case::CheckWrongProof,
            Case::InspectWrongReply,
            Case::InspectLost,
            Case::InspectWrongAccount,
            Case::InspectWrongHome,
            Case::InspectWrongEmail,
            Case::InspectWrongProof,
            Case::Existing,
            Case::UnknownPresence,
        ] {
            let error = framed(case, false).err().expect("creation must stop");
            assert_eq!(error.kind, "account_coding_defaults");
            assert!(
                error.message.contains("creation was not requested"),
                "{case:?}"
            );
            assert!(!error.message.contains("PRIVATE"));
        }
    }

    #[test]
    fn coding_defaults_framed_explicit_network_and_fixed_public_outcomes() {
        for network in [false, true] {
            let output = framed(Case::Created, network).unwrap();
            assert_eq!(output.command, "account.create-coding-defaults");
            assert_eq!(output.data["creation"], "created");
            assert_eq!(output.data["network_access"], network);
            assert_eq!(output.data["execution_readiness"], "not_measured");
            assert_eq!(output.exit_code(), ExitCode::SUCCESS);
            assert!(
                output
                    .human
                    .contains("Existing conversations and other actors keep their own permissions")
            );
        }
        let refused = framed(Case::NotCreated, false).unwrap();
        assert_eq!(refused.data["creation"], "not_created");
        assert_eq!(
            refused.data["result"],
            json!({"not_created":"shared_config"})
        );
        assert!(refused.human.contains(Refusal::SharedConfig.description()));
        assert_eq!(refused.exit_code(), ExitCode::from(4));
        let uncertain = framed(Case::PublishedUncertain, false).unwrap();
        assert_eq!(uncertain.data["creation"], "uncertain");
        assert_eq!(
            uncertain.data["result"],
            json!({"creation_uncertain":"directory_sync_failed"})
        );
        assert_eq!(uncertain.exit_code(), ExitCode::from(4));
    }

    #[test]
    fn coding_defaults_framed_lost_or_mismatched_creation_is_uncertain_without_resend() {
        for case in [
            Case::CreateLost,
            Case::CreateRefused,
            Case::CreateWrongReply,
            Case::CreateWrongAccount,
            Case::CreateWrongHome,
            Case::CreateWrongProof,
            Case::CreateWrongNetwork,
            Case::CreateEnvelopeWrongProof,
        ] {
            let output = framed(case, true).unwrap();
            assert_eq!(output.data["creation"], "uncertain", "{case:?}");
            assert!(output.data["result"].is_null());
            assert_eq!(output.exit_code(), ExitCode::from(4));
            assert!(
                output
                    .human
                    .contains("Inspect this account's configuration on the same device")
            );
            assert!(!output.human.contains("PRIVATE"));
        }
    }
}
