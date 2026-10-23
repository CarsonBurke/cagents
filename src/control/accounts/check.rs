//! One authenticated, selected-device observation; never a launch permit.
use super::super::*;
use crate::{
    account::{
        AccountId,
        readiness::{CheckReport, CheckRequest, CheckStatus, Measurement},
        workspace_probe::Cleanup,
    },
    device_service::{Operation, Reply},
};

#[derive(Debug, Clone, clap::Args)]
pub struct Args {
    /// Exact Codex account ID from account list on the selected device.
    #[arg(long, value_parser = parse_account)]
    pub account: AccountId,
    /// Absolute workspace on that device, or quoted '~/path'.
    #[arg(long, value_parser = parse_workspace)]
    pub workspace: PathBuf,
    /// Provider email that must match the fresh official account observation.
    #[arg(long, value_parser = parse_email)]
    pub expect_email: String,
    /// Permit one fixed scratch write/read/remove under the observed default policy.
    #[arg(long)]
    pub write: bool,
    /// Permit one read of this GitHub repository through the observed default policy.
    #[arg(long, value_parser = parse_repository)]
    pub github_repository: Option<String>,
}

impl Args {
    fn request(&self) -> Result<CheckRequest, AppError> {
        // Keep validation before socket lookup for callers that bypass clap.
        let request = CheckRequest {
            account: parse_account(self.account.as_str())
                .map_err(|error| AppError::new("account_check", error))?,
            workspace: parse_workspace(self.workspace.to_str().ok_or_else(|| {
                AppError::new("account_check", "The workspace must be a valid device path")
            })?)
            .map_err(|error| AppError::new("account_check", error))?,
            expected_email: parse_email(&self.expect_email)
                .map_err(|error| AppError::new("account_check", error))?,
            write: self.write,
            github_repository: self
                .github_repository
                .as_deref()
                .map(parse_repository)
                .transpose()
                .map_err(|error| AppError::new("account_check", error))?,
        };
        request
            .validate()
            .map_err(|error| AppError::new("account_check", error))?;
        Ok(request)
    }
}

pub(super) fn parse_account(value: &str) -> Result<AccountId, String> {
    let account: AccountId = value.parse()?;
    if !account.as_str().starts_with("codex/") {
        return Err("Account checks currently support only an explicit Codex account".into());
    }
    Ok(account)
}

pub(super) fn parse_workspace(value: &str) -> Result<PathBuf, String> {
    if value.is_empty()
        || value.chars().any(char::is_control)
        || !(Path::new(value).is_absolute()
            || value
                .strip_prefix("~/")
                .is_some_and(|relative| !Path::new(relative).is_absolute()))
    {
        return Err("Choose an absolute workspace on the selected device, or a quoted '~/path' without a doubled slash".into());
    }
    // Do not canonicalize, trim or expand using this client's filesystem/HOME.
    Ok(value.into())
}

pub(super) fn parse_email(value: &str) -> Result<String, String> {
    let email = value.trim();
    let valid_parts = email.split_once('@').is_some_and(|(local, domain)| {
        !local.is_empty() && !domain.is_empty() && !domain.contains('@')
    });
    if email.len() > 320
        || !valid_parts
        || email
            .chars()
            .any(|character| character.is_whitespace() || character.is_control())
    {
        return Err("Provide the expected provider email explicitly".into());
    }
    Ok(email.into())
}

fn parse_repository(value: &str) -> Result<String, String> {
    let valid = value.split_once('/').is_some_and(|(owner, name)| {
        !owner.is_empty()
            && owner.len() <= 100
            && !name.is_empty()
            && name.len() <= 100
            && owner.as_bytes()[0].is_ascii_alphanumeric()
            && owner
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
            && name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
            && !matches!(name, "." | "..")
    });
    if valid {
        Ok(value.into())
    } else {
        Err("Use a GitHub repository in OWNER/REPO form".into())
    }
}

pub(super) fn execute(args: Args) -> Result<CommandOutput, AppError> {
    let request = args.request()?;
    let socket = control_daemon_socket()?;
    let mut client = ControlClient::connect(&socket)
        .map_err(|error| AppError::new("daemon_connection", error))?;
    execute_with(&mut client, request)
}

fn execute_with(
    client: &mut ControlClient,
    request: CheckRequest,
) -> Result<CommandOutput, AppError> {
    client
        .authenticate(Authority::from_environment())
        .map_err(protocol_error)?;
    let Reply::AccountCheck(report) = client
        .device_operation(Operation::AccountCheck(request.clone()))
        .map_err(protocol_error)?
    else {
        return Err(invalid_report());
    };
    if !report.matches_request(&request, client.proof()) {
        return Err(invalid_report());
    }
    output(*report)
}

fn invalid_report() -> AppError {
    AppError::new(
        "account_check",
        "The account check returned an incompatible or mismatched observation; no check was retried",
    )
}

fn output(report: CheckReport) -> Result<CommandOutput, AppError> {
    let passed = report.provider_cleanup_confirmed
        && report.authentication.status == CheckStatus::Passed
        && report.execution_profile_configured.status == CheckStatus::Passed
        && matches!(
            report.workspace_write.status,
            CheckStatus::Passed | CheckStatus::NotRequested
        )
        && matches!(
            report.github.status,
            CheckStatus::Passed | CheckStatus::NotRequested
        );
    let mut human = format!(
        "Account: {}\nWorkspace: {}\nScope: account default policy; observed {}\n{}",
        report.account,
        super::super::format::plain(&report.workspace.to_string_lossy()),
        report.observed_at,
        super::super::format::table(
            ["CHECK", "STATUS", "REASON"],
            [
                ("Authentication", report.authentication),
                (
                    "Default execution policy",
                    report.execution_profile_configured
                ),
                ("Workspace write/read/remove", report.workspace_write),
                ("GitHub repository read", report.github),
            ]
            .into_iter()
            .map(|(name, measurement)| [
                name.into(),
                status(measurement).into(),
                measurement.reason.description().into()
            ])
            .collect(),
            "No observations",
        )
    );
    human.push_str(if report.provider_cleanup_confirmed {
        "\nProvider process cleanup: confirmed"
    } else {
        "\nProvider process cleanup: not confirmed"
    });
    if let Some(cleanup) = report.workspace_cleanup {
        let description = match cleanup {
            Cleanup::NotCreated => "not created",
            Cleanup::Removed => "removed",
            Cleanup::ReplacementPreserved => "replacement preserved",
            Cleanup::Unconfirmed => "not confirmed",
        };
        human.push_str(&format!("\nWorkspace cleanup: {description}"));
    }
    if let Some(push) = report.github_push_allowed {
        human.push_str(&format!(
            "\nGitHub reports push permission: {} (no push performed)",
            if push { "yes" } else { "no" }
        ));
    }
    human.push_str("\nThis observation does not authorize another actor or a future launch.");
    let mut data = serde_json::to_value(report)
        .map_err(|_| AppError::new("account_check", "The account check could not be formatted"))?;
    data["outcome"] = json!(if passed { "succeeded" } else { "failed" });
    Ok(CommandOutput {
        command: "account.check",
        data,
        human,
    })
}

fn status(measurement: Measurement) -> &'static str {
    match measurement.status {
        CheckStatus::NotRequested => "not requested",
        CheckStatus::Passed => "passed",
        CheckStatus::Failed => "failed",
        CheckStatus::Unsupported => "unsupported",
        CheckStatus::Unknown => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account::workspace_probe::{Receipt, Status};
    use crate::account::{
        Identity,
        readiness::{ApprovalPolicy, CheckReason, PolicySummary, SandboxMode},
    };
    use clap::Parser;

    fn args() -> Args {
        Args {
            account: "codex/00000000-0000-4000-8000-000000000001"
                .parse()
                .unwrap(),
            workspace: "~/workspace with space ".into(),
            expect_email: "selected@example.test".into(),
            write: true,
            github_repository: Some("owner/repo.name-1".into()),
        }
    }

    #[test]
    fn check_grammar_validates_before_daemon_and_preserves_destination_paths() {
        let cli = Cli::try_parse_from([
            "cagents",
            "--device",
            "fw16",
            "account",
            "check",
            "--account",
            "codex/system",
            "--workspace",
            "~/workspace with space ",
            "--expect-email",
            "selected@example.test",
            "--write",
            "--github-repository",
            "owner/repo.name-1",
        ])
        .unwrap();
        assert_eq!(cli.device.as_deref(), Some("fw16"));
        let TopCommand::Account {
            command: AccountCommand::Check(parsed),
        } = cli.command
        else {
            panic!("wrong account command");
        };
        assert_eq!(
            parsed.request().unwrap().workspace,
            Path::new("~/workspace with space ")
        );
        let cli = Cli::try_parse_from([
            "cagents",
            "account",
            "check",
            "--account",
            "codex/system",
            "--workspace",
            "/workspace",
            "--expect-email",
            "a@b",
        ])
        .unwrap();
        let TopCommand::Account {
            command: AccountCommand::Check(parsed),
        } = cli.command
        else {
            panic!("wrong account command");
        };
        let request = parsed.request().unwrap();
        assert!(!request.write, "scratch writes require explicit consent");
        assert!(
            request.github_repository.is_none(),
            "endpoint reads require explicit consent"
        );
        for (flag, value) in [
            ("--account", "claude/system"),
            ("--account", "omp/system"),
            ("--account", "/account/home"),
            ("--workspace", "relative"),
            ("--workspace", "~//other"),
            ("--workspace", "~another/project"),
            ("--workspace", "/project\0other"),
            ("--workspace", "/project\nother"),
            ("--expect-email", ""),
            ("--expect-email", "not-an-email"),
            ("--expect-email", "a@b@c"),
            ("--expect-email", "a@b\nother"),
            ("--expect-email", "a@b\0other"),
            ("--github-repository", "https://github.com/o/r"),
            ("--github-repository", "owner/../repo"),
            ("--github-repository", "--help/repo"),
        ] {
            let mut values = vec![
                "cagents",
                "account",
                "check",
                "--account",
                "codex/system",
                "--workspace",
                "/workspace",
                "--expect-email",
                "a@b",
            ];
            if let Some(index) = values.iter().position(|value| *value == flag) {
                values[index + 1] = value;
            } else {
                values.extend([flag, value]);
            }
            assert!(
                Cli::try_parse_from(values).is_err(),
                "accepted {flag} {value:?}"
            );
        }
        for flag in ["--home", "--command", "--env", "--sandbox", "--resume"] {
            assert!(
                Cli::try_parse_from([
                    "cagents",
                    "account",
                    "check",
                    "--account",
                    "codex/system",
                    "--workspace",
                    "/workspace",
                    "--expect-email",
                    "a@b",
                    flag,
                    "anything"
                ])
                .is_err()
            );
        }
        let mut invalid = args();
        invalid.workspace = "relative".into();
        assert_eq!(execute(invalid).err().unwrap().kind, "account_check");
    }

    fn successful_report(request: &CheckRequest, proof: HostSessionProof) -> CheckReport {
        let mut report = CheckReport::new(request);
        report.workspace = "/remote/home/workspace with space ".into();
        report.observed_at = 123;
        report.provider_cleanup_confirmed = true;
        report.host = Some(proof);
        report.authentication =
            Measurement::new(CheckStatus::Passed, CheckReason::IdentityConfirmed);
        report.execution_profile_configured =
            Measurement::new(CheckStatus::Passed, CheckReason::PolicyReported);
        report.profile = Some(PolicySummary {
            sandbox_mode: SandboxMode::WorkspaceWrite,
            approval_policy: ApprovalPolicy::Never,
            network_access: Some(true),
        });
        report.observed_identity = Some(Identity {
            email: Some("Selected@Example.Test".into()),
            organization: None,
            plan: Some("pro".into()),
        });
        report.workspace_write =
            Measurement::new(CheckStatus::Passed, CheckReason::WorkspaceRoundTrip);
        report.workspace_cleanup = Some(Cleanup::Removed);
        report.workspace_receipt = Some(Receipt {
            nonce: uuid::Uuid::new_v4(),
            created: true,
            read_match: true,
            removed: true,
            status: Status::Verified,
            cleanup: Cleanup::Removed,
        });
        report.github = Measurement::new(CheckStatus::Passed, CheckReason::GitHubRepositoryAccess);
        report.github_push_allowed = Some(false);
        report
    }

    #[test]
    fn unconfirmed_provider_cleanup_cannot_produce_success() {
        let request = args().request().unwrap();
        let proof = HostSessionProof {
            host_session_id: crate::model::HostSessionId::new("check-peer"),
            boot_id: crate::model::BootId::new("private-boot"),
            host_incarnation: crate::model::HostIncarnation::new("private-incarnation"),
        };
        let mut report = successful_report(&request, proof);
        report.provider_cleanup_confirmed = false;
        let output = output(report).unwrap();
        assert_eq!(output.data["outcome"], "failed");
        assert_eq!(output.data["authentication"]["status"], "passed");
        assert_eq!(output.data["workspace_cleanup"], "removed");
        assert!(
            output
                .human
                .contains("Provider process cleanup: not confirmed")
        );
        assert_eq!(output.exit_code(), ExitCode::from(4));
    }

    #[test]
    fn check_uses_one_authenticated_framed_request_and_keeps_failures_scoped() {
        use crate::{
            model::{BootId, HostIncarnation, HostSessionId},
            terminal_ipc::{
                ClientEnvelope, ClientRequest, PROTOCOL_VERSION, ProtocolError, ProtocolErrorCode,
                ServerEnvelope, ServerMessage, ServerResponse, read_frame, write_frame,
            },
        };
        use std::{io::Write, os::unix::net::UnixStream, thread};
        for outcome in 0..15 {
            let request = args().request().unwrap();
            let expected = request.clone();
            let (stream, mut peer) = UnixStream::pair().unwrap();
            peer.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
            peer.set_write_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let worker = thread::spawn(move || {
                let proof = HostSessionProof {
                    host_session_id: HostSessionId::new("check-peer"),
                    boot_id: BootId::new("private-boot"),
                    host_incarnation: HostIncarnation::new("private-incarnation"),
                };
                for step in 0..3 {
                    let envelope: ClientEnvelope = read_frame(&mut peer).unwrap().unwrap();
                    if step > 0 {
                        assert_eq!(envelope.proof.as_ref(), Some(&proof));
                    }
                    let response = match (step, envelope.request) {
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
                                operation: Operation::AccountCheck(actual),
                            },
                        ) => {
                            assert_eq!(actual, expected, "destination fields travel unchanged");
                            let mut report = successful_report(&actual, proof.clone());
                            match outcome {
                                1 => {
                                    report.workspace_write = Measurement::new(
                                        CheckStatus::Failed,
                                        CheckReason::SandboxDenied,
                                    );
                                    report.workspace_receipt = None;
                                    report.workspace_cleanup = Some(Cleanup::Unconfirmed);
                                }
                                6 => report.account = "codex/system".parse().unwrap(),
                                7 => report.requested_workspace = "/other-workspace".into(),
                                8 => report.expected_email = "other@example.test".into(),
                                9 => report.host = None,
                                10 => report.github_repository = Some("other/repository".into()),
                                11 => report.workspace_receipt.as_mut().unwrap().removed = false,
                                13 => {
                                    // A failed identity cannot qualify a
                                    // capability, even if another axis says pass.
                                    report.authentication = Measurement::new(
                                        CheckStatus::Failed,
                                        CheckReason::IdentityMismatch,
                                    );
                                }
                                14 => {
                                    report.workspace_write = Measurement::new(
                                        CheckStatus::Unknown,
                                        CheckReason::InvalidCommandReceipt,
                                    );
                                    report.workspace_receipt = None;
                                    report.workspace_cleanup = None;
                                }
                                _ => {}
                            }
                            if outcome == 4 {
                                ServerResponse::DeviceOperation {
                                    reply: Reply::AccountRegistry(json!({"unexpected": true})),
                                }
                            } else {
                                ServerResponse::DeviceOperation {
                                    reply: Reply::AccountCheck(Box::new(report)),
                                }
                            }
                        }
                        _ => panic!("unexpected account check sequence"),
                    };
                    if step == 2 && outcome == 5 {
                        return;
                    }
                    if step == 2 && outcome == 12 {
                        // Real length framing with invalid postcard payload, not
                        // a pre-decoded fake method response.
                        peer.write_all(&[0, 0, 0, 1, 255]).unwrap();
                        break;
                    }
                    let refused = (outcome == 2 && step == 1) || (outcome == 3 && step == 2);
                    let message = if refused {
                        ServerMessage::Error(ProtocolError::new(
                            ProtocolErrorCode::PermissionDenied,
                            "account check authority refused",
                            false,
                        ))
                    } else {
                        ServerMessage::Response { response }
                    };
                    write_frame(
                        &mut peer,
                        &ServerEnvelope {
                            version: PROTOCOL_VERSION,
                            request_id: Some(envelope.request_id),
                            proof: proof.clone(),
                            message,
                        },
                    )
                    .unwrap();
                    if refused {
                        break;
                    }
                }
                assert!(
                    read_frame::<ClientEnvelope>(&mut peer).unwrap().is_none(),
                    "check must not retry, replace the registry or dispatch another request"
                );
            });
            let mut client = ControlClient::from_stream(stream).unwrap();
            let result = execute_with(&mut client, request);
            drop(client);
            worker.join().unwrap();
            if outcome <= 1 {
                let output = result.unwrap();
                assert_eq!(output.command, "account.check");
                assert_eq!(
                    output.data["outcome"],
                    if outcome == 0 { "succeeded" } else { "failed" }
                );
                assert_eq!(
                    output.data["github_push_allowed"], false,
                    "read access does not assert push access"
                );
                assert!(
                    output.human.contains("Authentication")
                        && output.human.contains("Workspace write/read/remove")
                        && output.human.contains("GitHub repository read")
                );
                if outcome == 1 {
                    assert_eq!(output.data["authentication"]["status"], "passed");
                    assert_eq!(output.data["workspace_write"]["status"], "failed");
                    assert_eq!(output.data["workspace_cleanup"], "unconfirmed");
                    assert_eq!(output.exit_code(), ExitCode::from(4));
                }
            } else {
                assert!(
                    result.is_err(),
                    "accepted refusal/malformed/mismatched outcome {outcome}"
                );
            }
        }
    }
}
