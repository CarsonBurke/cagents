use super::super::*;
use crate::account::{
    AccountId,
    info::{AccountInfo, Query},
    resets::{ConsumeOutcome, ConsumeRequest, ResetScope, UsageReset},
};
use std::io::{BufRead, IsTerminal, Write};

pub(super) fn execute(
    account: AccountId,
    scope: Option<String>,
    reset_id: Option<String>,
    yes: bool,
    idempotency_key: Option<String>,
) -> Result<CommandOutput, AppError> {
    let interactive = std::io::stdin().is_terminal() && std::io::stderr().is_terminal();
    require_confirmation(yes, interactive)?;
    let recovery = idempotency_key.is_some();
    let key = idempotency_key.unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    validate_key(&key)?;
    let socket = control_daemon_socket()?;
    let mut client = ControlClient::connect(&socket)
        .map_err(|error| AppError::new("daemon_connection", error))?;
    let report = super::read_report(
        &mut client,
        Authority::from_environment(),
        Query {
            provider: None,
            account: Some(account.clone()),
            refresh: true,
        },
    )?;
    let info = report
        .accounts
        .iter()
        .find(|info| info.id == account)
        .ok_or_else(|| AppError::new("account_reset", "The account did not return usage"))?;
    // Refresh can retain a cached reading on failure. A retained count is not
    // sufficient evidence to spend an irreversible credit.
    if info.stale || info.identity_conflict || info.detail.is_some() {
        return Err(AppError::new(
            "account_reset",
            info.detail.as_deref().unwrap_or(
                "Refresh this account's usage and resolve its login before using a reset",
            ),
        ));
    }
    let resets = &info
        .usage
        .as_ref()
        .ok_or_else(|| AppError::new("account_reset", "Reset availability is unknown"))?
        .resets;
    let now = i64::try_from(report.sampled_at / 1000).unwrap_or(i64::MAX);
    let selection =
        prepare_selection(resets, scope.as_deref(), reset_id.as_deref(), recovery, now)?;
    let name = account_name(info);
    let prompt = confirmation(&account, &name, &selection.scope, selection.count, recovery);
    if !yes {
        let mut stderr = std::io::stderr().lock();
        write!(stderr, "{prompt} [y/N] ")
            .and_then(|_| stderr.flush())
            .map_err(|_| {
                AppError::new(
                    "confirmation",
                    "The confirmation prompt could not be displayed",
                )
            })?;
        let mut answer = String::new();
        std::io::stdin()
            .lock()
            .read_line(&mut answer)
            .map_err(|_| AppError::new("confirmation", "The confirmation could not be read"))?;
        if !accepted(&answer) {
            return Err(AppError::new("confirmation", "Reset kept"));
        }
    }
    let request = ConsumeRequest {
        account,
        reset_id: selection.id,
        idempotency_key: key,
        expected_identity: info.observed_identity.clone(),
    };
    let device = crate::device::selected().map_or("local", |device| device.name.as_str());
    let retry = retry_command(&request, &selection.scope, device);
    // Print recovery coordinates before sending: a dropped connection must
    // not leave the caller with no way to reuse the original request key.
    eprintln!("{prompt}\nIf the result is unconfirmed, refresh usage before retrying:\n{retry}");
    client
        .authenticate(Authority::from_environment())
        .map_err(protocol_error)?;
    let outcome = submit(&mut client, request.clone()).map_err(|error| {
        AppError::new("account_reset_unconfirmed", format!(
            "The reset result is unconfirmed: {}. Refresh account usage before retrying. Reuse the same reset and key: {retry}",
            error.message,
        ))
    })?;
    let output = output(&request, &outcome);
    if matches!(
        outcome.status,
        crate::account::resets::ConsumeStatus::Reset
            | crate::account::resets::ConsumeStatus::NotLimited
    ) {
        Ok(output)
    } else {
        Err(AppError {
            kind: "account_reset",
            message: output.human,
            details: Some(output.data),
        })
    }
}

fn require_confirmation(yes: bool, interactive: bool) -> Result<(), AppError> {
    if yes || interactive {
        return Ok(());
    }
    Err(AppError::new(
        "confirmation",
        "Using a reset requires --yes or an interactive confirmation",
    ))
}

fn validate_key(key: &str) -> Result<(), AppError> {
    if !key.is_empty()
        && key.len() <= 64
        && key
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Ok(());
    }
    Err(AppError::new(
        "account_reset",
        "Use an idempotency key of 1–64 letters, digits, hyphens or underscores",
    ))
}

fn accepted(answer: &str) -> bool {
    matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

fn account_name(info: &AccountInfo) -> String {
    info.label.clone().unwrap_or_else(|| info.id.to_string())
}

fn confirmation(
    account: &AccountId,
    name: &str,
    scope: &str,
    count: Option<u64>,
    recovery: bool,
) -> String {
    let account_name = if name == account.as_str() {
        name.to_owned()
    } else {
        format!("{name} ({account})")
    };
    let left = count.map_or_else(
        || "Current count unknown".into(),
        |count| format!("{count} left"),
    );
    if recovery {
        format!(
            "Retry the same {scope} reset for {account_name} with its original key? {left}. This will not select another reset."
        )
    } else {
        format!("Use one {scope} reset for {account_name}? {left}. This cannot be undone.")
    }
}

/// Confirmation metadata is separate from the provider inventory: a spent
/// credit may disappear after an unconfirmed attempt. Recovery keeps its ID,
/// original scope label and key, never a replacement selected from that list.
struct Selection {
    id: String,
    scope: String,
    count: Option<u64>,
}

fn prepare_selection(
    resets: &[UsageReset],
    scope: Option<&str>,
    reset_id: Option<&str>,
    recovery: bool,
    now: i64,
) -> Result<Selection, AppError> {
    if recovery
        && let Some(id) = reset_id
        && !resets.iter().any(|reset| reset.id == id)
    {
        let original_scope = scope.map(str::trim).filter(|scope| !scope.is_empty())
            .ok_or_else(|| AppError::new("account_reset", "The original reset is no longer listed. Retry with the exact reset ID, original idempotency key, and original --scope label printed by the first attempt"))?;
        return Ok(Selection {
            id: id.into(),
            scope: original_scope.into(),
            count: None,
        });
    }
    let reset = select_reset(resets, scope, reset_id, recovery, now)?;
    let count = resets
        .iter()
        .filter(|candidate| candidate.scopes == reset.scopes && candidate.is_available(now))
        .fold(0_u64, |count, candidate| {
            count.saturating_add(candidate.available_count)
        });
    Ok(Selection {
        id: reset.id.clone(),
        scope: reset.label(),
        count: Some(count),
    })
}

fn scope_matches(scope: &str, reset: &UsageReset) -> bool {
    reset.scopes.iter().any(|candidate| match candidate {
        ResetScope::AccountSession => scope.eq_ignore_ascii_case("session"),
        ResetScope::AccountWeekly => scope.eq_ignore_ascii_case("weekly"),
        ResetScope::Account => scope.eq_ignore_ascii_case("account"),
        ResetScope::Model(model) => {
            scope.eq_ignore_ascii_case(model)
                || model
                    .split_whitespace()
                    .next()
                    .is_some_and(|family| family.eq_ignore_ascii_case(scope))
        }
    })
}

fn select_reset<'a>(
    resets: &'a [UsageReset],
    scope: Option<&str>,
    reset_id: Option<&str>,
    recovery: bool,
    now: i64,
) -> Result<&'a UsageReset, AppError> {
    if let Some(id) = reset_id {
        // An explicit recovery never substitutes a new grant, even when the
        // first attempt already depleted the originally selected one.
        return resets.iter().find(|reset| reset.id == id && (recovery || reset.is_available(now)))
            .ok_or_else(|| AppError::new("account_reset", "The original reset is no longer listed. Its result remains unconfirmed; do not spend a different reset with this key"));
    }
    let mut candidates = resets
        .iter()
        .filter(|reset| {
            reset.is_available(now) && scope.is_none_or(|scope| scope_matches(scope, reset))
        })
        .collect::<Vec<_>>();
    candidates.sort_by_key(|reset| (reset.expires_at.unwrap_or(i64::MAX), reset.id.as_str()));
    if let Some(first) = candidates.first()
        && candidates
            .iter()
            .all(|candidate| candidate.scopes == first.scopes)
    {
        return Ok(first);
    }
    match candidates.as_slice() {
        [reset] => Ok(reset),
        [] => Err(AppError::new(
            "account_reset",
            "No usable reset matches this account and scope",
        )),
        _ => Err(AppError::new(
            "account_reset",
            format!(
                "Several resets are available. Choose --scope: {}",
                candidates
                    .iter()
                    .map(|reset| reset.label())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        )),
    }
}

fn retry_command(request: &ConsumeRequest, scope: &str, device: &str) -> String {
    format!(
        "cagents --device {} account reset {} --reset-id {} --scope {} --idempotency-key {} --yes",
        shell_operand(device),
        request.account,
        shell_operand(&request.reset_id),
        shell_operand(scope),
        request.idempotency_key
    )
}

fn shell_operand(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn submit(client: &mut ControlClient, request: ConsumeRequest) -> Result<ConsumeOutcome, AppError> {
    let reply = client
        .device_operation(crate::device_service::Operation::AccountReset { request })
        .map_err(protocol_error)?;
    let crate::device_service::Reply::AccountReset(value) = reply else {
        return Err(AppError::new("account_reset", "Unexpected reset response"));
    };
    serde_json::from_value(value)
        .map_err(|_| AppError::new("account_reset", "The reset response could not be read"))
}

fn output(request: &ConsumeRequest, outcome: &ConsumeOutcome) -> CommandOutput {
    let mut lines = vec![format!("{}: {:?}", request.account, outcome.status)];
    if let Some(message) = &outcome.message {
        lines.push(message.clone());
    }
    if let Some(snapshot) = &outcome.snapshot {
        lines.push(format!(
            "Refreshed usage (observed {}):",
            snapshot.observed_at
        ));
        for window in &snapshot.windows {
            lines.push(format!(
                "  {}: {:.0}% remaining",
                window.label,
                window.left() * 100.0
            ));
        }
        for reset in &snapshot.resets {
            lines.push(format!(
                "  {} resets: {} left",
                reset.label(),
                reset.available_count
            ));
        }
    } else {
        lines
            .push("Current usage could not be refreshed; do not assume a reset was unused.".into());
    }
    CommandOutput {
        command: "account.reset",
        human: lines.join("\n"),
        data: json!({ "request": request, "result": outcome }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account::resets::ResetKind;
    use clap::Parser;

    fn grant(id: &str, scopes: Vec<ResetScope>, count: u64) -> UsageReset {
        UsageReset {
            id: id.into(),
            kind: ResetKind::Granted,
            scopes,
            available_count: count,
            expires_at: Some(200),
            usable: true,
        }
    }

    #[test]
    fn noninteractive_reset_requires_yes_before_connecting() {
        assert!(require_confirmation(false, false).is_err());
        assert!(require_confirmation(true, false).is_ok());
        assert!(require_confirmation(false, true).is_ok());
        assert!(accepted(" YES\n"));
        for answer in ["", "n", "sure", "yesterday"] {
            assert!(!accepted(answer));
        }
        assert!(
            Cli::try_parse_from(["cagents", "account", "reset", "codex/system", "--yes"]).is_ok()
        );
        assert!(
            Cli::try_parse_from([
                "cagents",
                "account",
                "reset",
                "codex/system",
                "--idempotency-key",
                "original"
            ])
            .is_err()
        );
    }

    #[test]
    fn scope_selection_and_confirmation_keep_model_and_account_distinct() {
        let resets = vec![
            grant("model", vec![ResetScope::Model("Fable".into())], 2),
            grant("weekly", vec![ResetScope::AccountWeekly], 1),
        ];
        assert!(select_reset(&resets, None, None, false, 100).is_err());
        let fable = select_reset(&resets, Some("fable"), None, false, 100).unwrap();
        assert_eq!(fable.id, "model");
        assert_eq!(
            select_reset(&resets, Some("weekly"), None, false, 100)
                .unwrap()
                .id,
            "weekly"
        );
        assert!(select_reset(&resets, Some("session"), None, false, 100).is_err());
        assert!(select_reset(&resets, Some("fable"), None, false, 200).is_err());
        let text = confirmation(
            &"claude/system".parse().unwrap(),
            "Work",
            &fable.label(),
            Some(2),
            false,
        );
        assert!(text.contains("Work (claude/system)"));
        assert!(text.contains("Fable"));
        assert!(text.contains("2 left"));
    }

    #[test]
    fn recovery_uses_the_original_reset_even_after_inventory_changes() {
        let resets = vec![
            grant("original", vec![ResetScope::Account], 0),
            grant("next", vec![ResetScope::Account], 1),
        ];
        assert_eq!(
            select_reset(&resets, None, Some("original"), true, 100)
                .unwrap()
                .id,
            "original"
        );
        assert!(select_reset(&resets, None, Some("missing"), true, 100).is_err());
        assert!(validate_key("same-key_1").is_ok());
        assert!(validate_key("bad key").is_err());
    }
    #[test]
    fn same_scope_credits_choose_earliest_expiry_without_scope_dead_end() {
        let mut later = grant("later", vec![ResetScope::Account], 1);
        later.expires_at = Some(300);
        let resets = vec![later, grant("earlier", vec![ResetScope::Account], 1)];
        assert_eq!(
            select_reset(&resets, None, None, false, 100).unwrap().id,
            "earlier"
        );
        assert_eq!(
            select_reset(&resets, Some("account"), None, false, 100)
                .unwrap()
                .id,
            "earlier"
        );
        assert!(
            select_reset(
                &[grant("empty", vec![ResetScope::Account], 0)],
                None,
                Some("empty"),
                false,
                100
            )
            .is_err()
        );
    }

    #[test]
    fn submit_preserves_exact_reset_and_key_and_never_retries_a_disconnect() {
        use crate::{
            account::{UsageSnapshot, UsageStatus, resets::ConsumeStatus},
            model::{BootId, HostIncarnation, HostSessionId, HostSessionProof},
            terminal_ipc::{
                ClientEnvelope, ClientRequest, PROTOCOL_VERSION, ServerEnvelope, ServerMessage,
                ServerResponse, read_frame, write_frame,
            },
        };
        use std::{os::unix::net::UnixStream, thread};
        for status in [
            Some(ConsumeStatus::Reset),
            Some(ConsumeStatus::NoCredit),
            None,
        ] {
            let (stream, mut peer) = UnixStream::pair().unwrap();
            peer.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
            peer.set_write_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let request = ConsumeRequest {
                account: "codex/system".parse().unwrap(),
                reset_id: "original-credit".into(),
                idempotency_key: "original-key".into(),
                expected_identity: None,
            };
            let original = request.clone();
            let worker = thread::spawn(move || {
                let proof = HostSessionProof {
                    host_session_id: HostSessionId::new("reset-peer"),
                    boot_id: BootId::new("fixture-boot"),
                    host_incarnation: HostIncarnation::new("fixture-incarnation"),
                };
                let connect: ClientEnvelope = read_frame(&mut peer).unwrap().unwrap();
                assert!(matches!(connect.request, ClientRequest::Connect { .. }));
                write_frame(
                    &mut peer,
                    &ServerEnvelope {
                        version: PROTOCOL_VERSION,
                        request_id: Some(connect.request_id),
                        proof: proof.clone(),
                        message: ServerMessage::Response {
                            response: ServerResponse::Connected {
                                proof: proof.clone(),
                                resumed: vec![],
                            },
                        },
                    },
                )
                .unwrap();
                let sent: ClientEnvelope = read_frame(&mut peer).unwrap().unwrap();
                assert_eq!(sent.proof.as_ref(), Some(&proof));
                assert_eq!(
                    sent.request,
                    ClientRequest::DeviceOperation {
                        operation: crate::device_service::Operation::AccountReset {
                            request: original
                        },
                    }
                );
                if let Some(status) = status {
                    let outcome = ConsumeOutcome {
                        status,
                        message: None,
                        snapshot: Some(UsageSnapshot::state(UsageStatus::Ok, "fixture", 123)),
                    };
                    write_frame(
                        &mut peer,
                        &ServerEnvelope {
                            version: PROTOCOL_VERSION,
                            request_id: Some(sent.request_id),
                            proof,
                            message: ServerMessage::Response {
                                response: ServerResponse::DeviceOperation {
                                    reply: crate::device_service::Reply::AccountReset(
                                        serde_json::to_value(outcome).unwrap(),
                                    ),
                                },
                            },
                        },
                    )
                    .unwrap();
                    assert!(
                        read_frame::<ClientEnvelope>(&mut peer).unwrap().is_none(),
                        "consume must not retry"
                    );
                }
            });
            let mut client = ControlClient::from_stream(stream).unwrap();
            let result = submit(&mut client, request.clone());
            drop(client);
            worker.join().unwrap();
            match status {
                Some(status) => {
                    let outcome = result.unwrap();
                    assert_eq!(outcome.status, status);
                    let output = output(&request, &outcome);
                    assert_eq!(output.data["request"]["idempotency_key"], "original-key");
                    assert_eq!(output.data["result"]["snapshot"]["observed_at"], 123);
                }
                None => assert!(result.is_err()),
            }
        }
    }
    #[test]
    fn recovery_reaches_original_operation_when_credit_disappears() {
        // A successful first request can redeem a credit before its reply is
        // lost. The next inventory may list only an entirely different credit.
        let resets = vec![grant("next-credit", vec![ResetScope::Account], 1)];
        let selected = prepare_selection(
            &resets,
            Some("Account limits"),
            Some("original-credit"),
            true,
            100,
        )
        .unwrap();
        assert_eq!(selected.id, "original-credit");
        assert_eq!(selected.scope, "Account limits");
        assert_eq!(selected.count, None);
        let account: AccountId = "codex/system".parse().unwrap();
        let prompt = confirmation(&account, "Work", &selected.scope, selected.count, true);
        assert!(prompt.contains("Retry the same Account limits reset"));
        assert!(prompt.contains("Work (codex/system)"));
        assert!(prompt.contains("Current count unknown"));
        let request = ConsumeRequest {
            account,
            reset_id: selected.id,
            idempotency_key: "original-key".into(),
            expected_identity: None,
        };
        let command = retry_command(&request, &selected.scope, "local");
        assert!(command.contains("--reset-id 'original-credit'"));
        assert!(command.contains("--scope 'Account limits'"));
        assert!(command.contains("--idempotency-key original-key"));
        assert!(!command.contains("next-credit"));
        assert!(
            Cli::try_parse_from([
                "cagents",
                "account",
                "reset",
                "codex/system",
                "--reset-id",
                "original-credit",
                "--scope",
                "Account limits",
                "--idempotency-key",
                "original-key",
                "--yes"
            ])
            .is_ok()
        );
        assert!(prepare_selection(&resets, None, Some("original-credit"), true, 100).is_err());
        assert!(
            prepare_selection(
                &resets,
                Some("Account limits"),
                Some("original-credit"),
                false,
                100
            )
            .is_err()
        );
    }
    #[test]
    fn recovery_command_preserves_the_selected_remote_device() {
        let request = ConsumeRequest {
            account: "claude/system".parse().unwrap(),
            reset_id: "cedar_ember/grant".into(),
            idempotency_key: "original-key".into(),
            expected_identity: None,
        };
        let command = retry_command(&request, "Weekly", "remote-work");
        assert!(command.starts_with("cagents --device 'remote-work' account reset claude/system "));
        assert!(command.contains("--reset-id 'cedar_ember/grant'"));
        assert!(command.contains("--idempotency-key original-key"));
        let parsed = Cli::try_parse_from([
            "cagents",
            "--device",
            "remote-work",
            "account",
            "reset",
            "claude/system",
            "--reset-id",
            "cedar_ember/grant",
            "--scope",
            "Weekly",
            "--idempotency-key",
            "original-key",
            "--yes",
        ])
        .unwrap();
        assert_eq!(parsed.device.as_deref(), Some("remote-work"));
        assert!(
            retry_command(&request, "Weekly", "local").starts_with("cagents --device 'local' ")
        );
    }
}
