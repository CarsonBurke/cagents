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
    let mut client = control_daemon_client()?;
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
    let prompt = confirmation(&account, &name, &selection, recovery);
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

fn confirmation(account: &AccountId, name: &str, selection: &Selection, recovery: bool) -> String {
    let account_name = if name == account.as_str() {
        name.to_owned()
    } else {
        format!("{name} ({account})")
    };
    if !recovery && let Some(reset) = &selection.reset {
        return format!(
            "{} {}",
            reset.confirmation_question(&account_name),
            reset.confirmation_effect(selection.count.unwrap_or(reset.available_count)),
        );
    }
    let available = selection.count.map_or_else(
        || "Current available count unknown".into(),
        |count| format!("{count} available"),
    );
    format!(
        "Retry the same {} reset for {account_name} with its original key? {available}. This will not select another reset.",
        selection.scope,
    )
}

/// Confirmation metadata is separate from the provider inventory: a spent
/// credit may disappear after an unconfirmed attempt. Recovery keeps its ID,
/// original scope label and key, never a replacement selected from that list.
struct Selection {
    id: String,
    scope: String,
    count: Option<u64>,
    /// The original inventory entry freezes the shared confirmation wording.
    /// A disappeared-credit recovery has only the original scope label.
    reset: Option<UsageReset>,
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
            reset: None,
        });
    }
    let reset = select_reset(resets, scope, reset_id, recovery, now)?;
    let count = resets
        .iter()
        .filter(|candidate| same_scopes(candidate, reset) && candidate.is_available(now))
        .fold(0_u64, |count, candidate| {
            count.saturating_add(candidate.available_count)
        });
    Ok(Selection {
        id: reset.id.clone(),
        scope: reset.label(),
        count: Some(count),
        reset: Some(reset.clone()),
    })
}

/// How a `--scope` argument names a reset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScopeMatch {
    /// The argument names every limit the reset clears, e.g. `Fable` for a
    /// Fable-only reset or `Weekly + Fable` for one clearing both.
    Exact,
    /// The reset also clears model allowances the argument does not name,
    /// as `weekly` names Claude's Weekly reset that also refills Fable.
    Broader,
    /// The reset also clears an account-wide limit the argument does not
    /// name. Spending it takes its full label: `Fable` never spends a reset
    /// that also clears the Weekly limit.
    Wider,
}

fn names_scope(name: &str, scope: &ResetScope) -> bool {
    match scope {
        ResetScope::AccountSession => name.eq_ignore_ascii_case("session"),
        ResetScope::AccountWeekly => name.eq_ignore_ascii_case("weekly"),
        ResetScope::Account => {
            name.eq_ignore_ascii_case("account") || name.eq_ignore_ascii_case(&scope.label())
        }
        ResetScope::Model(model) => {
            name.eq_ignore_ascii_case(model)
                || model
                    .split_whitespace()
                    .next()
                    .is_some_and(|family| family.eq_ignore_ascii_case(name))
        }
    }
}

fn scope_match(scope: &str, reset: &UsageReset) -> Option<ScopeMatch> {
    let names = scope
        .split('+')
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .collect::<Vec<_>>();
    if names.is_empty()
        || !names
            .iter()
            .all(|name| reset.scopes.iter().any(|scope| names_scope(name, scope)))
    {
        return None;
    }
    let mut unnamed = reset
        .scopes
        .iter()
        .filter(|scope| !names.iter().any(|name| names_scope(name, scope)))
        .peekable();
    Some(if unnamed.peek().is_none() {
        ScopeMatch::Exact
    } else if unnamed.all(|scope| matches!(scope, ResetScope::Model(_))) {
        ScopeMatch::Broader
    } else {
        ScopeMatch::Wider
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
    // A scope naming a reset exactly never falls through to a broader one,
    // and none falls through to a reset clearing an unnamed account-wide
    // limit: `--scope Fable` must not spend a Weekly reset that also clears
    // Fable, whether or not a Fable reset is held.
    let exact = scope.and_then(|scope| {
        resets
            .iter()
            .filter(|reset| reset.available_count > 0 && reset.expires_at.is_none_or(|at| at > now))
            .find(|reset| scope_match(scope, reset) == Some(ScopeMatch::Exact))
    });
    let mut candidates = resets
        .iter()
        .filter(|reset| {
            reset.is_available(now)
                && scope.is_none_or(|scope| match scope_match(scope, reset) {
                    Some(ScopeMatch::Exact) => true,
                    Some(ScopeMatch::Broader) => exact.is_none(),
                    Some(ScopeMatch::Wider) | None => false,
                })
        })
        .collect::<Vec<_>>();
    candidates.sort_by_key(|reset| (reset.expires_at.unwrap_or(i64::MAX), reset.id.as_str()));
    if let Some(first) = candidates.first()
        && candidates
            .iter()
            .all(|candidate| same_scopes(candidate, first))
    {
        return Ok(first);
    }
    match candidates.as_slice() {
        [reset] => Ok(reset),
        [] => {
            // Usable resets that clear more than was named, offered by their
            // full label so spending one is always an explicit choice.
            let wider = scope
                .map(|scope| {
                    resets
                        .iter()
                        .filter(|reset| {
                            reset.is_available(now)
                                && matches!(
                                    scope_match(scope, reset),
                                    Some(ScopeMatch::Broader | ScopeMatch::Wider)
                                )
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            let refusal = match exact {
                Some(held) => format!("The {} reset can't be used right now", held.label()),
                None => "No usable reset matches this account and scope".into(),
            };
            Err(AppError::new(
                "account_reset",
                if wider.is_empty() {
                    refusal
                } else {
                    format!(
                        "{refusal}. To spend a reset that also clears other limits, choose --scope {}",
                        quoted_labels(&wider).join(" or ")
                    )
                },
            ))
        }
        _ => Err(AppError::new(
            "account_reset",
            format!(
                "Several resets are available. Choose --scope: {}",
                quoted_labels(&candidates).join(", ")
            ),
        )),
    }
}

/// Grants list the limits they clear in the provider's order, so two grants
/// clearing the same limits are one choice whatever order they list them in.
fn same_scopes(left: &UsageReset, right: &UsageReset) -> bool {
    crate::account::resets::ordered_scopes(&left.scopes)
        == crate::account::resets::ordered_scopes(&right.scopes)
}

/// Each distinct reset label once, quoted, in the given order.
fn quoted_labels(resets: &[&UsageReset]) -> Vec<String> {
    let mut labels: Vec<String> = Vec::new();
    for reset in resets {
        let label = format!("'{}'", reset.label());
        if !labels.contains(&label) {
            labels.push(label);
        }
    }
    labels
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
        let now_secs = i64::try_from(snapshot.observed_at / 1000).unwrap_or(i64::MAX);
        lines.extend(
            crate::account::resets::inventory_lines(&snapshot.resets, now_secs)
                .into_iter()
                .map(|line| format!("  {line}")),
        );
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
        let selection = prepare_selection(&resets, Some("fable"), None, false, 100).unwrap();
        let text = confirmation(&"claude/system".parse().unwrap(), "Work", &selection, false);
        assert!(text.contains("Work (claude/system)"));
        assert!(text.contains("Fable"));
        assert_eq!(
            text,
            "Use a Fable reset for Work (claude/system)? This clears the Fable weekly limit now. 2 available; this can’t be undone."
        );
    }

    /// Claude's Weekly reset also refills Fable, while its Fable reset
    /// refills only Fable and is held while Fable is unused.
    #[test]
    fn a_named_scope_never_spends_a_broader_reset_in_its_place() {
        let weekly_and_fable = || {
            grant(
                "cedar_ember/full-reset-2611",
                vec![ResetScope::AccountWeekly, ResetScope::Model("Fable".into())],
                1,
            )
        };
        let mut held_fable = grant(
            "cedar_ember/launch-2610-fable",
            vec![ResetScope::Model("Fable".into())],
            1,
        );
        held_fable.usable = false;
        let resets = vec![weekly_and_fable(), held_fable.clone()];
        assert_eq!(
            select_reset(&resets, Some("weekly"), None, false, 100)
                .unwrap()
                .id,
            "cedar_ember/full-reset-2611"
        );
        assert_eq!(
            select_reset(&resets, None, None, false, 100).unwrap().id,
            "cedar_ember/full-reset-2611",
            "the only usable reset needs no scope"
        );
        let refused = select_reset(&resets, Some("Fable"), None, false, 100).unwrap_err();
        assert_eq!(
            refused.message,
            "The Fable reset can't be used right now. To spend a reset that also clears other limits, choose --scope 'Weekly + Fable'"
        );
        for exact in ["Weekly + Fable", "fable+weekly"] {
            assert_eq!(
                select_reset(&resets, Some(exact), None, false, 100)
                    .unwrap()
                    .id,
                "cedar_ember/full-reset-2611"
            );
        }
        let selection = prepare_selection(&resets, Some("weekly"), None, false, 100).unwrap();
        assert_eq!(
            confirmation(&"claude/system".parse().unwrap(), "Plan", &selection, false),
            "Use a Weekly + Fable reset for Plan (claude/system)? This clears the Weekly limit and the Fable weekly limit now. 1 available; this can’t be undone."
        );
        assert_eq!(selection.scope, "Weekly + Fable", "one name everywhere");
        let mut reordered = grant(
            "cedar_ember/full-reset-2612",
            vec![ResetScope::Model("Fable".into()), ResetScope::AccountWeekly],
            1,
        );
        reordered.expires_at = weekly_and_fable().expires_at.map(|at| at + 1);
        let resets = vec![weekly_and_fable(), reordered, held_fable.clone()];
        let selection = prepare_selection(&resets, Some("weekly"), None, false, 100).unwrap();
        assert_eq!(
            (selection.id.as_str(), selection.count),
            ("cedar_ember/full-reset-2611", Some(2)),
            "the order a grant lists its limits in doesn't split one reset into two"
        );
        assert_eq!(
            select_reset(&resets, Some("Fable"), None, false, 100)
                .unwrap_err()
                .message,
            "The Fable reset can't be used right now. To spend a reset that also clears other limits, choose --scope 'Weekly + Fable'"
        );

        // Without any Fable reset, `--scope Fable` still never spends the
        // Weekly reset; its full label does.
        let only_weekly = [weekly_and_fable()];
        assert_eq!(
            select_reset(&only_weekly, Some("Fable"), None, false, 100)
                .unwrap_err()
                .message,
            "No usable reset matches this account and scope. To spend a reset that also clears other limits, choose --scope 'Weekly + Fable'"
        );
        assert_eq!(
            select_reset(&only_weekly, Some("weekly"), None, false, 100)
                .unwrap()
                .id,
            "cedar_ember/full-reset-2611"
        );

        held_fable.usable = true;
        let resets = vec![weekly_and_fable(), held_fable];
        assert_eq!(
            select_reset(&resets, Some("fable"), None, false, 100)
                .unwrap()
                .id,
            "cedar_ember/launch-2610-fable"
        );
        assert_eq!(
            select_reset(&resets, Some("weekly"), None, false, 100)
                .unwrap()
                .id,
            "cedar_ember/full-reset-2611"
        );
        assert!(
            select_reset(&resets, None, None, false, 100)
                .unwrap_err()
                .message
                .contains("Choose --scope")
        );
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
        let prompt = confirmation(&account, "Work", &selected, true);
        assert!(prompt.contains("Retry the same Account limits reset"));
        assert!(prompt.contains("Work (codex/system)"));
        assert!(prompt.contains("Current available count unknown"));
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
        assert_eq!(parsed.global.device.as_deref(), Some("remote-work"));
        assert!(
            retry_command(&request, "Weekly", "local").starts_with("cagents --device 'local' ")
        );
    }
    #[test]
    fn combined_confirmation_names_both_cleared_windows_and_current_count() {
        let resets = vec![grant(
            "combined",
            vec![ResetScope::AccountSession, ResetScope::AccountWeekly],
            2,
        )];
        let selection = prepare_selection(&resets, None, None, false, 100).unwrap();
        let account = "claude/system".parse().unwrap();
        let prompt = confirmation(&account, "Work", &selection, false);
        assert_eq!(
            prompt,
            "Use a Session + Weekly reset for Work (claude/system)? This clears the Session and Weekly limits now. 2 available; this can’t be undone."
        );
        assert_eq!(
            selection.scope, "Session + Weekly",
            "recovery scope stays stable"
        );
    }

    #[test]
    fn account_confirmation_uses_shared_effect_and_sums_same_scope_credit_count() {
        let mut paused = grant("paused", vec![ResetScope::Account], 9);
        paused.usable = false;
        let resets = vec![
            grant("first", vec![ResetScope::Account], 1),
            grant("second", vec![ResetScope::Account], 1),
            paused,
        ];
        let selection = prepare_selection(&resets, None, None, false, 100).unwrap();
        let account = "codex/system".parse().unwrap();
        let prompt = confirmation(&account, "codex/system", &selection, false);
        assert_eq!(
            prompt,
            "Use an account reset for codex/system? This clears the account usage limits now. 2 available; this can’t be undone."
        );
        assert!(!prompt.contains("codex/system (codex/system)"));
    }
    #[test]
    fn info_and_reset_result_share_grouped_scope_first_inventory() {
        use crate::account::{
            UsageSnapshot, UsageStatus,
            info::{AccountInfo, Report},
            resets::ConsumeStatus,
        };
        let mut first = grant("fable-first", vec![ResetScope::Model("Fable".into())], 1);
        first.expires_at = None;
        let mut second = first.clone();
        second.id = "fable-second".into();
        let mut session = grant("session", vec![ResetScope::AccountSession], 1);
        session.expires_at = None;
        let mut held = grant(
            "combined-held",
            vec![ResetScope::AccountSession, ResetScope::AccountWeekly],
            1,
        );
        held.expires_at = None;
        held.usable = false;
        let mut snapshot = UsageSnapshot::state(UsageStatus::Ok, "fixture", 100_000);
        snapshot.resets = vec![first, second, session, held];
        let request = ConsumeRequest {
            account: "claude/system".parse().unwrap(),
            reset_id: "fable-first".into(),
            idempotency_key: "same-operation".into(),
            expected_identity: None,
        };
        let mut account = AccountInfo {
            id: request.account.clone(),
            provider: crate::agent::Provider::Claude,
            label: Some("Work".into()),
            registered_identity: None,
            observed_identity: None,
            identity_conflict: false,
            selected_by_caller: false,
            default_for_new_chats: false,
            archived: false,
            usage: None,
            remaining: vec![],
            credits: None,
            credits_remaining_fraction: None,
            credits_remaining: None,
            age_ms: None,
            stale: false,
            detail: None,
        };
        account.set_usage(snapshot.clone(), snapshot.observed_at);
        let report = Report {
            sampled_at: snapshot.observed_at,
            caller: None,
            accounts: vec![account],
            diagnostics: vec![],
        };
        let info = super::super::human(&report);
        let outcome = ConsumeOutcome {
            status: ConsumeStatus::Reset,
            message: None,
            snapshot: Some(snapshot.clone()),
        };
        let result = output(&request, &outcome);
        let inventory = crate::account::resets::inventory_lines(&snapshot.resets, 100);
        assert_eq!(
            inventory.len(),
            3,
            "equivalent Fable grants share one inventory line"
        );
        assert!(
            inventory
                .iter()
                .any(|line| line.starts_with("2 Fable resets"))
        );
        assert!(
            inventory
                .iter()
                .any(|line| line.starts_with("1 Session reset"))
        );
        assert!(
            inventory
                .iter()
                .any(|line| line.contains("1 Session + Weekly reset · not usable now"))
        );
        for line in inventory {
            assert!(info.lines().any(|rendered| rendered == format!("  {line}")));
            assert!(
                result
                    .human
                    .lines()
                    .any(|rendered| rendered == format!("  {line}"))
            );
            assert!(!line.contains("available"));
            assert!(!line.contains("left"));
        }
        assert_eq!(
            result.data["result"]["snapshot"]["resets"]
                .as_array()
                .unwrap()
                .len(),
            4,
            "human grouping must not change JSON inventory"
        );
        assert_eq!(
            serde_json::to_value(report).unwrap()["accounts"][0]["usage"]["resets"]
                .as_array()
                .unwrap()
                .len(),
            4
        );
    }
}
