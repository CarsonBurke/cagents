use super::{super::*, AccountCommand};
use crate::account::reset::{
    self, Decision, ProviderAccount,
    request::{AUTOMATIC_ADMISSION_UNAVAILABLE, Action, Report, Request},
};

pub(super) fn execute(command: AccountCommand) -> Result<CommandOutput, AppError> {
    if matches!(
        &command,
        AccountCommand::Monitor {
            authorize: true,
            ..
        }
    ) {
        return Err(automatic_monitor_unavailable());
    }
    let socket = control_daemon_socket()?;
    let mut client =
        ControlClient::connect(&socket).map_err(|e| AppError::new("daemon_connection", e))?;
    execute_with_client(&mut client, command, Authority::from_environment())
}

fn execute_with_client(
    client: &mut ControlClient,
    command: AccountCommand,
    authority: Authority,
) -> Result<CommandOutput, AppError> {
    // Preserve the caller role on this exact connection. A failed session
    // authentication must never fall back to user authority or another client.
    client.authenticate(authority).map_err(protocol_error)?;
    let mut invoke = |request: Request| {
        request
            .validate()
            .map_err(|e| AppError::new("account_reset_request", e))?;
        let session = request.session;
        let expected_redemption = match &request.action {
            Action::Inspect => None,
            Action::Redeem { expected, .. } => Some(expected.clone()),
        };
        let reply = client
            .device_operation(crate::device_service::Operation::AccountAllowance { request })
            .map_err(protocol_error)?;
        let crate::device_service::Reply::AccountAllowance(report) = reply else {
            return Err(AppError::new(
                "account_allowance",
                "Unexpected allowance response; no alternative target or local fallback was selected",
            ));
        };
        report
            .validate()
            .map_err(|e| AppError::new("account_allowance", e))?;
        if report.session != session {
            return Err(AppError::new(
                "account_allowance",
                "Allowance reply names a different session",
            ));
        }
        match expected_redemption {
            None if report.outcome.is_some() || report.reconciled => {
                return Err(AppError::new(
                    "account_allowance",
                    "Allowance inspection reply unexpectedly describes redemption",
                ));
            }
            Some(expected) if report.allowance.account.as_ref() != Some(&expected) => {
                return Err(AppError::new(
                    "account_allowance",
                    "Reset reply does not match the requested provider account and backend origin; redemption outcome is unconfirmed",
                ));
            }
            Some(_) if report.outcome.is_none() => {
                return Err(AppError::new(
                    "account_allowance",
                    "Reset reply did not include a reset outcome; redemption outcome is unconfirmed",
                ));
            }
            _ => {}
        }
        Ok(report)
    };
    match command {
        AccountCommand::Allowance { session } => {
            let report = invoke(Request {
                session,
                action: Action::Inspect,
            })?;
            output("account.allowance", report)
        }
        AccountCommand::Redeem {
            session,
            provider_account,
            backend_origin,
            credit,
            authorize,
        } => {
            let report = invoke(Request {
                session,
                action: Action::Redeem {
                    expected: ProviderAccount {
                        account_id: provider_account,
                        backend_origin,
                    },
                    credit_id: credit,
                    authorized: authorize,
                    condition: None,
                },
            })?;
            output("account.redeem", report)
        }
        AccountCommand::Monitor {
            session,
            threshold,
            interval_seconds,
            checks,
            max_redemptions,
            authorize,
        } => monitor(
            session,
            threshold,
            interval_seconds,
            checks,
            max_redemptions,
            authorize,
            &mut invoke,
            thread::sleep,
        ),
        AccountCommand::Info { .. } => Err(AppError::new(
            "account_command",
            "Account info uses its existing read route",
        )),
    }
}

fn output(command: &'static str, report: Report) -> Result<CommandOutput, AppError> {
    Ok(CommandOutput {
        command,
        human: human(&report),
        data: serde_json::to_value(report).map_err(|e| AppError::new("account_allowance", e))?,
    })
}

fn human(report: &Report) -> String {
    let account = report
        .allowance
        .account
        .as_ref()
        .map(|a| a.account_id.as_str())
        .unwrap_or("unknown; redemption unavailable");
    let mut lines = vec![
        format!("Session {}  provider account {account}", report.session),
        format!(
            "Source {:?}; fetched at {} ms",
            report.source, report.allowance.observed_at_ms
        ),
    ];
    for window in &report.allowance.windows {
        lines.push(format!(
            "{} {:?}: {}% used; scheduled reset {}",
            window.bucket,
            window.period,
            window.used_percent,
            window
                .resets_at_ms
                .map(|at| at.to_string())
                .unwrap_or_else(|| "unknown".into())
        ));
    }
    lines.push(match &report.allowance.reset_credits {
        Some(credits) => format!(
            "Available earned reset credits: {}; {} detail rows",
            credits.available_count,
            credits
                .details
                .as_ref()
                .map(|v| v.len().to_string())
                .unwrap_or_else(|| "unknown".into())
        ),
        None => "Earned reset-credit inventory: unavailable from this provider reply".into(),
    });
    if let Some(outcome) = report.outcome {
        lines.push(format!(
            "Reset result: {outcome:?}{}",
            if report.reconciled {
                " (original attempt reconciled; no new key)"
            } else {
                ""
            }
        ));
    }
    if let Some(detail) = &report.allowance.account_binding_detail {
        lines.push(detail.clone());
    }
    lines.join("\n")
}

fn automatic_monitor_unavailable() -> AppError {
    AppError::new(
        "account_monitor_unsupported",
        AUTOMATIC_ADMISSION_UNAVAILABLE,
    )
}

fn monitor(
    session: TerminalId,
    threshold: u8,
    interval: u64,
    checks: u32,
    maximum: u16,
    authorized: bool,
    mut invoke: impl FnMut(Request) -> Result<Report, AppError>,
    mut sleep: impl FnMut(Duration),
) -> Result<CommandOutput, AppError> {
    if authorized {
        return Err(automatic_monitor_unavailable());
    }
    if checks == 0
        || checks > 1_000_000
        || maximum == 0
        || maximum > 100
        || interval == 0
        || interval > 3600
        || threshold == 0
        || threshold > 100
    {
        return Err(AppError::new(
            "account_monitor",
            "Use positive bounded checks, interval and requested budget",
        ));
    }
    let mut completed = 0;
    let mut last = None;
    let mut stopped = "check_limit";
    for index in 0..checks {
        let report = invoke(Request {
            session,
            action: Action::Inspect,
        })?;
        let now = crate::chat::now_ms().ok_or_else(|| {
            AppError::new(
                "account_monitor_clock",
                "Cannot timestamp allowance freshness",
            )
        })?;
        let decision = reset::decision(&report.allowance, now, 30_000, threshold, false)
            .map_err(|e| AppError::new("account_monitor", e))?;
        if matches!(decision, Decision::AuthorizationRequired) {
            eprintln!(
                "Allowance check {}: threshold reached; automatic redemption is currently unavailable. Inspect and explicitly redeem a named credit.",
                index + 1
            );
        } else {
            eprintln!("Allowance check {}: {decision:?}", index + 1);
        }
        match decision {
            Decision::NoCredits => stopped = "inventory_exhausted",
            Decision::Stale
            | Decision::UnknownUsage
            | Decision::UnknownAccount
            | Decision::InventoryUnknown
            | Decision::CreditDetailsUnavailable => {
                return Err(AppError::new(
                    "account_monitor_unavailable",
                    format!(
                        "Allowance/reset state is {decision:?}; monitor stopped without guessing or resuming work"
                    ),
                ));
            }
            Decision::Eligible { .. } => return Err(automatic_monitor_unavailable()),
            Decision::BelowThreshold | Decision::AuthorizationRequired => {}
        }
        completed += 1;
        last = Some(report);
        if stopped != "check_limit" {
            break;
        }
        if index + 1 < checks {
            sleep(Duration::from_secs(interval));
        }
    }
    let last =
        last.ok_or_else(|| AppError::new("account_monitor", "Monitor obtained no reading"))?;
    Ok(CommandOutput {
        command: "account.monitor",
        human: format!(
            "{}\nRead-only monitor stopped: {stopped}; {completed} checks, 0 reset requests",
            human(&last)
        ),
        data: serde_json::json!({"last":last,"checks_completed":completed,"successful_reset_results":0,"authorized":false,"automatic_redemption_supported":false,"stopped_reason":stopped}),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account::reset::{
        Allowance, Period, ResetCredit, ResetCredits, Window, request::Source,
    };
    fn reading(session: TerminalId) -> Report {
        let now = crate::chat::now_ms().unwrap();
        Report {
            session,
            provider: crate::agent::Provider::Codex,
            source: Source::CodexAppServer,
            allowance: Allowance {
                observed_at_ms: now,
                account: Some(ProviderAccount {
                    account_id: "actual-account".into(),
                    backend_origin: "https://chatgpt.com".into(),
                }),
                account_binding_detail: None,
                windows: vec![Window {
                    bucket: "codex".into(),
                    period: Period::Primary,
                    used_percent: 96,
                    resets_at_ms: Some(now + 60_000),
                }],
                reset_credits: Some(ResetCredits {
                    available_count: 1,
                    details: Some(vec![ResetCredit {
                        id: "actual-credit".into(),
                        supported_type: true,
                        available: true,
                        expires_at_ms: None,
                    }]),
                }),
            },
            outcome: None,
            reconciled: false,
        }
    }
    #[test]
    fn allowance_commands_authenticate_the_same_client_and_preserve_agent_refusals() {
        use crate::{
            device_service::{Operation, Reply},
            model::{BootId, HostIncarnation, HostSessionId, HostSessionProof},
            terminal_ipc::{
                ClientEnvelope, ClientRequest, PROTOCOL_VERSION, ProtocolError, ProtocolErrorCode,
                ServerEnvelope, ServerMessage, ServerResponse, read_frame, write_frame,
            },
        };
        use std::os::unix::net::UnixStream;

        // These are synthetic fixture roles, never a process environment token.
        // User succeeds; valid agent is refused at DeviceOperation; invalid
        // agent is refused at Authenticate. Neither failure can retry as user.
        for role in 0..3 {
            for command_kind in 0..3 {
                let session = TerminalId::new();
                let (stream, mut peer) = UnixStream::pair().unwrap();
                peer.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
                peer.set_write_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                let worker = thread::spawn(move || {
                    let proof = HostSessionProof {
                        host_session_id: HostSessionId::new("private-reset-role-peer"),
                        boot_id: BootId::new("private-boot"),
                        host_incarnation: HostIncarnation::new("private-incarnation"),
                    };
                    let steps = if role == 2 { 2 } else { 3 };
                    for step in 0..steps {
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
                                let expected = if role == 0 {
                                    Authority::default()
                                } else {
                                    Authority::session("synthetic-reset-fixture-role".into())
                                };
                                assert_eq!(authority, expected);
                                ServerResponse::Accepted
                            }
                            (
                                2,
                                ClientRequest::DeviceOperation {
                                    operation: Operation::AccountAllowance { request },
                                },
                            ) => {
                                assert_eq!(request.session, session);
                                request.validate().unwrap();
                                if command_kind == 1 {
                                    assert!(matches!(
                                        request.action,
                                        Action::Redeem {
                                            authorized: true,
                                            condition: None,
                                            ..
                                        }
                                    ));
                                } else {
                                    assert_eq!(request.action, Action::Inspect);
                                }
                                let mut report = reading(session);
                                if command_kind == 1 {
                                    report.outcome = Some(reset::Outcome::Reset);
                                }
                                ServerResponse::DeviceOperation {
                                    reply: Reply::AccountAllowance(report),
                                }
                            }
                            _ => panic!("unexpected request, role fallback or replacement target"),
                        };
                        let refused = (role == 2 && step == 1) || (role == 1 && step == 2);
                        let message = if refused {
                            ServerMessage::Error(ProtocolError::new(
                                ProtocolErrorCode::PermissionDenied,
                                "Synthetic caller role refused; no allowance effect admitted",
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
                    }
                    assert!(
                        read_frame::<ClientEnvelope>(&mut peer).unwrap().is_none(),
                        "caller refusal must not retry or reconnect as the user"
                    );
                });
                let command = match command_kind {
                    0 => AccountCommand::Allowance { session },
                    1 => AccountCommand::Redeem {
                        session,
                        provider_account: "actual-account".into(),
                        backend_origin: "https://chatgpt.com".into(),
                        credit: "actual-credit".into(),
                        authorize: true,
                    },
                    _ => AccountCommand::Monitor {
                        session,
                        threshold: 90,
                        interval_seconds: 1,
                        checks: 1,
                        max_redemptions: 1,
                        authorize: false,
                    },
                };
                let authority = if role == 0 {
                    Authority::default()
                } else {
                    Authority::session("synthetic-reset-fixture-role".into())
                };
                let mut client = ControlClient::from_stream(stream);
                let result = match &mut client {
                    Ok(client) => execute_with_client(client, command, authority),
                    Err(_) => Err(AppError::new("fixture", "private handshake failed")),
                };
                drop(client);
                let joined = worker.join();
                joined.unwrap();
                if role == 0 {
                    assert!(result.is_ok());
                } else {
                    assert!(
                        result
                            .err()
                            .unwrap()
                            .message
                            .contains("Synthetic caller role refused")
                    );
                }
            }
        }
    }

    fn framed_allowance_report(
        session: TerminalId,
        redeem: bool,
        report: Report,
    ) -> Result<CommandOutput, AppError> {
        use crate::{
            device_service::{Operation, Reply},
            model::{BootId, HostIncarnation, HostSessionId, HostSessionProof},
            terminal_ipc::{
                ClientEnvelope, ClientRequest, PROTOCOL_VERSION, ServerEnvelope, ServerMessage,
                ServerResponse, read_frame, write_frame,
            },
        };
        use std::os::unix::net::UnixStream;

        let (stream, mut peer) = UnixStream::pair().unwrap();
        peer.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
        peer.set_write_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let worker = thread::spawn(move || {
            let proof = HostSessionProof {
                host_session_id: HostSessionId::new("private-reset-report-peer"),
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
                        assert_eq!(authority, Authority::default());
                        ServerResponse::Accepted
                    }
                    (
                        2,
                        ClientRequest::DeviceOperation {
                            operation: Operation::AccountAllowance { request },
                        },
                    ) => {
                        assert_eq!(request.session, session);
                        assert_eq!(
                            request.action,
                            if redeem {
                                Action::Redeem {
                                    expected: ProviderAccount {
                                        account_id: "actual-account".into(),
                                        backend_origin: "https://chatgpt.com".into(),
                                    },
                                    credit_id: "actual-credit".into(),
                                    authorized: true,
                                    condition: None,
                                }
                            } else {
                                Action::Inspect
                            }
                        );
                        ServerResponse::DeviceOperation {
                            reply: Reply::AccountAllowance(report.clone()),
                        }
                    }
                    _ => panic!("unexpected report fixture request"),
                };
                write_frame(
                    &mut peer,
                    &ServerEnvelope {
                        version: PROTOCOL_VERSION,
                        request_id: Some(envelope.request_id),
                        proof: proof.clone(),
                        message: ServerMessage::Response { response },
                    },
                )
                .unwrap();
            }
            assert!(
                read_frame::<ClientEnvelope>(&mut peer).unwrap().is_none(),
                "an unconfirmed report must not trigger another operation"
            );
        });
        let command = if redeem {
            AccountCommand::Redeem {
                session,
                provider_account: "actual-account".into(),
                backend_origin: "https://chatgpt.com".into(),
                credit: "actual-credit".into(),
                authorize: true,
            }
        } else {
            AccountCommand::Allowance { session }
        };
        let mut client = ControlClient::from_stream(stream).unwrap();
        let result = execute_with_client(&mut client, command, Authority::default());
        drop(client);
        worker.join().unwrap();
        result
    }

    #[test]
    fn reset_reports_must_match_the_requested_action_and_provider_account() {
        let session = TerminalId::new();
        for (case, message) in [
            (0, "did not include a reset outcome"),
            (1, "does not match the requested provider account"),
            (2, "does not match the requested provider account"),
            (3, "does not match the requested provider account"),
            (4, "names a different session"),
        ] {
            let mut report = reading(session);
            report.outcome = Some(reset::Outcome::Reset);
            match case {
                0 => report.outcome = None,
                1 => report.allowance.account = None,
                2 => report.allowance.account.as_mut().unwrap().account_id = "other-account".into(),
                3 => {
                    report.allowance.account.as_mut().unwrap().backend_origin =
                        "https://other.example".into()
                }
                _ => report.session = TerminalId::new(),
            }
            let error = framed_allowance_report(session, true, report)
                .err()
                .unwrap();
            assert_eq!(error.kind, "account_allowance");
            assert!(error.message.contains(message), "{}", error.message);
        }
        for outcome in [
            reset::Outcome::Reset,
            reset::Outcome::AlreadyRedeemed,
            reset::Outcome::NothingToReset,
            reset::Outcome::NoCredit,
        ] {
            for reconciled in [false, true] {
                let mut report = reading(session);
                report.outcome = Some(outcome);
                report.reconciled = reconciled;
                let output = framed_allowance_report(session, true, report).unwrap();
                assert_eq!(output.command, "account.redeem");
                assert_eq!(
                    output.data["outcome"],
                    serde_json::to_value(outcome).unwrap()
                );
                assert_eq!(output.data["reconciled"], reconciled);
            }
        }
        for unknown in [false, true] {
            let mut report = reading(session);
            if unknown {
                report.allowance.account = None;
                report.allowance.reset_credits = None;
                report.allowance.windows.clear();
            }
            assert_eq!(
                framed_allowance_report(session, false, report)
                    .unwrap()
                    .command,
                "account.allowance"
            );
        }
        for reconciled_only in [false, true] {
            let mut report = reading(session);
            if reconciled_only {
                report.reconciled = true;
            } else {
                report.outcome = Some(reset::Outcome::Reset);
            }
            let error = framed_allowance_report(session, false, report)
                .err()
                .unwrap();
            assert_eq!(error.kind, "account_allowance");
            assert!(error.message.contains("inspection reply unexpectedly"));
        }
    }

    #[test]
    fn observe_only_monitor_never_constructs_a_redeem_request() {
        let session = TerminalId::new();
        let mut requests = Vec::new();
        let result = monitor(
            session,
            90,
            1,
            2,
            1,
            false,
            |request| {
                assert_eq!(request.session, session);
                requests.push(request.action);
                Ok(reading(session))
            },
            |_| {},
        )
        .unwrap();
        assert!(
            requests
                .iter()
                .all(|action| matches!(action, Action::Inspect))
        );
        assert_eq!(requests.len(), 2);
        assert_eq!(result.data["successful_reset_results"], 0);
        assert_eq!(result.data["automatic_redemption_supported"], false);
    }
    #[test]
    fn authorized_monitors_refuse_before_any_provider_read_or_credit_choice() {
        for credit in ["capped-credit-a", "capped-credit-b"] {
            let session = TerminalId::new();
            let mut requests = 0;
            let error = monitor(
                session,
                90,
                1,
                10,
                1,
                true,
                |_| {
                    requests += 1;
                    let mut report = reading(session);
                    report
                        .allowance
                        .reset_credits
                        .as_mut()
                        .unwrap()
                        .details
                        .as_mut()
                        .unwrap()[0]
                        .id = credit.into();
                    Ok(report)
                },
                |_| panic!("unsupported automatic monitoring cannot wait"),
            )
            .err()
            .unwrap();
            assert_eq!(requests, 0);
            assert_eq!(error.message, AUTOMATIC_ADMISSION_UNAVAILABLE);
        }
    }
    #[test]
    fn read_only_monitor_reports_unknown_and_stale_state_without_an_effect() {
        for unknown in [true, false] {
            let session = TerminalId::new();
            let mut requests = 0;
            let error = monitor(
                session,
                90,
                1,
                2,
                1,
                false,
                |request| {
                    assert!(matches!(request.action, Action::Inspect));
                    requests += 1;
                    let mut report = reading(session);
                    if unknown {
                        report.allowance.account = None;
                    } else {
                        report.allowance.observed_at_ms = 0;
                    }
                    Ok(report)
                },
                |_| panic!("unavailable reading cannot spin"),
            )
            .err()
            .unwrap();
            assert_eq!(requests, 1);
            assert!(
                error
                    .message
                    .contains(if unknown { "UnknownAccount" } else { "Stale" })
            );
        }
    }
    #[test]
    fn read_only_monitor_preserves_provider_failure_and_never_retries() {
        let session = TerminalId::new();
        let mut requests = 0;
        let error = monitor(
            session,
            90,
            1,
            10,
            1,
            false,
            |request| {
                requests += 1;
                assert!(matches!(request.action, Action::Inspect));
                Err(AppError::new(
                    "provider_unavailable",
                    "Exact source allowance read failed",
                ))
            },
            |_| panic!("failed read cannot spin"),
        )
        .err()
        .unwrap();
        assert_eq!(requests, 1);
        assert_eq!(error.message, "Exact source allowance read failed");
    }
}
