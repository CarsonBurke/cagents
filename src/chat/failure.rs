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
        let kind = if let Some(kind) = structured_kind {
            kind
        } else if contains(&[
            "you've hit your limit",
            "you have hit your limit",
            "you've hit your session limit",
            "you have hit your session limit",
            "you’ve hit your session limit",
            "you've hit your usage limit",
            "you have hit your usage limit",
            "usage limit is reached",
            "usage limit reached",
            "session limit reached",
            "rate_limit_error",
            "claude: rate_limit",
            "insufficient_quota",
            "quota exceeded",
            "exceeded your current quota",
        ]) {
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
        } else {
            return None;
        };
        let name = match provider {
            Provider::Claude => "Claude",
            Provider::Codex => "Codex",
            Provider::Omp => "Oh My Pi",
        };
        let reset = (kind == FailureKind::AccountLimit)
            .then(|| reported_reset(&signal))
            .flatten();
        let (mut summary, action) = match kind {
            FailureKind::AccountLimit => (
                format!("{name} usage limit reached"),
                "Wait for the reported reset or choose another account.",
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
        };
        if let Some(reset) = &reset {
            summary.push_str(" · resets ");
            summary.push_str(reset);
        }
        let key = match &reset {
            Some(reset) => Cow::Owned(format!(
                "{provider:?}:account-limit:{}",
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

/// Whether a failure contains information beyond the compact account-limit
/// notice. This is presentation-only: callers retain the complete original.
/// Unknown, oversized, partial, or annotated diagnostics always remain distinct.
pub fn has_distinct_diagnostic(provider: Provider, message: &str) -> bool {
    // Inspect the complete bounded source, never a prefix that could hide a
    // request ID or meaningful tail. Check bytes before allocating/parsing.
    if message.len() > 8192
        || !Failure::parse(provider, message)
            .is_some_and(|failure| failure.kind == FailureKind::AccountLimit)
    {
        return true;
    }
    let trimmed = message.trim();
    let json = if trimmed.starts_with('{') {
        Some(trimmed)
    } else if let Some(tail) = trimmed.strip_prefix("API Error:") {
        let Some(at) = tail.find('{') else {
            return true;
        };
        let (status, body) = tail.split_at(at);
        if status.trim() != "429" {
            return true;
        }
        Some(body)
    } else {
        None
    };
    match json {
        Some(json) => {
            !unique_limit_envelope_fields(json)
                || !serde_json::from_str::<serde_json::Value>(json)
                    .ok()
                    .is_some_and(|value| redundant_limit_envelope(&value, true).is_some())
        }
        None => !redundant_limit_sentence(trimmed),
    }
}

// Validation-only shape: deriving Deserialize rejects duplicate known fields,
// including escaped duplicates, before Value could silently discard originals.
fn unique_limit_envelope_fields(json: &str) -> bool {
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Envelope {
        #[serde(rename = "type")]
        _kind: Option<String>,
        #[serde(rename = "code")]
        _code: Option<String>,
        #[serde(rename = "message")]
        _message: Option<String>,
        #[serde(rename = "error")]
        _error: Option<Error>,
    }
    #[allow(dead_code)] // Both variants exist only to validate the original shape.
    #[derive(serde::Deserialize)]
    #[serde(untagged)]
    enum Error {
        Text(String),
        Envelope(Box<Envelope>),
    }
    serde_json::from_str::<Envelope>(json).is_ok()
}

fn redundant_limit_envelope(value: &serde_json::Value, nested: bool) -> Option<&str> {
    let object = value.as_object()?;
    if object
        .keys()
        .any(|key| !matches!(key.as_str(), "type" | "code" | "message" | "error"))
    {
        return None;
    }
    for key in ["type", "code"] {
        if let Some(value) = object.get(key) {
            let kind = value.as_str()?;
            if !(key == "type" && kind == "error")
                && structured_kind(kind) != Some(FailureKind::AccountLimit)
            {
                return None;
            }
        }
    }
    let message = match object.get("message") {
        None => None,
        Some(value) => Some(
            value
                .as_str()
                .filter(|text| redundant_limit_sentence(text))?,
        ),
    };
    let error = match object.get("error") {
        None => None,
        Some(serde_json::Value::String(text)) if redundant_limit_sentence(text) => {
            Some(text.as_str())
        }
        Some(value) if nested => Some(redundant_limit_envelope(value, false)?),
        Some(_) => return None,
    };
    match (message, error) {
        (Some(message), Some(error)) if message.trim() == error.trim() => Some(message),
        (Some(message), None) => Some(message),
        (None, Some(error)) => Some(error),
        _ => None,
    }
}

fn redundant_limit_sentence(message: &str) -> bool {
    let trimmed = message.trim();
    if trimmed.contains(['\n', '\r']) {
        return false;
    }
    let lower = trimmed.to_ascii_lowercase();
    let Some(tail) = [
        "you've hit your session limit",
        "you have hit your session limit",
        "you’ve hit your session limit",
        "you've hit your usage limit",
        "you have hit your usage limit",
        "you've hit your limit",
        "you have hit your limit",
        "usage limit is reached",
        "usage limit reached",
        "session limit reached",
        "quota exceeded",
    ]
    .into_iter()
    .find_map(|prefix| lower.strip_prefix(prefix)) else {
        return false;
    };
    let tail = tail.trim();
    if tail.is_empty() || tail == "." {
        return true;
    }
    let reset = [
        "· resets at ",
        "· resets ",
        "resets at ",
        "resets ",
        "; resets at ",
        "; resets ",
    ]
    .into_iter()
    .find_map(|prefix| tail.strip_prefix(prefix));
    reset.is_some_and(strict_reset_clock)
}

fn strict_reset_clock(reset: &str) -> bool {
    let (clock, zone) = match reset.split_once(" (") {
        Some((clock, tail)) => {
            let Some(zone) = tail.strip_suffix(')') else {
                return false;
            };
            (clock, Some(zone))
        }
        None => (reset, None),
    };
    if let Some(zone) = zone {
        if zone.is_empty()
            || zone.len() > 80
            || !zone
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"/_+-".contains(&byte))
            || !(zone.contains('/') || matches!(zone, "utc" | "gmt"))
            || zone.split('/').any(str::is_empty)
        {
            return false;
        }
    }
    let (clock, twelve_hour) = if let Some(clock) = clock
        .strip_suffix("am")
        .or_else(|| clock.strip_suffix("pm"))
    {
        (clock, true)
    } else {
        (clock, false)
    };
    let (hour, minute) = match clock.split_once(':') {
        Some((hour, minute)) => (hour, Some(minute)),
        None if twelve_hour => (clock, None),
        None => return false,
    };
    if hour.is_empty() || hour.len() > 2 || !hour.bytes().all(|byte| byte.is_ascii_digit()) {
        return false;
    }
    let Ok(hour) = hour.parse::<u8>() else {
        return false;
    };
    if (twelve_hour && !(1..=12).contains(&hour)) || (!twelve_hour && hour > 23) {
        return false;
    }
    minute.is_none_or(|minute| {
        minute.len() == 2
            && minute.bytes().all(|byte| byte.is_ascii_digit())
            && minute.parse::<u8>().is_ok_and(|minute| minute < 60)
    })
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

/// Keep the actual provider message's reported clock/date/zone rather than
/// guessing a local reset or parsing an HTTP code as account authority.
/// Diagnostic tails are not part of the human reset label or its semantic key.
fn reported_reset(message: &str) -> Option<String> {
    let lower = message.to_ascii_lowercase();
    let (at, marker) = ["resets at ", "resets ", "reset at "]
        .into_iter()
        .filter_map(|marker| lower.find(marker).map(|at| (at, marker)))
        .min_by_key(|(at, _)| *at)?;
    let tail = &message[at + marker.len()..];
    let line = tail.lines().next()?.split(" · ").next()?.trim();
    if line.is_empty()
        || line.chars().count() > 128
        || !line.chars().any(|c| c.is_ascii_digit())
        || line.chars().any(char::is_control)
    {
        return None;
    }
    Some(line.split_whitespace().collect::<Vec<_>>().join(" "))
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn session_limit_notices_keep_reported_reset_and_semantic_provider_identity() {
        let raw = "You've hit your session limit · resets 3:40pm (America/Vancouver)";
        let first = Failure::parse(Provider::Claude, raw).unwrap();
        assert_eq!(first.kind, FailureKind::AccountLimit);
        assert_eq!(first.reset.as_deref(), Some("3:40pm (America/Vancouver)"));
        assert_eq!(
            first.summary,
            "Claude usage limit reached · resets 3:40pm (America/Vancouver)"
        );
        assert_eq!(first.details, raw);
        assert_eq!(first.details.as_ptr(), raw.as_ptr());
        for wording in [
            "You've hit your limit",
            "You have hit your session limit",
            "You’ve hit your session limit",
            "Session limit reached",
            "You've hit your usage limit",
            "You have hit your usage limit",
            "Usage limit reached",
        ] {
            let equivalent = format!("{wording} · resets at 3:40pm (America/Vancouver)");
            let parsed = Failure::parse(Provider::Claude, &equivalent).unwrap();
            assert_eq!(parsed.kind, FailureKind::AccountLimit);
            assert_eq!(parsed.key, first.key, "{wording}");
            assert_eq!(parsed.details, equivalent);
        }
        for different in [
            "You've hit your session limit · resets 4:40pm (America/Vancouver)",
            "You've hit your session limit · resets 3:40pm (America/Toronto)",
        ] {
            assert_ne!(
                first.key,
                Failure::parse(Provider::Claude, different).unwrap().key
            );
        }
        assert_ne!(first.key, Failure::parse(Provider::Codex, raw).unwrap().key);
        assert_ne!(first.key, Failure::parse(Provider::Omp, raw).unwrap().key);
        let no_reset = "You've hit your session limit";
        let parsed = Failure::parse(Provider::Claude, no_reset).unwrap();
        assert_eq!(parsed.kind, FailureKind::AccountLimit);
        assert!(parsed.reset.is_none());
        assert_eq!(parsed.key, no_reset);
    }

    #[test]
    fn only_complete_redundant_limit_notices_hide_diagnostic_details() {
        let notice = "You've hit your session limit · resets 3:40pm (America/Vancouver)";
        for message in [
            notice,
            "You've hit your limit",
            "Usage limit reached · resets 15:40 (UTC)",
        ] {
            assert!(
                !has_distinct_diagnostic(Provider::Claude, message),
                "{message}"
            );
            for raw in [
                serde_json::json!({"type":"rate_limit_error", "message":message}).to_string(),
                serde_json::json!({"error":{"type":"rate_limit_error", "message":message}}).to_string(),
                serde_json::json!({"type":"error", "error":{"type":"rate_limit_error", "message":message}}).to_string(),
                serde_json::json!({"error":message}).to_string(),
            ] {
                assert!(!has_distinct_diagnostic(Provider::Claude, &raw), "{raw}");
                assert!(!has_distinct_diagnostic(Provider::Claude, &format!("API Error: 429 {raw}")));
            }
        }
        for raw in [
            format!("{notice}\nrequest-id: actual-first"),
            format!("{notice} · request-id: actual-second"),
            format!("{notice} Please contact support with code 1234"),
            format!("Review quoted notice: {notice}"),
            serde_json::json!({"type":"rate_limit_error", "message":notice, "request_id":"first"}).to_string(),
            serde_json::json!({"type":"rate_limit_error", "message":notice, "unknown":null}).to_string(),
            serde_json::json!({"type":"error", "error":{"type":"rate_limit_error", "message":notice, "exchange":{"status":429}}}).to_string(),
            serde_json::json!({"type":"connection_error", "message":notice}).to_string(),
            serde_json::json!({"type":"invalid_request_error", "code":"insufficient_quota", "message":notice}).to_string(),
            serde_json::json!({"type":"rate_limit_error", "message":notice, "error":"Another account limit"}).to_string(),
            serde_json::json!({"type":"rate_limit_error", "message":notice, "code":null}).to_string(),
            format!("API Error: 403 {}", serde_json::json!({"type":"rate_limit_error", "message":notice})),
            format!("API Error: 429 {{\"type\":\"rate_limit_error\",\"message\":{notice:?}"),
        ] {
            assert!(has_distinct_diagnostic(Provider::Claude, &raw), "{raw}");
        }
        // Value alone would discard the original first message; keep it visible.
        let duplicate = format!(
            "{{\"type\":\"rate_limit_error\",\"message\":\"request id 123\",\"message\":{notice:?}}}"
        );
        assert!(has_distinct_diagnostic(Provider::Claude, &duplicate));
        let nested_duplicate = format!(
            "{{\"error\":{{\"type\":\"rate_limit_error\",\"message\":\"request id 123\",\"message\":{notice:?}}}}}"
        );
        assert!(has_distinct_diagnostic(Provider::Claude, &nested_duplicate));
        let large = format!("{notice}{}\nrequest-id: retained", " ".repeat(8192));
        assert!(has_distinct_diagnostic(Provider::Claude, &large));
        for reset in [
            "25:40",
            "0pm",
            "3:99pm",
            "3:40pm (America/Vancouver) extra",
            "3:40pm (Tomorrow)",
            "3:40pm (America//Vancouver)",
        ] {
            assert!(
                has_distinct_diagnostic(
                    Provider::Claude,
                    &format!("You've hit your session limit · resets {reset}")
                ),
                "{reset}"
            );
        }
        for raw in [
            "Connection reset by peer",
            "Quota exceeded: account B",
            "Request was interrupted",
            "Unknown error",
        ] {
            assert!(has_distinct_diagnostic(Provider::Claude, raw));
        }
    }

    #[test]
    fn session_limit_quotations_do_not_override_structured_error_authority() {
        let quoted = "You've hit your session limit · resets 3:40pm (America/Vancouver)";
        for raw in [
            serde_json::json!({"request":{"prompt":quoted}}).to_string(),
            serde_json::json!({"type":"invalid_request_error", "message":"Unknown model", "request":{"prompt":quoted}}).to_string(),
            serde_json::json!({"type":"error", "error":{"request":{"prompt":quoted}}}).to_string(),
        ] {
            assert!(Failure::parse(Provider::Claude, &raw).is_none(), "{raw}");
        }
        let connection = serde_json::json!({"type":"connection_error", "message":quoted, "request_id":"distinct"}).to_string();
        let parsed = Failure::parse(Provider::Claude, &connection).unwrap();
        assert_eq!(parsed.kind, FailureKind::Connection);
        assert!(parsed.reset.is_none());
        assert_eq!(parsed.key, connection);
        assert_eq!(parsed.details, connection);
        let limited = serde_json::json!({"type":"rate_limit_error", "message":quoted, "request":{"prompt":"resets 9:30pm"}}).to_string();
        let parsed = Failure::parse(Provider::Claude, &limited).unwrap();
        assert_eq!(parsed.kind, FailureKind::AccountLimit);
        assert_eq!(parsed.reset.as_deref(), Some("3:40pm (America/Vancouver)"));
        assert_eq!(parsed.details, limited);
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
}
