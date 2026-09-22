//! Codex's account reports, from its app server.
//!
//! `account/read` names the login and `account/rateLimits/read` its limits.
//! Limits are reported per metered bucket: `rateLimits` is a single-bucket
//! view kept for older clients, and `rateLimitsByLimitId` carries every
//! bucket keyed by its limit id. Each bucket has a primary and a secondary
//! window reported as a whole-number percentage used, a length in minutes and
//! a reset in Unix seconds.

use serde_json::Value;

use super::{
    Credits, Identity, UsageSnapshot, UsageSource, UsageStatus, UsageWindow, WindowKind,
    span_label, status_for,
};

/// The bucket `rateLimits` describes when it does not name one.
const DEFAULT_LIMIT_ID: &str = "codex";
/// JSON-RPC's code for a method the server does not have.
const METHOD_NOT_FOUND: i64 = -32601;

/// The identity `account/read` reports, or why there is none.
pub fn parse_account(result: &Value) -> Result<Identity, UsageStatus> {
    let account = &result["account"];
    if !account.is_object() {
        // No stored login. Codex can still run against an API key or another
        // provider, in which case it asks for no OpenAI login at all and
        // there are simply no ChatGPT limits to show.
        return Err(if result["requiresOpenaiAuth"].as_bool() == Some(false) {
            UsageStatus::Unavailable
        } else {
            UsageStatus::NeedsLogin
        });
    }
    if account["type"].as_str() != Some("chatgpt") {
        return Err(UsageStatus::Unavailable);
    }
    let text = |key: &str| {
        account[key]
            .as_str()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
    };
    Ok(Identity {
        email: text("email"),
        organization: None,
        plan: text("planType").filter(|plan| plan != "unknown"),
    })
}

/// The snapshot an `account/rateLimits/read` result describes.
pub fn parse_rate_limits(result: &Value, observed_at: u64) -> UsageSnapshot {
    let mut windows = Vec::new();
    let mut credits = None;
    let mut plan = None;
    let mut limited = false;
    let buckets: Vec<(&str, &Value)> = match result["rateLimitsByLimitId"].as_object() {
        Some(buckets) if !buckets.is_empty() => {
            let mut buckets: Vec<_> = buckets
                .iter()
                .map(|(id, bucket)| (id.as_str(), bucket))
                .collect();
            // The default bucket leads, so the account's main limits read
            // first however the server ordered its map.
            buckets.sort_by_key(|(id, _)| (*id != DEFAULT_LIMIT_ID, *id));
            buckets
        }
        _ => vec![(
            result["rateLimits"]["limitId"]
                .as_str()
                .unwrap_or(DEFAULT_LIMIT_ID),
            &result["rateLimits"],
        )],
    };
    for (id, bucket) in buckets {
        windows.extend(bucket_windows(id, bucket));
        credits = credits.or_else(|| bucket_credits(&bucket["credits"]));
        plan = plan.or_else(|| {
            bucket["planType"]
                .as_str()
                .filter(|plan| *plan != "unknown")
                .map(str::to_owned)
        });
        limited |= bucket["rateLimitReachedType"].is_string() && id == DEFAULT_LIMIT_ID;
    }
    // Explicit permission outranks arithmetic: the server says whether
    // ordinary usage is allowed, and clients are told not to infer it back
    // from percentages.
    limited |= result["ordinaryUsageAllowed"].as_bool() == Some(false);
    let status = status_for(&windows, limited, false);
    UsageSnapshot {
        windows,
        credits,
        status,
        observed_at,
        source: UsageSource::Probe,
        plan,
        detail: None,
    }
}

/// Folds an `account/rateLimits/updated` notification into a snapshot.
///
/// The notification is sparse: a window or credit block it leaves null is
/// unchanged, not cleared, so only what it states replaces what was known.
pub fn merge_update(snapshot: &mut UsageSnapshot, params: &Value, observed_at: u64) {
    let bucket = &params["rateLimits"];
    if !bucket.is_object() {
        return;
    }
    let id = bucket["limitId"].as_str().unwrap_or(DEFAULT_LIMIT_ID);
    for window in bucket_windows(id, bucket) {
        match snapshot
            .windows
            .iter_mut()
            .find(|existing| existing.key == window.key)
        {
            Some(existing) => *existing = window,
            None => snapshot.windows.push(window),
        }
    }
    if let Some(credits) = bucket_credits(&bucket["credits"]) {
        snapshot.credits = Some(credits);
    }
    if let Some(plan) = bucket["planType"]
        .as_str()
        .filter(|plan| *plan != "unknown")
    {
        snapshot.plan = Some(plan.to_owned());
    }
    let limited = bucket["rateLimitReachedType"].is_string() && id == DEFAULT_LIMIT_ID;
    snapshot.observed_at = snapshot.observed_at.max(observed_at);
    snapshot.source = UsageSource::Live;
    snapshot.detail = None;
    snapshot.status = UsageStatus::Ok;
    snapshot.refresh_status(limited, false);
}

/// How a JSON-RPC error from the app server should be shown.
///
/// A stale or revoked login reaches the client as an internal error whose
/// message quotes the backend's 401. It is still a login the user can fix
/// by signing in again, which is a different state from a server that
/// cannot report limits at all.
///
/// Anything else - the backend unreachable, a 5xx - is `None`: a failure
/// that may pass, retried rather than shown as a state.
pub fn error_status(error: &Value) -> Option<UsageStatus> {
    let message = error["message"].as_str().unwrap_or_default();
    let lower = message.to_ascii_lowercase();
    if [
        "401",
        "unauthorized",
        "sign in again",
        "signing in again",
        "log in again",
        "not logged in",
        "login required",
        "token expired",
        "refresh token",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
    {
        Some(UsageStatus::NeedsLogin)
    } else if error["code"].as_i64() == Some(METHOD_NOT_FOUND) || lower.contains("method not found")
    {
        // A Codex too old to have the method will not grow it.
        Some(UsageStatus::Unavailable)
    } else {
        None
    }
}

fn bucket_windows(id: &str, bucket: &Value) -> Vec<UsageWindow> {
    let name = bucket["limitName"]
        .as_str()
        .map(str::trim)
        .filter(|name| !name.is_empty() && id != DEFAULT_LIMIT_ID);
    ["primary", "secondary"]
        .into_iter()
        .filter_map(|slot| {
            let window = &bucket[slot];
            let used = window["usedPercent"]
                .as_f64()
                .filter(|value| value.is_finite())?;
            let span_secs = window["windowDurationMins"]
                .as_u64()
                .filter(|minutes| *minutes > 0)
                .and_then(|minutes| minutes.checked_mul(60));
            let kind = match span_secs {
                Some(span) if span <= 86_400 => WindowKind::Session,
                Some(span) if span == 7 * 86_400 && name.is_none() => WindowKind::Weekly,
                Some(_) if name.is_some() => WindowKind::Scoped,
                Some(_) => WindowKind::Other,
                None if name.is_some() => WindowKind::Scoped,
                None if slot == "primary" => WindowKind::Session,
                None => WindowKind::Weekly,
            };
            let span = match span_secs {
                Some(span) if span <= 86_400 => format!("{} window", span_label(span)),
                Some(span) => span_label(span),
                None if slot == "primary" => "Primary".into(),
                None => "Secondary".into(),
            };
            Some(UsageWindow {
                key: format!("{id}:{slot}"),
                kind,
                label: match name {
                    Some(name) => format!("{span} · {name}"),
                    None => span,
                },
                used: super::fraction(used / 100.0),
                resets_at: window["resetsAt"].as_i64(),
                span_secs,
                binding: false,
            })
        })
        .collect()
}

fn bucket_credits(credits: &Value) -> Option<Credits> {
    if !credits.is_object() {
        return None;
    }
    let unlimited = credits["unlimited"].as_bool().unwrap_or(false);
    let enabled = unlimited || credits["hasCredits"].as_bool().unwrap_or(false);
    Some(Credits {
        enabled,
        unlimited,
        used: None,
        limit: None,
        balance: credits["balance"]
            .as_str()
            .map(str::trim)
            .filter(|balance| !balance.is_empty())
            .map(str::to_owned),
        used_fraction: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fixture(name: &str) -> Value {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("fixtures/accounts")
            .join(name);
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap()
    }

    #[test]
    fn account_read_yields_identity_or_a_login_prompt() {
        let identity = parse_account(&fixture("codex-account-read.json")["result"]).unwrap();
        assert_eq!(identity.email.as_deref(), Some("user@example.com"));
        assert_eq!(identity.plan.as_deref(), Some("prolite"));
        assert_eq!(
            parse_account(&json!({"account": null, "requiresOpenaiAuth": true})),
            Err(UsageStatus::NeedsLogin)
        );
        assert_eq!(
            parse_account(&json!({"account": null, "requiresOpenaiAuth": false})),
            Err(UsageStatus::Unavailable)
        );
        assert_eq!(
            parse_account(&json!({"account": {"type": "apiKey"}, "requiresOpenaiAuth": true})),
            Err(UsageStatus::Unavailable)
        );
    }

    /// Every bucket is reported, the default one first, each window keyed by
    /// bucket and slot so a later sparse update can find it.
    #[test]
    fn rate_limits_read_reports_every_bucket() {
        let usage = parse_rate_limits(&fixture("codex-rate-limits-read.json")["result"], 3);
        let keys: Vec<_> = usage
            .windows
            .iter()
            .map(|window| window.key.as_str())
            .collect();
        assert_eq!(
            keys,
            [
                "codex:primary",
                "codex:secondary",
                "codex_bengalfox:primary"
            ]
        );
        let labels: Vec<_> = usage
            .windows
            .iter()
            .map(|window| window.label.as_str())
            .collect();
        assert_eq!(
            labels,
            ["5h window", "Weekly", "5h window · GPT-5.3-Codex-Spark"]
        );
        assert_eq!(usage.windows[0].kind, WindowKind::Session);
        assert_eq!(usage.windows[1].kind, WindowKind::Weekly);
        assert!((usage.windows[0].used - 0.42).abs() < 1e-6);
        assert_eq!(usage.windows[0].resets_at, Some(1_790_119_800));
        assert_eq!(usage.windows[1].span_secs, Some(7 * 86_400));
        assert_eq!(usage.plan.as_deref(), Some("prolite"));
        assert_eq!(usage.status, UsageStatus::Warning);
        assert_eq!(usage.binding_window().unwrap().key, "codex:secondary");
        let credits = usage.credits.unwrap();
        assert_eq!(credits.summary(), "Off");
    }

    #[test]
    fn the_single_bucket_view_is_read_when_there_is_no_map() {
        let usage = parse_rate_limits(
            &json!({"rateLimits": {
                "primary": {"usedPercent": 100, "windowDurationMins": 300, "resetsAt": 10},
                "secondary": null,
                "credits": {"hasCredits": true, "unlimited": false, "balance": "12.50"},
                "rateLimitReachedType": "rate_limit_reached",
            }}),
            0,
        );
        assert_eq!(usage.windows.len(), 1);
        assert_eq!(usage.windows[0].key, "codex:primary");
        assert_eq!(usage.status, UsageStatus::Limited);
        assert_eq!(usage.credits.unwrap().summary(), "12.50 left");
    }

    #[test]
    fn the_server_saying_usage_is_not_allowed_is_limited() {
        let usage = parse_rate_limits(
            &json!({"ordinaryUsageAllowed": false, "rateLimits": {
                "primary": {"usedPercent": 10, "windowDurationMins": 300},
            }}),
            0,
        );
        assert_eq!(usage.status, UsageStatus::Limited);
    }

    /// A sparse update moves only what it names: the weekly window and the
    /// other bucket keep their probed figures.
    #[test]
    fn a_sparse_update_replaces_only_what_it_names() {
        let mut usage = parse_rate_limits(&fixture("codex-rate-limits-read.json")["result"], 3);
        merge_update(
            &mut usage,
            &fixture("codex-rate-limits-updated.json")["params"],
            9,
        );
        assert_eq!(usage.windows.len(), 3);
        assert!((usage.windows[0].used - 0.57).abs() < 1e-6);
        assert!((usage.windows[1].used - 0.81).abs() < 1e-6, "weekly kept");
        assert!(usage.credits.is_some(), "credits kept");
        assert_eq!(usage.source, UsageSource::Live);
        assert_eq!(usage.observed_at, 9);
        merge_update(&mut usage, &json!({"rateLimits": null}), 12);
        assert_eq!(usage.observed_at, 9, "an empty update changes nothing");
    }

    /// The recorded failure from a stale login on this workstation.
    #[test]
    fn an_unauthorized_rate_limit_read_asks_for_a_login() {
        let error = fixture("codex-rate-limits-401.json");
        assert_eq!(error_status(&error["error"]), Some(UsageStatus::NeedsLogin));
        assert_eq!(
            error_status(&json!({"code": -32601, "message": "method not found"})),
            Some(UsageStatus::Unavailable)
        );
        assert_eq!(
            error_status(
                &json!({"code": -32603, "message": "failed to fetch codex rate limits: 502 Bad Gateway"})
            ),
            None
        );
    }
}
