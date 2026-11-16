//! Conservative, value-only presentation of provider failures. The original
//! diagnostic remains borrowed and unchanged; this never changes stored history,
//! provider state, retry policy, or account authority.
use std::borrow::Cow;

use crate::agent::Provider;

fn provider_marker(provider: Provider) -> (&'static str, &'static str) {
    match provider {
        Provider::Claude => ("provider-error:claude", "Claude"),
        Provider::Codex => ("provider-error:codex", "Codex"),
        Provider::Omp => ("provider-error:omp", "Oh My Pi"),
    }
}

/// Adapter-only provenance, using the existing delivery representation. Call
/// only when the provider explicitly marked this exact text as a failure.
/// A child/user quotation being rendered as System text is not enough.
pub fn mark_provider_error(provider: Provider, item: &mut super::Item) -> bool {
    let super::ItemContent::Text { text, .. } = &mut item.content else {
        return false;
    };
    let body = std::mem::take(text);
    let (from, sender) = provider_marker(provider);
    item.role = super::Role::System;
    item.content = super::ItemContent::Delivery {
        kind: super::DeliveryKind::Notice,
        from: from.into(),
        sender: sender.into(),
        summary: "Provider request failed".into(),
        body,
    };
    true
}

/// Ends a child provider failure whose resident body is a bounded preview;
/// only its retained copy source holds the complete original.
/// Written before retention is decided, so it states only that the text is
/// partial; the failure card says whether the full original can be copied.
pub const PREVIEW_OMITTED: &str = "\n[Diagnostic preview shortened]";

/// Whether a provider failure's resident text is only a preview of it.
pub fn is_preview(text: &str) -> bool {
    text.ends_with(PREVIEW_OMITTED)
}

/// Require the full tuple and paired provider. Existing task/peer deliveries
/// can carry arbitrary names, so a matching `from` field alone is insufficient.
pub fn provider_error_text(provider: Provider, item: &super::Item) -> Option<&str> {
    let (expected_from, expected_sender) = provider_marker(provider);
    match &item.content {
        super::ItemContent::Delivery {
            kind: super::DeliveryKind::Notice,
            from,
            sender,
            summary,
            body,
        } if item.role == super::Role::System
            && from == expected_from
            && sender == expected_sender
            && summary == "Provider request failed" =>
        {
            Some(body)
        }
        _ => None,
    }
}

/// Claude text block IDs are generated as `<message-id>:<block-index>`.
/// Group only explicitly marked blocks from that proven carrier; other provider
/// ID formats and unmarked quotations have no inferred carrier identity.
pub fn provider_error_carrier(provider: Provider, item: &super::Item) -> Option<&str> {
    if provider != Provider::Claude {
        return None;
    }
    provider_error_text(provider, item)?;
    let (carrier, index) = item.id.rsplit_once(':')?;
    (!carrier.is_empty() && !index.is_empty() && index.bytes().all(|byte| byte.is_ascii_digit()))
        .then_some(carrier)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FailureKind {
    AccountLimit,
    Authentication,
    Capacity,
    Connection,
    Interrupted,
    /// The system stopped the provider because memory ran out.
    OutOfMemory,
    /// The system stopped one of the provider's commands because memory ran
    /// out; the provider kept running.
    CommandOutOfMemory,
    /// The provider process ended without being asked to.
    Exited,
}

/// Opens the runtime's report of a provider that memory exhaustion ended.
/// The runtime composes these; they are not provider text.
pub const PROVIDER_OUT_OF_MEMORY: &str = "The system ran out of memory, so the agent was stopped.";
/// Opens the runtime's report of a command that memory exhaustion ended
/// while its provider kept running.
pub const COMMAND_OUT_OF_MEMORY: &str =
    "The system ran out of memory, so a command the agent started was stopped.";
/// Opens the runtime's report of a provider process that exited on its own.
pub const PROVIDER_EXITED: &str = "Provider exited with ";

/// Whether a reported error tells of something the agent carried on past,
/// rather than a failure of the conversation. It still gets its notice in
/// the transcript, but the conversation's state does not become an error.
pub fn is_notice(message: &str) -> bool {
    message.trim_start().starts_with(COMMAND_OUT_OF_MEMORY)
}

/// What a runtime exit report says happened to the provider, in words: the
/// signal or status itself stays in the details.
fn exit_summary(name: &str, message: &str) -> String {
    let status = message
        .find(PROVIDER_EXITED)
        .and_then(|at| message[at + PROVIDER_EXITED.len()..].lines().next())
        .map(str::trim)
        .unwrap_or_default();
    if let Some(code) = status.strip_prefix("exit status: ") {
        // A report can continue on the same line with the provider's output.
        let end = code
            .find(|c: char| !(c.is_ascii_digit() || c == '-'))
            .unwrap_or(code.len());
        if end > 0 {
            return format!("{name} quit unexpectedly with code {}", &code[..end]);
        }
    }
    // `9 (SIGKILL)`, possibly followed by `(core dumped)`.
    let signal = status
        .strip_prefix("signal: ")
        .and_then(|signal| signal.split_once('('))
        .and_then(|(_, name)| name.split_once(')'))
        .map(|(name, _)| name.trim());
    match signal {
        Some("SIGKILL") => format!("{name} was force-stopped"),
        Some("SIGTERM" | "SIGINT" | "SIGHUP" | "SIGQUIT") => format!("{name} was stopped"),
        Some(_) => format!("{name} crashed"),
        None => format!("{name} stopped unexpectedly"),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure<'a> {
    pub kind: FailureKind,
    /// Group only within the same exact conversation/account. A known account
    /// limit with a reported reset has semantic identity; other incidents keep
    /// the exact original text so different failures cannot silently disappear.
    pub key: Cow<'a, str>,
    pub summary: String,
    /// Guidance, never an instruction to the provider or an automatic retry.
    pub action: &'static str,
    pub reset: Option<String>,
    pub details: &'a str,
}

impl<'a> Failure<'a> {
    /// Call only for provider error/system/tool-failure surfaces, never arbitrary
    /// user or assistant prose. Unknown failures retain the caller's fallback.
    pub fn parse(provider: Provider, message: &'a str) -> Option<Self> {
        // Provider diagnostics can include many megabytes of request bodies.
        // Classifying a visible notice must have a bounded allocation/work cost.
        let prefix: String = message.chars().take(8192).collect();
        let (structured_kind, signal) = classification_signal(&prefix)?;
        let lower = signal.to_ascii_lowercase();
        let contains = |needles: &[&str]| needles.iter().any(|s| lower.contains(s));
        // The runtime's own exit reports lead their message; provider text
        // never starts with them.
        let runtime_report = signal.trim_start();
        let reached = reached_limit(&signal);
        let kind = if let Some(kind) = structured_kind {
            kind
        } else if runtime_report.starts_with(PROVIDER_OUT_OF_MEMORY) {
            FailureKind::OutOfMemory
        } else if runtime_report.starts_with(COMMAND_OUT_OF_MEMORY) {
            FailureKind::CommandOutOfMemory
        } else if reached.is_some()
            || contains(&[
                "usage limit is reached",
                "usage limit reached",
                "session limit reached",
                "rate_limit_error",
                "claude: rate_limit",
                // A subagent's API failure as Claude Code reports it to its
                // parent: "… (error type rate_limit, HTTP 429, request id …)".
                "error type rate_limit",
                "usage_limit_reached",
                "insufficient_quota",
                "quota exceeded",
                "exceeded your current quota",
            ])
        {
            FailureKind::AccountLimit
        } else if super::requires_sign_in(provider, &signal)
            || contains(&[
                "authentication_error",
                "invalid_api_key",
                "invalid api key",
                "authentication failed",
                "please log in again",
                "please sign in again",
            ])
        {
            FailureKind::Authentication
        } else if contains(&[
            "overloaded_error",
            "provider is at capacity",
            "server is overloaded",
            "temporarily overloaded",
        ]) {
            FailureKind::Capacity
        } else if contains(&[
            "connection reset by peer",
            "connection refused",
            "network is unreachable",
            "connection timed out",
            "websocket connection closed",
        ]) {
            FailureKind::Connection
        } else if contains(&[
            "request was interrupted",
            "request was cancelled",
            "request was canceled",
            "turn was interrupted",
        ]) {
            FailureKind::Interrupted
        } else if runtime_report.starts_with(PROVIDER_EXITED) {
            // Last: the output it carries often names a more specific cause.
            FailureKind::Exited
        } else {
            return None;
        };
        let name = match provider {
            Provider::Claude => "Claude",
            Provider::Codex => "Codex",
            Provider::Omp => "Oh My Pi",
        };
        // What stopped is the provider's program, named as the stopped
        // chat's controls name it, rather than its service.
        let program = provider.label();
        let reset = (kind == FailureKind::AccountLimit)
            .then(|| reported_reset(&signal))
            .flatten();
        let (mut summary, action) = match kind {
            FailureKind::AccountLimit => (
                format!("{name} {} limit reached", limit_name(reached)),
                limit_action(limit_name(reached)),
            ),
            FailureKind::Authentication => (
                format!("{name} sign-in needs attention"),
                "Check the selected account and sign in again.",
            ),
            FailureKind::Capacity => (
                format!("{name} is temporarily at capacity"),
                "Try again when the provider is available.",
            ),
            FailureKind::Connection => (
                format!("{name} connection was interrupted"),
                "Check the connection before retrying.",
            ),
            FailureKind::Interrupted => (
                format!("{name} request was interrupted"),
                "Review the retained work before continuing.",
            ),
            FailureKind::OutOfMemory => (
                format!("{program} stopped because the system ran out of memory"),
                "The system stopped the agent to keep the computer responsive. Free up memory before you resume the conversation.",
            ),
            FailureKind::CommandOutOfMemory => (
                // Its step reads "failed", as the agent reported it: this
                // says why, in the same word.
                "A command failed because the system ran out of memory".to_owned(),
                "The system stopped the command to free memory. The agent is still running and sees the failure.",
            ),
            FailureKind::Exited => (
                exit_summary(program, &signal),
                // The stopped chat's bar offers what fits, Resume or Try
                // again; this only says where the cause is.
                "Details show what the agent reported before it stopped.",
            ),
        };
        if let Some(reset) = &reset {
            summary.push_str(" · resets ");
            summary.push_str(reset);
        }
        let key = match &reset {
            // Distinct limits that happen to reset together stay distinct.
            Some(reset) => Cow::Owned(format!(
                "{provider:?}:account-limit:{}:{}",
                limit_name(reached).to_ascii_lowercase(),
                reset.to_ascii_lowercase()
            )),
            None => Cow::Borrowed(message),
        };
        Some(Self {
            kind,
            key,
            summary,
            action,
            reset,
            details: message,
        })
    }
}

/// A failure's message in one line: its first line of prose, or "Request
/// failed" for a raw provider payload, which reads as nothing.
pub fn headline(message: &str) -> &str {
    let first = message
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or_default()
        .trim();
    if first.starts_with(['{', '[']) || first.starts_with("API Error:") {
        "Request failed"
    } else {
        first
    }
}

/// A failure as one line for a listing: what it means when it is a known
/// kind, such as a usage limit and its reset, and otherwise its headline.
pub fn summary(provider: Provider, message: &str) -> Cow<'_, str> {
    match Failure::parse(provider, message) {
        Some(failure) => Cow::Owned(failure.summary),
        None => Cow::Borrowed(headline(message)),
    }
}

/// Inspect only the provider error fields, never a quoted request, prompt, or
/// model payload. Known error types take precedence over incidental wording in
/// their message. Plain diagnostics keep the existing phrase-based behavior.
fn classification_signal(prefix: &str) -> Option<(Option<FailureKind>, Cow<'_, str>)> {
    let trimmed = prefix.trim_start();
    // Claude also renders HTTP failures as `API Error: 429 {"error": ...}`.
    // The status is an envelope, not evidence of an account failure by itself.
    let json = if trimmed.starts_with(['{', '[']) {
        Some(trimmed)
    } else {
        trimmed.strip_prefix("API Error:").and_then(|tail| {
            let (status, body) = tail.split_at(tail.find('{')?);
            (status.trim().is_empty() || status.trim().parse::<u16>().is_ok()).then_some(body)
        })
    };
    let Some(json) = json else {
        return Some((None, Cow::Borrowed(prefix)));
    };
    // A truncated/invalid structured error cannot safely be searched as prose:
    // doing so would promote request-body quotations into provider authority.
    let value: serde_json::Value = serde_json::from_str(json).ok()?;
    let kind_of = |error: &serde_json::Value| {
        error
            .get("type")
            .and_then(serde_json::Value::as_str)
            .and_then(structured_kind)
            .or_else(|| {
                error
                    .get("code")
                    .and_then(serde_json::Value::as_str)
                    .and_then(structured_kind)
            })
    };
    let direct_kind = kind_of(&value);
    let direct_message = value.get("message").and_then(serde_json::Value::as_str);
    let generic_envelope = value.get("type").and_then(serde_json::Value::as_str) == Some("error");
    // A secondary raw `error` field cannot override an actual direct error.
    // Unwrap only a generic envelope or one without a direct kind/message.
    let error = if direct_kind.is_some() || (direct_message.is_some() && !generic_envelope) {
        &value
    } else {
        value
            .get("error")
            .filter(|error| error.is_object() || error.is_string())
            .unwrap_or(&value)
    };
    let kind = kind_of(error);
    let message = error
        .as_str()
        .or_else(|| error.get("message").and_then(serde_json::Value::as_str));
    if kind.is_none() && message.is_none() {
        return None;
    }
    Some((kind, Cow::Owned(message.unwrap_or_default().to_owned())))
}

fn structured_kind(kind: &str) -> Option<FailureKind> {
    match kind {
        "rate_limit_error" | "insufficient_quota" => Some(FailureKind::AccountLimit),
        "authentication_error" | "invalid_api_key" => Some(FailureKind::Authentication),
        "overloaded_error" | "capacity_error" => Some(FailureKind::Capacity),
        "connection_error" | "api_connection_error" | "network_error" => {
            Some(FailureKind::Connection)
        }
        "interrupted_error" => Some(FailureKind::Interrupted),
        _ => None,
    }
}

/// What a "You've hit your … limit" message says was hit, up to "limit" on
/// the same line: empty for Claude's "You've hit your limit", "weekly" or
/// "monthly spend" for a named one. Any other "you've hit your" is not one.
fn reached_limit(signal: &str) -> Option<&str> {
    let lower = signal.to_ascii_lowercase();
    let start = ["you've hit your ", "you’ve hit your ", "you have hit your "]
        .into_iter()
        .find_map(|marker| lower.find(marker).map(|at| at + marker.len()))?;
    let line = lower[start..].lines().next()?;
    if line.starts_with("limit") {
        return Some("");
    }
    let end = line.find(" limit")?;
    Some(&signal[start..start + end])
}

/// The name a summary gives a reached limit: a plain name of at most three
/// words, such as "weekly", "Opus" or "Fable 5.2", or else "usage".
fn limit_name(reached: Option<&str>) -> &str {
    reached
        .filter(|name| {
            !name.is_empty()
                && name.split(' ').count() <= 3
                && name.split(' ').all(|word| {
                    !word.is_empty()
                        && (word.chars().all(|c| c.is_ascii_alphabetic()) || is_version(word))
                })
        })
        .unwrap_or("usage")
}

/// Whether `word` is a model version, such as "5" or "5.2".
fn is_version(word: &str) -> bool {
    word.split('.')
        .all(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()))
}

/// What to do about the limit `limit_name` named. Claude names a model's
/// limit by the model, capitalized - "Opus limit", "Fable limit" - and an
/// account's in lower case - "weekly limit", "5-hour limit". A model's limit
/// leaves the account's other models usable. Account-aware reset controls
/// are added by the interface when an available reset covers this limit.
fn limit_action(limit: &str) -> &'static str {
    let model = limit.split(' ').next().unwrap_or_default();
    if model.starts_with(|c: char| c.is_ascii_uppercase()) {
        "Choose another model or account, or wait for the reported reset."
    } else {
        "Wait for the reported reset or choose another account."
    }
}

/// Keep the actual provider message's reported clock/date/zone rather than
/// guessing a local reset or parsing an HTTP code as account authority.
/// Diagnostic tails are not part of the human reset label or its semantic key:
/// a parenthesis after the reset is kept only when it names a time zone, so
/// "(America/Vancouver) (error type rate_limit, HTTP 429, …)" keeps the zone.
fn reported_reset(message: &str) -> Option<String> {
    let lower = message.to_ascii_lowercase();
    // Codex says "… Try again at Nov 8th, 2026 11:11 AM."
    let (at, marker) = ["resets at ", "resets ", "reset at ", "try again at "]
        .into_iter()
        .filter_map(|marker| lower.find(marker).map(|at| (at, marker)))
        .min_by_key(|(at, _)| *at)?;
    let tail = &message[at + marker.len()..];
    let line = tail.lines().next()?.split(" · ").next()?;
    let mut parts = line.split(" (");
    let mut kept = parts.next()?.trim().to_owned();
    for part in parts {
        let Some(zone) = part.trim_end().trim_end_matches('.').strip_suffix(')') else {
            break;
        };
        if !is_zone(zone) {
            break;
        }
        kept.push_str(" (");
        kept.push_str(zone);
        kept.push(')');
    }
    let line = kept.trim_end_matches('.');
    if line.is_empty()
        || line.chars().count() > 128
        || !line.chars().any(|c| c.is_ascii_digit())
        || line.chars().any(char::is_control)
    {
        return None;
    }
    Some(line.split_whitespace().collect::<Vec<_>>().join(" "))
}

/// Whether a parenthesis after a reset names its time zone, such as "UTC",
/// "America/Vancouver", "Etc/GMT+8" or "Pacific Time", rather than adding
/// diagnostics such as "error type rate_limit, HTTP 429".
fn is_zone(text: &str) -> bool {
    let words: Vec<_> = text.split(' ').collect();
    !text.is_empty()
        && words.iter().all(|word| {
            !word.is_empty()
                && word
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '_' | '+' | '-' | ':'))
                && (words.len() == 1 || word.starts_with(|c: char| c.is_ascii_uppercase()))
        })
}

/// What became of a request Claude's retained conversation process could not
/// confirm or take, as the daemon reports it, and what the person can do.
/// Each text is one sentence of outcome and one of next step, so a notice can
/// show the step as its guidance instead of a generic one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeliveryOutcome {
    pub summary: &'static str,
    pub action: &'static str,
    /// Whether the request may still have gone through, rather than
    /// certainly not.
    pub unknown: bool,
}

macro_rules! delivery_unknown {
    (unknown) => {
        true
    };
    (refused) => {
        false
    };
}

macro_rules! delivery_outcomes {
    ($($(#[$doc:meta])* $kind:ident $name:ident: $summary:literal, $action:literal;)*) => {
        /// The texts of every [`DeliveryOutcome`].
        pub mod delivery {
            $($(#[$doc])* pub const $name: &str = concat!($summary, " ", $action);)*
            pub(super) const ALL: &[(&str, super::DeliveryOutcome)] = &[$((
                $name,
                super::DeliveryOutcome {
                    summary: $summary,
                    action: $action,
                    unknown: delivery_unknown!($kind),
                },
            )),*];
        }
    };
}

delivery_outcomes! {
    /// An input whose delivery was never confirmed, settled as not sent.
    refused UNCONFIRMED_INPUT: "Claude couldn't accept this in time, so it was not sent.", "Try again.";
    /// The caller stopped waiting while the outcome is still unknown.
    unknown UNACKNOWLEDGED_INPUT: "Claude hasn't confirmed this yet.",
        "It may still go through, so check the conversation before trying again.";
    /// An answer whose outcome is still unknown when its caller stops waiting.
    unknown UNACKNOWLEDGED_ANSWER: "Claude hasn't confirmed this answer yet.",
        "It may still go through, so check the conversation before answering again.";
    /// A stop whose outcome is still unknown when its caller stops waiting.
    unknown UNACKNOWLEDGED_STOP: "Claude hasn't confirmed the stop yet.",
        "It may still take effect, so check the conversation before trying again.";
    /// A new input while an earlier one is still unconfirmed.
    refused UNSETTLED_INPUT: "Claude hasn't confirmed the previous request yet, so this was not sent.",
        "Try again in a moment.";
    /// Every new input once an unconfirmed one cannot be settled.
    refused UNSETTLEABLE_INPUT: "Claude stopped confirming requests in this conversation, so this was not sent.",
        "Close and reopen the conversation to continue.";
    /// Pending inputs once the proxy stopped before their outcome was known.
    unknown STOPPED_INPUT: "Claude's conversation process stopped before confirming this, so it may or may not have gone through.",
        "Check the conversation before trying again.";
    /// A new input while the proxy reattaches to its helper.
    refused RECONNECTING_INPUT: "Claude is still reconnecting to this conversation, so this was not sent.",
        "Try again in a moment.";
    /// A new input beyond the requests already waiting.
    refused CROWDED_INPUT: "Too many requests are waiting for Claude, so this was not sent.",
        "Try again in a moment.";
    /// A new input while completed ones are still being retired.
    refused RETIRING_INPUT: "Claude is still finishing earlier requests, so this was not sent.",
        "Try again in a moment.";
    /// An input too large for one protocol frame.
    refused OVERSIZED_INPUT: "This is too large to send to Claude, so it was not sent.",
        "Shorten it or split it into smaller messages.";
    /// An input to a helper whose conversation is closing.
    refused CLOSING_INPUT: "Claude's conversation is closing, so this was not sent.",
        "Reopen the conversation to continue.";
    /// An input to a helper handing its conversation to an updated daemon.
    refused HANDOVER_INPUT: "Claude's conversation is moving to the updated cagents service, so this was not sent.",
        "Try again in a moment.";
    /// An input after the helper lost track of earlier outcomes.
    refused DISCONTINUOUS_INPUT: "Claude can't confirm earlier work in this conversation, so this was not sent.",
        "Start a new conversation to continue.";
    /// An input whose frame arrived altered.
    refused DAMAGED_INPUT: "This request was damaged on its way to Claude, so it was not sent.",
        "Try again.";
    /// An intact input that the helper's protocol does not describe.
    refused UNSUPPORTED_INPUT: "Claude's conversation process doesn't support this request, so it was not sent.",
        "Close and reopen the conversation to continue.";
    /// Any input once the proxy has stopped.
    refused UNAVAILABLE_INPUT: "Claude's conversation process is no longer reachable, so this was not sent.",
        "Close and reopen the conversation to continue.";
}

impl DeliveryOutcome {
    /// The outcome whose exact text `message` is.
    pub fn parse(message: &str) -> Option<Self> {
        let message = message.trim();
        delivery::ALL
            .iter()
            .find(|(text, _)| *text == message)
            .map(|(_, outcome)| *outcome)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delivery_outcomes_split_into_their_outcome_and_next_step() {
        assert_eq!(
            DeliveryOutcome::parse(delivery::UNACKNOWLEDGED_INPUT),
            Some(DeliveryOutcome {
                summary: "Claude hasn't confirmed this yet.",
                action: "It may still go through, so check the conversation before trying again.",
                unknown: true,
            })
        );
        for (index, (text, outcome)) in delivery::ALL.iter().enumerate() {
            assert_eq!(DeliveryOutcome::parse(text), Some(*outcome), "{text}");
            assert!(outcome.summary.ends_with('.') && outcome.action.ends_with('.'));
            assert!(
                delivery::ALL[..index]
                    .iter()
                    .all(|(other, _)| other != text),
                "{text} is listed twice"
            );
        }
        // A provider failure's keywords never claim an outcome's text.
        for (text, _) in delivery::ALL {
            for provider in [Provider::Claude, Provider::Codex, Provider::Omp] {
                assert!(Failure::parse(provider, text).is_none(), "{text}");
            }
        }
        // Only the exact text is recognised, never a quotation of it.
        assert_eq!(
            DeliveryOutcome::parse(&format!("Claude said: {}", delivery::UNCONFIRMED_INPUT)),
            None
        );
    }

    #[test]
    fn explicit_provenance_requires_full_provider_tuple_and_preserves_original_item_identity() {
        let raw = "You've hit your limit · resets 1:30pm\nfull raw diagnostics";
        let mut item = super::super::Item {
            id: "stable-error:0".into(),
            role: super::super::Role::Assistant,
            content: super::super::ItemContent::Text {
                text: raw.into(),
                thinking: false,
                phase: super::super::Phase::Final,
            },
            at: Some(10),
            started_at: Some(5),
        };
        assert!(provider_error_text(Provider::Claude, &item).is_none());
        assert!(mark_provider_error(Provider::Claude, &mut item));
        assert_eq!(provider_error_text(Provider::Claude, &item), Some(raw));
        assert_eq!(
            provider_error_carrier(Provider::Claude, &item),
            Some("stable-error")
        );
        assert!(provider_error_carrier(Provider::Codex, &item).is_none());
        assert!(provider_error_carrier(Provider::Omp, &item).is_none());
        for id in [
            "unsuffixed",
            "carrier:",
            ":0",
            "carrier:not-index",
            "carrier:−1",
        ] {
            let mut malformed = item.clone();
            malformed.id = id.into();
            assert!(provider_error_carrier(Provider::Claude, &malformed).is_none());
        }
        assert_eq!(item.id, "stable-error:0");
        assert_eq!((item.at, item.started_at), (Some(10), Some(5)));
        assert!(provider_error_text(Provider::Codex, &item).is_none());
        let mut peer = item.clone();
        if let super::super::ItemContent::Delivery { sender, .. } = &mut peer.content {
            sender.clear();
        }
        assert!(provider_error_text(Provider::Claude, &peer).is_none());
        assert!(provider_error_carrier(Provider::Claude, &peer).is_none());
        let mut user = item.clone();
        user.role = super::super::Role::User;
        assert!(provider_error_text(Provider::Claude, &user).is_none());
    }

    #[test]
    fn equivalent_limits_share_reset_identity_without_losing_raw_diagnostics() {
        let a = "You've hit your limit · resets 1:30pm (America/Los_Angeles)\nrequest-id: first";
        let b = "API Error: rate_limit_error; resets at 1:30pm (America/Los_Angeles)\nrequest-id: second";
        let first = Failure::parse(Provider::Claude, a).unwrap();
        let repeated = Failure::parse(Provider::Claude, b).unwrap();
        assert_eq!(first.kind, FailureKind::AccountLimit);
        assert_eq!(first.key, repeated.key);
        let stream = serde_json::json!({"type":"rate_limit_error", "message":"You've hit your limit · resets 1:30pm (America/Los_Angeles)", "request_id":"another"}).to_string();
        assert_eq!(
            first.key,
            Failure::parse(Provider::Claude, &stream).unwrap().key
        );
        assert_eq!(
            first.summary,
            "Claude usage limit reached · resets 1:30pm (America/Los_Angeles)"
        );
        assert_eq!(first.details, a);
        assert_eq!(repeated.details, b);
        assert_ne!(
            first.key,
            Failure::parse(
                Provider::Claude,
                "You've hit your limit · resets 2:30pm (America/Los_Angeles)"
            )
            .unwrap()
            .key
        );
    }

    #[test]
    fn a_named_limit_and_a_subagents_report_of_it_are_one_limit_with_its_reset() {
        // As Claude Code 2.1.296 reported one weekly limit: the parent's own
        // API error, and the failed Agent call of a subagent that hit it.
        let parent = "You've hit your weekly limit · resets Nov 8, 8am (America/Vancouver)";
        let agent = "Agent terminated early due to an API error: You've hit your weekly limit · resets Nov 8, 8am (America/Vancouver) (error type rate_limit, HTTP 429, request id req_011, model sent to the API: claude-opus-5-5)";
        let parent = Failure::parse(Provider::Claude, parent).unwrap();
        let agent = Failure::parse(Provider::Claude, agent).unwrap();
        for failure in [&parent, &agent] {
            assert_eq!(failure.kind, FailureKind::AccountLimit);
            assert_eq!(
                failure.reset.as_deref(),
                Some("Nov 8, 8am (America/Vancouver)")
            );
            assert_eq!(
                failure.summary,
                "Claude weekly limit reached · resets Nov 8, 8am (America/Vancouver)"
            );
        }
        assert_eq!(parent.key, agent.key);
        for (raw, summary) in [
            (
                "You’ve hit your Opus limit · resets 3pm",
                "Claude Opus limit reached · resets 3pm",
            ),
            (
                "You have hit your monthly spend limit · resets Dec 1",
                "Claude monthly spend limit reached · resets Dec 1",
            ),
            (
                "You've hit your Fable limit · resets Nov 15, 8am (America/Vancouver)",
                "Claude Fable limit reached · resets Nov 15, 8am (America/Vancouver)",
            ),
            (
                "Agent terminated early due to an API error: You've hit your Fable 5.2 limit · resets Nov 15, 8am (America/Vancouver) (error type rate_limit, HTTP 429, request id req_012, model sent to the API: claude-fable-5-2)",
                "Claude Fable 5.2 limit reached · resets Nov 15, 8am (America/Vancouver)",
            ),
            (
                "You've hit your channel's monthly spend limit.",
                "Claude usage limit reached",
            ),
            (
                "Agent terminated early due to an API error: Request rejected (error type rate_limit, HTTP 429)",
                "Claude usage limit reached",
            ),
        ] {
            let failure = Failure::parse(Provider::Claude, raw).unwrap();
            assert_eq!(failure.kind, FailureKind::AccountLimit, "{raw}");
            assert_eq!(failure.summary, summary, "{raw}");
        }
        // Only a time zone's parenthesis belongs to the reset.
        for (raw, reset) in [
            (
                "You've hit your limit · resets 9:30pm (error type rate_limit)",
                "9:30pm",
            ),
            (
                "You've hit your limit · resets 9:30pm (UTC) (HTTP 429)",
                "9:30pm (UTC)",
            ),
            (
                "You've hit your limit · resets 9:30pm (Etc/GMT+8)",
                "9:30pm (Etc/GMT+8)",
            ),
            (
                "You've hit your limit · resets 9:30pm (Pacific Time)",
                "9:30pm (Pacific Time)",
            ),
            (
                "You've hit your limit · resets Nov 8 (America/Vancouver).",
                "Nov 8 (America/Vancouver)",
            ),
            ("You've hit your limit · resets 9:30pm (HTTP 429)", "9:30pm"),
        ] {
            let failure = Failure::parse(Provider::Claude, raw).unwrap();
            assert_eq!(failure.reset.as_deref(), Some(reset), "{raw}");
        }

        // "You've hit your" says nothing of a limit unless it names one.
        assert!(
            Failure::parse(Provider::Claude, "You've hit your stride")
                .is_none_or(|failure| failure.kind != FailureKind::AccountLimit)
        );
        // Two limits that reset together are still two limits.
        let weekly = Failure::parse(
            Provider::Claude,
            "You've hit your weekly limit · resets 3pm",
        );
        let opus = Failure::parse(Provider::Claude, "You've hit your Opus limit · resets 3pm");
        assert_ne!(weekly.unwrap().key, opus.unwrap().key);
    }

    /// Generic guidance names the remaining choices. Available reset actions
    /// depend on the account and are added by the interface.
    #[test]
    fn a_reached_limit_suggests_what_it_leaves_open() {
        let action = |raw: &str| Failure::parse(Provider::Claude, raw).unwrap().action;
        for raw in [
            "You've hit your Fable limit · resets Nov 15, 8am",
            "You've hit your Fable 5.2 limit · resets Nov 15, 8am",
        ] {
            assert!(!action(raw).contains("/limit-reset"), "{raw}");
            assert!(action(raw).contains("another model"), "{raw}");
        }
        let opus = action("You’ve hit your Opus limit · resets 3pm");
        assert!(opus.contains("another model"));
        assert!(!opus.contains("/limit-reset"));
        for raw in [
            "You've hit your weekly limit · resets Nov 8, 8am (America/Vancouver)",
            "You've hit your limit · resets 2:30pm",
            "You've hit your 5-hour limit · resets 2:30pm",
        ] {
            assert_eq!(
                action(raw),
                "Wait for the reported reset or choose another account.",
                "{raw}"
            );
        }
    }

    #[test]
    fn distinct_or_unproved_incidents_and_unrelated_http_failures_stay_distinct() {
        let a = Failure::parse(Provider::Claude, "quota exceeded: account A").unwrap();
        let b = Failure::parse(Provider::Claude, "quota exceeded: account B").unwrap();
        assert_ne!(a.key, b.key);
        assert!(matches!(a.key, Cow::Borrowed(_)));
        assert!(Failure::parse(Provider::Claude, "tool endpoint returned HTTP 401").is_none());
        assert!(Failure::parse(Provider::Claude, "tool returned HTTP 429").is_none());
        assert!(Failure::parse(Provider::Claude, "Something else failed").is_none());
        assert!(
            Failure::parse(
                Provider::Claude,
                "You've hit your limit; resets whenever available"
            )
            .unwrap()
            .reset
            .is_none()
        );
    }

    #[test]
    fn adjacent_failures_keep_conservative_classification_and_bounded_display() {
        for (raw, kind) in [
            (
                "authentication_error: invalid credentials",
                FailureKind::Authentication,
            ),
            ("overloaded_error", FailureKind::Capacity),
            ("connection reset by peer", FailureKind::Connection),
            ("request was interrupted", FailureKind::Interrupted),
        ] {
            let failure = Failure::parse(Provider::Claude, raw).unwrap();
            assert_eq!(failure.kind, kind);
            assert_eq!(failure.details, raw);
            assert_eq!(failure.key, raw);
            assert!(failure.summary.len() < 256);
        }
        let raw = format!(
            "You've hit your limit · resets {}\n{}",
            "1".repeat(500),
            "diagnostic".repeat(100_000)
        );
        let failure = Failure::parse(Provider::Claude, &raw).unwrap();
        assert!(failure.reset.is_none());
        assert_eq!(failure.details.len(), raw.len());
        assert!(failure.summary.len() < 256);
    }

    #[test]
    fn structured_request_diagnostics_cannot_inject_failure_kind_or_reset() {
        for diagnostic in [
            "quota exceeded · resets 9:30pm",
            "authentication_error: please sign in again",
            "overloaded_error",
            "request was interrupted",
        ] {
            let raw = serde_json::json!({
                "type": "connection_error",
                "message": "Connection reset by peer",
                "request": {"prompt": diagnostic},
                "model": diagnostic,
                "error": diagnostic,
                "diagnostics": {"message": diagnostic, "type": "rate_limit_error"}
            })
            .to_string();
            let failure = Failure::parse(Provider::Claude, &raw).unwrap();
            assert_eq!(failure.kind, FailureKind::Connection);
            assert!(failure.reset.is_none());
            assert_eq!(failure.key, raw);
            assert_eq!(failure.details, raw);
            assert_eq!(failure.details.as_ptr(), raw.as_ptr());

            let unknown = serde_json::json!({
                "type": "invalid_request_error",
                "message": "The requested model does not exist",
                "request": {"prompt": diagnostic},
                "model": diagnostic
            })
            .to_string();
            assert!(Failure::parse(Provider::Codex, &unknown).is_none());
        }
        for raw in [
            r#"{"type":"connection_error","message":"Connection reset by peer","error":{"type":"rate_limit_error","message":"quota exceeded · resets 9:30pm"}}"#,
            r#"{"type":"api_error","message":"Connection reset by peer","error":"quoted quota exceeded"}"#,
            r#"{"message":"Connection reset by peer","error":{"message":"quota exceeded"}}"#,
        ] {
            let failure = Failure::parse(Provider::Claude, raw).unwrap();
            assert_eq!(failure.kind, FailureKind::Connection);
            assert!(failure.reset.is_none());
            assert_eq!(failure.details, raw);
        }
        // An explicit type remains authoritative even if its actual message
        // quotes a different failure; this is not a provider account limit.
        let raw = r#"{"type":"connection_error","message":"Remote diagnostic quoted quota exceeded · resets 9:30pm"}"#;
        let failure = Failure::parse(Provider::Claude, raw).unwrap();
        assert_eq!(failure.kind, FailureKind::Connection);
        assert!(failure.reset.is_none());
        assert_eq!(failure.details, raw);
        let raw = r#"{"type":"rate_limit_error","message":"Quota exceeded","request":{"prompt":"resets 9:30pm"}}"#;
        let failure = Failure::parse(Provider::Claude, raw).unwrap();
        assert_eq!(failure.kind, FailureKind::AccountLimit);
        assert!(failure.reset.is_none());
        assert_eq!(failure.key, raw);
    }

    #[test]
    fn structured_provider_error_envelopes_keep_genuine_actions_and_original_details() {
        for (error, kind, reset) in [
            (
                serde_json::json!({"type":"rate_limit_error", "message":"You've hit your limit · resets 1:30pm (America/Los_Angeles)"}),
                FailureKind::AccountLimit,
                Some("1:30pm (America/Los_Angeles)"),
            ),
            (
                serde_json::json!({"type":"authentication_error", "message":"Credentials were rejected"}),
                FailureKind::Authentication,
                None,
            ),
            (
                serde_json::json!({"type":"overloaded_error", "message":"Please try later"}),
                FailureKind::Capacity,
                None,
            ),
            (
                serde_json::json!({"type":"connection_error", "message":"Socket closed"}),
                FailureKind::Connection,
                None,
            ),
            (
                serde_json::json!({"type":"interrupted_error", "message":"Stopped"}),
                FailureKind::Interrupted,
                None,
            ),
            (
                serde_json::json!({"type":"api_error", "message":"Provider is at capacity"}),
                FailureKind::Capacity,
                None,
            ),
            (
                serde_json::json!({"type":"invalid_request_error", "code":"insufficient_quota", "message":"Quota exceeded · resets at 2:30pm"}),
                FailureKind::AccountLimit,
                Some("2:30pm"),
            ),
        ] {
            let direct = error.to_string();
            let nested = serde_json::json!({
                "type":"error", "message":"Outer diagnostic quoted authentication_error", "error":error,
                "request":{"prompt":"authentication_error · resets 9:30pm"}
            })
            .to_string();
            for raw in [direct, nested.clone(), format!("API Error: 429 {nested}")] {
                let failure = Failure::parse(Provider::Claude, &raw).unwrap();
                assert_eq!(failure.kind, kind);
                assert_eq!(failure.reset.as_deref(), reset);
                assert_eq!(failure.details, raw);
                assert_eq!(failure.details.as_ptr(), raw.as_ptr());
                assert!(!failure.action.is_empty());
                if let Some(reset) = reset {
                    let plain = format!("You've hit your limit · resets {reset}");
                    assert_eq!(
                        failure.key,
                        Failure::parse(Provider::Claude, &plain).unwrap().key
                    );
                } else {
                    assert_eq!(failure.key, raw);
                }
            }
        }
        // Some adapters extract a message rather than preserving the object;
        // both that text and an error string envelope retain phrase semantics.
        for raw in [
            r#"{"message":"Access token could not be refreshed","request":{"prompt":"quota exceeded"}}"#,
            r#"{"error":"Access token could not be refreshed","request":{"prompt":"quota exceeded"}}"#,
        ] {
            assert_eq!(
                Failure::parse(Provider::Codex, raw).unwrap().kind,
                FailureKind::Authentication
            );
        }
        let type_only = r#"{"type":"rate_limit_error","request":{"prompt":"resets 9:30pm"}}"#;
        let failure = Failure::parse(Provider::Claude, type_only).unwrap();
        assert_eq!(failure.kind, FailureKind::AccountLimit);
        assert!(failure.reset.is_none());
    }

    #[test]
    fn malformed_or_unproved_structured_diagnostics_do_not_gain_provider_authority() {
        for raw in [
            r#"{"request":{"prompt":"quota exceeded"}}"#,
            r#"{"model":"authentication_error","diagnostics":{"message":"quota exceeded"}}"#,
            r#"{"type":"error","error":{"request":{"prompt":"quota exceeded"}}}"#,
            r#"[{"prompt":"quota exceeded"}]"#,
            r#"{"type":"rate_limit_error","message":"quota exceeded""#,
            r#"API Error: 429 {"request":{"prompt":"quota exceeded"}}"#,
            r#"API Error: 401 {"error":{"type":"unknown","message":"Tool endpoint returned HTTP 401"}}"#,
        ] {
            assert!(Failure::parse(Provider::Claude, raw).is_none(), "{raw}");
        }
        // Preserve the existing bounded work policy for oversized structures;
        // an incomplete prefix is unknown, not prose authority for a reset.
        let raw = serde_json::json!({
            "type":"connection_error", "message":"Connection reset by peer",
            "request":{"prompt":format!("quota exceeded {}", "x".repeat(20_000))}
        })
        .to_string();
        assert!(Failure::parse(Provider::Claude, &raw).is_none());
    }

    #[test]
    fn a_listed_failure_reads_as_one_line_never_a_raw_payload() {
        let limit = summary(Provider::Codex, "Usage limit reached for this plan");
        assert!(limit.starts_with("Codex usage limit reached"), "{limit}");
        // A limit keeps the time it can be used again.
        let own = "You\u{2019}ve hit your usage limit. Try again at Nov 8th, 2026 11:11 AM.";
        assert_eq!(
            summary(Provider::Codex, own),
            "Codex usage limit reached · resets Nov 8th, 2026 11:11 AM"
        );
        assert_eq!(
            summary(
                Provider::Claude,
                "[ede_diagnostic] result_type=user stop_reason=tool_use"
            ),
            "Request failed"
        );
        assert_eq!(
            summary(Provider::Claude, "\nThe process exited\nwith details"),
            "The process exited"
        );
    }

    #[test]
    fn runtime_exit_reports_explain_the_stop_in_plain_words() {
        let oom = format!(
            "{PROVIDER_OUT_OF_MEMORY} systemd-oomd ended it and the commands it was running. Provider exited with signal: 9 (SIGKILL)"
        );
        let failure = Failure::parse(Provider::Claude, &oom).unwrap();
        assert_eq!(failure.kind, FailureKind::OutOfMemory);
        assert!(
            failure.summary.contains("ran out of memory"),
            "{}",
            failure.summary
        );
        assert!(failure.action.contains("resume"), "{}", failure.action);
        assert_eq!(
            failure.summary,
            "Claude Code stopped because the system ran out of memory"
        );
        assert_eq!(failure.details, oom);

        let command = format!(
            "{COMMAND_OUT_OF_MEMORY} systemd-oomd ended it (12 processes); the agent is still running."
        );
        let failure = Failure::parse(Provider::Codex, &command).unwrap();
        assert_eq!(failure.kind, FailureKind::CommandOutOfMemory);
        // The stopped command's step reads "failed"; the card agrees.
        assert_eq!(
            failure.summary,
            "A command failed because the system ran out of memory"
        );
        assert!(
            failure.action.contains("still running"),
            "{}",
            failure.action
        );

        for (raw, summary) in [
            (
                "Provider exited with signal: 9 (SIGKILL)",
                "Claude Code was force-stopped",
            ),
            (
                "Provider exited with signal: 6 (SIGABRT) (core dumped)",
                "Claude Code crashed",
            ),
            (
                "Provider exited with signal: 15 (SIGTERM)",
                "Claude Code was stopped",
            ),
            (
                "Provider exited with signal: 40",
                "Claude Code stopped unexpectedly",
            ),
            (
                "Provider exited with exit status: 3. Last ignored output: boom",
                "Claude Code quit unexpectedly with code 3",
            ),
        ] {
            let failure = Failure::parse(Provider::Claude, raw).unwrap();
            assert_eq!(failure.kind, FailureKind::Exited, "{raw}");
            assert_eq!(failure.summary, summary, "{raw}");
            assert!(!failure.action.contains("Resume"), "{raw}");
            assert_eq!(failure.details, raw);
        }
    }

    #[test]
    fn an_exit_whose_output_names_its_cause_keeps_that_cause() {
        let limit = "Provider exited with exit status: 1. Last ignored output: You've hit your limit · resets 3pm";
        assert_eq!(
            Failure::parse(Provider::Claude, limit).unwrap().kind,
            FailureKind::AccountLimit
        );
        let auth = "Provider exited with exit status: 1. Last ignored output: Invalid API key · Please run /login";
        assert_eq!(
            Failure::parse(Provider::Claude, auth).unwrap().kind,
            FailureKind::Authentication
        );
    }

    #[test]
    fn provider_prose_mentioning_an_exit_is_not_a_runtime_report() {
        // Only a report that leads with the runtime's own wording is one;
        // the same words quoted later by a provider keep their own class.
        let quoted = "Tool endpoint said: Provider exited with signal: 9 (SIGKILL)";
        assert!(
            Failure::parse(Provider::Claude, quoted)
                .is_none_or(|failure| failure.kind != FailureKind::Exited)
        );
    }
}
