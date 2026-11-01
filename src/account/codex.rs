//! Codex's account reports, from its app server.
//!
//! `account/read` names the login and `account/rateLimits/read` its limits.
//! Limits are reported per metered bucket: `rateLimits` is a single-bucket
//! view kept for older clients, and `rateLimitsByLimitId` carries every
//! bucket keyed by its limit id. Each bucket has a primary and a secondary
//! window reported as a whole-number percentage used, a length in minutes and
//! a reset in Unix seconds.

use serde::{Deserialize, Serialize};
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
    let ordinary_usage_allowed = result["ordinaryUsageAllowed"].as_bool();
    let status = permission_status(&windows, ordinary_usage_allowed, limited);
    let detail = (status == UsageStatus::Unavailable)
        .then(|| "Codex did not report usable usage limits".into());
    UsageSnapshot {
        windows,
        credits,
        status,
        ordinary_usage_allowed,
        observed_at,
        source: UsageSource::Probe,
        plan,
        detail,
    }
}

/// A successful RPC frame without quota or explicit backend permission is
/// not a usable probe answer. Scoped-only windows likewise do not replace
/// ordinary-account evidence. The existing failure path retains earlier
/// figures/identity instead of publishing an unavailable replacement.
#[cfg(any(feature = "host", feature = "ui"))]
pub(super) fn probe_rate_limits(
    result: &Value,
    observed_at: u64,
) -> Result<UsageSnapshot, &'static str> {
    let snapshot = parse_rate_limits(result, observed_at);
    let ordinary_quota = snapshot
        .windows
        .iter()
        .any(|window| window.kind != WindowKind::Scoped && window.used.is_finite());
    if snapshot.ordinary_usage_allowed.is_none()
        && snapshot.status != UsageStatus::Limited
        && !ordinary_quota
    {
        Err("Codex could not read usage just now")
    } else {
        Ok(snapshot)
    }
}

fn permission_status(
    windows: &[UsageWindow],
    ordinary_usage_allowed: Option<bool>,
    limited: bool,
) -> UsageStatus {
    match ordinary_usage_allowed {
        Some(false) => UsageStatus::Limited,
        Some(true) => match status_for(windows, false, false) {
            UsageStatus::Limited => UsageStatus::Warning,
            status => status,
        },
        // Without explicit permission or rejection, no parsed window means
        // no account-wide eligibility observation, not an empty allowance.
        None if windows.is_empty() && !limited => UsageStatus::Unavailable,
        None => status_for(windows, limited, false),
    }
}

/// What an `account/rateLimits/updated` notification states.
///
/// The notification is sparse: a window or credit block it leaves null is
/// unchanged, not cleared, so it is kept as what it names and folded into a
/// snapshot by [`merge_update`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LimitsUpdate {
    pub windows: Vec<UsageWindow>,
    pub credits: Option<Credits>,
    pub plan: Option<String>,
    /// The default bucket reported its limit as reached.
    pub limited: bool,
}

/// The limits an `account/rateLimits/updated` notification's params state,
/// or `None` when it states nothing a snapshot could take.
pub fn parse_update(params: &Value) -> Option<LimitsUpdate> {
    let bucket = &params["rateLimits"];
    if !bucket.is_object() {
        return None;
    }
    let id = bucket["limitId"].as_str().unwrap_or(DEFAULT_LIMIT_ID);
    let update = LimitsUpdate {
        windows: bucket_windows(id, bucket),
        credits: bucket_credits(&bucket["credits"]),
        plan: bucket["planType"]
            .as_str()
            .filter(|plan| *plan != "unknown")
            .map(str::to_owned),
        limited: bucket["rateLimitReachedType"].is_string() && id == DEFAULT_LIMIT_ID,
    };
    (!update.windows.is_empty() || update.credits.is_some() || update.limited).then_some(update)
}

/// Folds a rate-limit update into a snapshot: only what it names replaces
/// what was known. Returns whether it renews account-wide eligibility evidence.
/// Percentages cannot renew an explicit permission from an older probe.
pub fn merge_update(snapshot: &mut UsageSnapshot, update: LimitsUpdate, observed_at: u64) -> bool {
    let account_limits_changed = update.limited
        || update
            .windows
            .iter()
            .any(|window| window.key.starts_with("codex:"));
    let eligibility_observed =
        update.limited || (account_limits_changed && snapshot.ordinary_usage_allowed.is_none());
    for window in update.windows {
        match snapshot
            .windows
            .iter_mut()
            .find(|existing| existing.key == window.key)
        {
            Some(existing) => *existing = window,
            None => snapshot.windows.push(window),
        }
    }
    if let Some(credits) = update.credits {
        snapshot.credits = Some(credits);
    }
    if let Some(plan) = update.plan {
        snapshot.plan = Some(plan);
    }
    if eligibility_observed {
        snapshot.observed_at = snapshot.observed_at.max(observed_at);
    }
    snapshot.source = UsageSource::Live;
    snapshot.detail = None;
    if account_limits_changed {
        if update.limited {
            snapshot.ordinary_usage_allowed = Some(false);
        }
        snapshot.status = permission_status(
            &snapshot.windows,
            snapshot.ordinary_usage_allowed,
            update.limited,
        );
    } else if snapshot.status == UsageStatus::Ok
        && status_for(&snapshot.windows, false, false) == UsageStatus::Warning
    {
        // A scoped allowance can warn without changing the backend's latest
        // permission for ordinary usage. Credits alone prove no recovery.
        snapshot.status = UsageStatus::Warning;
    }
    eligibility_observed
}

/// The login an `account/updated` notification reports.
///
/// Codex sends it when the login in its home changes. It is not a reading of
/// the account to show - the notification cannot tell a sign-out from a
/// provider that needs no OpenAI login - only a reason to ask again.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoginUpdate {
    /// Signed in to ChatGPT, the only login Codex reports limits for.
    pub chatgpt: bool,
    pub plan: Option<String>,
}

impl LoginUpdate {
    /// Whether this is the login the account was last seen with: signed in
    /// to ChatGPT with figures to show, on the same plan where both name one,
    /// or not signed in to ChatGPT with none. Nothing known is not
    /// contradicted.
    pub fn describes(&self, snapshot: Option<&UsageSnapshot>, identity: Option<&Identity>) -> bool {
        let Some(snapshot) = snapshot else {
            return true;
        };
        let signed_in = !matches!(
            snapshot.status,
            UsageStatus::NeedsLogin | UsageStatus::Unavailable
        );
        let plan = snapshot
            .plan
            .as_deref()
            .or_else(|| identity?.plan.as_deref());
        signed_in == self.chatgpt
            && (!self.chatgpt || plan.zip(self.plan.as_deref()).is_none_or(|(a, b)| a == b))
    }
}

/// The login an `account/updated` notification's params report.
pub fn parse_login_update(params: &Value) -> LoginUpdate {
    LoginUpdate {
        chatgpt: matches!(
            params["authMode"].as_str(),
            Some("chatgpt" | "chatgptAuthTokens")
        ),
        plan: params["planType"]
            .as_str()
            .filter(|plan| *plan != "unknown")
            .map(str::to_owned),
    }
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

    #[test]
    fn backend_permission_keeps_exhausted_windows_usable() {
        let usage = parse_rate_limits(
            &json!({"ordinaryUsageAllowed": true, "rateLimits": {
                "primary": {"usedPercent": 100, "windowDurationMins": 300},
                "rateLimitReachedType": "rate_limit_reached",
            }}),
            1_000,
        );
        assert_eq!(usage.status, UsageStatus::Warning);
        assert_eq!(usage.windows[0].used, 1.0);
        assert!(super::super::routing::available(&usage, 1_000));
        assert!(!super::super::routing::needs_alternate(&usage, 1_000));
    }

    /// A sparse update moves only what it names: the weekly window and the
    /// other bucket keep their probed figures.
    #[test]
    fn a_sparse_update_replaces_only_what_it_names() {
        let mut usage = parse_rate_limits(&fixture("codex-rate-limits-read.json")["result"], 3);
        let update = parse_update(&fixture("codex-rate-limits-updated.json")["params"]).unwrap();
        merge_update(&mut usage, update, 9);
        assert_eq!(usage.windows.len(), 3);
        assert!((usage.windows[0].used - 0.57).abs() < 1e-6);
        assert!((usage.windows[1].used - 0.81).abs() < 1e-6, "weekly kept");
        assert!(usage.credits.is_some(), "credits kept");
        assert_eq!(usage.source, UsageSource::Live);
        assert_eq!(usage.observed_at, 3);
        assert_eq!(parse_update(&json!({"rateLimits": null})), None);
        assert_eq!(
            parse_update(&json!({"rateLimits": {"primary": null, "secondary": null}})),
            None,
            "an update naming nothing states nothing"
        );
    }

    #[test]
    fn unrelated_updates_preserve_backend_permission_for_ordinary_usage() {
        for (allowed, used, expected) in [
            (false, 10, UsageStatus::Limited),
            (true, 100, UsageStatus::Warning),
        ] {
            for bucket in [
                json!({"limitId": "codex_bengalfox", "primary": {
                    "usedPercent": 20, "windowDurationMins": 300,
                }}),
                json!({"credits": {"hasCredits": true, "unlimited": false, "balance": "12.50"}}),
            ] {
                let mut usage = parse_rate_limits(
                    &json!({"ordinaryUsageAllowed": allowed, "rateLimits": {
                        "primary": {"usedPercent": used, "windowDurationMins": 300},
                    }}),
                    1_000,
                );
                let update = parse_update(&json!({"rateLimits": bucket})).unwrap();
                assert!(
                    super::super::LiveReport::CodexLimits(update).apply(&mut usage, 1_001, 1_001)
                );
                assert_eq!(usage.status, expected);
                assert_eq!(usage.observed_at, 1_000);
                assert_eq!(
                    super::super::routing::needs_alternate(&usage, 1_001),
                    !allowed
                );
                assert_eq!(super::super::routing::available(&usage, 1_001), allowed);
                let stale = 1_000 + super::super::schedule::DISPLAY_STALE_AFTER + 1;
                let update = parse_update(&json!({"rateLimits": {
                    "limitId": "codex_bengalfox",
                    "primary": {"usedPercent": 40, "windowDurationMins": 300},
                }}))
                .unwrap();
                assert!(
                    super::super::LiveReport::CodexLimits(update).apply(&mut usage, stale, stale)
                );
                assert!(!super::super::routing::needs_alternate(&usage, stale));
                assert!(!super::super::routing::available(&usage, stale));
            }
        }
    }

    #[test]
    fn default_window_updates_preserve_permission_until_explicit_backend_evidence() {
        for (allowed, used, expected) in [
            (false, 10, UsageStatus::Limited),
            (true, 100, UsageStatus::Warning),
        ] {
            let probe = parse_rate_limits(
                &json!({"ordinaryUsageAllowed": allowed, "rateLimits": {
                    "primary": {"usedPercent": used, "windowDurationMins": 300},
                }}),
                1_000,
            );
            // Cached/device snapshots retain permission across a JSON round trip.
            let mut usage: UsageSnapshot =
                serde_json::from_value(serde_json::to_value(&probe).unwrap()).unwrap();
            let update = parse_update(&json!({"rateLimits": {
                "primary": {"usedPercent": used, "windowDurationMins": 300},
            }}))
            .unwrap();
            assert!(!merge_update(&mut usage, update, 1_001));
            assert_eq!(usage.status, expected);
            assert_eq!(usage.ordinary_usage_allowed, Some(allowed));
            assert_eq!(super::super::routing::available(&usage, 1_001), allowed);
            assert_eq!(usage.observed_at, 1_000);
            let stale = 1_000 + super::super::schedule::DISPLAY_STALE_AFTER + 1;
            let update = parse_update(&json!({"rateLimits": {
                "primary": {"usedPercent": used, "windowDurationMins": 300},
            }}))
            .unwrap();
            assert!(super::super::LiveReport::CodexLimits(update).apply(&mut usage, stale, stale));
            assert_eq!(usage.observed_at, 1_000);
            assert!(!super::super::routing::needs_alternate(&usage, stale));
            assert!(!super::super::routing::available(&usage, stale));

            let rejection = parse_update(&json!({"rateLimits": {
                "rateLimitReachedType": "rate_limit_reached",
            }}))
            .unwrap();
            assert!(merge_update(&mut usage, rejection, 1_002));
            assert_eq!(usage.status, UsageStatus::Limited);
            assert_eq!(usage.ordinary_usage_allowed, Some(false));
        }
    }

    #[test]
    fn legacy_snapshots_without_permission_still_infer_default_window_status() {
        let mut value = serde_json::to_value(parse_rate_limits(
            &json!({"rateLimits": {"primary": {
                "usedPercent": 10, "windowDurationMins": 300,
            }}}),
            1_000,
        ))
        .unwrap();
        assert!(
            value
                .as_object_mut()
                .unwrap()
                .remove("ordinary_usage_allowed")
                .is_none()
        );
        let mut usage: UsageSnapshot = serde_json::from_value(value).unwrap();
        let update = parse_update(&json!({"rateLimits": {
            "primary": {"usedPercent": 100, "windowDurationMins": 300},
        }}))
        .unwrap();
        assert!(merge_update(&mut usage, update, 1_001));
        assert_eq!(usage.status, UsageStatus::Limited);
        assert_eq!(usage.ordinary_usage_allowed, None);
    }

    /// A login report is a reason to ask again only when it contradicts
    /// what the account was last seen as.
    #[test]
    fn a_login_report_is_checked_against_what_was_seen() {
        let figures = parse_rate_limits(&fixture("codex-rate-limits-read.json")["result"], 3);
        let signed_out = UsageSnapshot::state(UsageStatus::NeedsLogin, "Signed out", 3);
        let same = parse_login_update(&fixture("codex-account-updated.json")["params"]);
        assert_eq!(
            same,
            LoginUpdate {
                chatgpt: true,
                plan: Some("prolite".into()),
            }
        );
        assert!(same.describes(Some(&figures), None));
        assert!(
            same.describes(None, None),
            "nothing known is not contradicted"
        );
        assert!(!same.describes(Some(&signed_out), None));
        let upgraded = parse_login_update(&json!({"authMode": "chatgpt", "planType": "pro"}));
        assert!(!upgraded.describes(Some(&figures), None));
        let logged_out = parse_login_update(&json!({"authMode": null, "planType": null}));
        assert!(!logged_out.describes(Some(&figures), None));
        assert!(logged_out.describes(Some(&signed_out), None));
        let api_key = parse_login_update(&json!({"authMode": "apikey", "planType": null}));
        assert!(!api_key.chatgpt);
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

    #[test]
    fn sparse_or_malformed_probe_limits_do_not_authorize_routing() {
        let now = 1_000;
        for result in [
            json!(null),
            json!({}),
            json!({"rateLimits": null}),
            json!({"rateLimits": {}}),
            json!({"rateLimits": []}),
            json!({"rateLimits": {"primary": null, "secondary": null}}),
            json!({"rateLimits": {"primary": {"usedPercent": "0"},
                "secondary": {"usedPercent": false}}}),
            json!({"rateLimits": {"primary": {"usedPercent": []},
                "secondary": {"usedPercent": {}}}}),
            json!({"rateLimitsByLimitId": {}}),
            json!({"rateLimitsByLimitId": {"codex": null}}),
            json!({"rateLimitsByLimitId": {"codex": {"primary": {"usedPercent": null}}}}),
            json!({"rateLimitsByLimitId": {"codex": [], "future": false}}),
            json!({"ordinaryUsageAllowed": "true", "rateLimits": {}}),
            json!({"ordinaryUsageAllowed": 1, "rateLimits": null}),
        ] {
            let usage = parse_rate_limits(&result, now);
            assert_eq!(usage.status, UsageStatus::Unavailable, "{result}");
            assert!(usage.windows.is_empty(), "{result}");
            assert_eq!(usage.ordinary_usage_allowed, None, "{result}");
            assert!(usage.detail.is_some());
            assert_eq!(usage.observed_at, now);
            assert!(!super::super::routing::available(&usage, now), "{result}");
            assert!(
                !super::super::routing::needs_alternate(&usage, now),
                "unknown quota cannot claim exhaustion or sign-out: {result}"
            );
        }
    }

    #[test]
    fn explicit_permission_without_windows_retains_its_backend_meaning() {
        for limits in [
            json!(null),
            json!({}),
            json!({"primary": {"usedPercent": "unavailable"}}),
        ] {
            for allowed in [true, false] {
                let mut usage = parse_rate_limits(
                    &json!({"ordinaryUsageAllowed": allowed, "rateLimits": limits}),
                    1_000,
                );
                assert!(usage.windows.is_empty());
                assert_eq!(usage.ordinary_usage_allowed, Some(allowed));
                assert_eq!(
                    usage.status,
                    if allowed {
                        UsageStatus::Ok
                    } else {
                        UsageStatus::Limited
                    }
                );
                assert_eq!(super::super::routing::available(&usage, 1_000), allowed);
                assert_eq!(
                    super::super::routing::needs_alternate(&usage, 1_000),
                    !allowed
                );
                // Percent-only live updates cannot renew or override the
                // backend permission, even when its probe had no windows.
                let update = parse_update(&json!({"rateLimits": {
                    "primary": {"usedPercent": 10, "windowDurationMins": 300},
                }}))
                .unwrap();
                assert!(!merge_update(&mut usage, update, 1_001));
                assert_eq!(usage.observed_at, 1_000);
                assert_eq!(usage.ordinary_usage_allowed, Some(allowed));
                assert_eq!(super::super::routing::available(&usage, 1_001), allowed);
            }
        }
        let reached = parse_rate_limits(
            &json!({"rateLimits": {
                "rateLimitReachedType": "rate_limit_reached",
            }}),
            1_000,
        );
        assert_eq!(reached.status, UsageStatus::Limited);
        assert!(super::super::routing::needs_alternate(&reached, 1_000));
        assert!(!super::super::routing::available(&reached, 1_000));
    }

    #[test]
    fn quota_metadata_alone_does_not_authorize_routing_but_finite_windows_do() {
        let mut result = json!({"rateLimits": {
            "planType": "pro", "credits": {"hasCredits": true, "balance": "12.50"},
        }});
        let unknown = parse_rate_limits(&result, 1_000);
        assert_eq!(unknown.status, UsageStatus::Unavailable);
        assert_eq!(unknown.plan.as_deref(), Some("pro"));
        assert_eq!(unknown.credits.as_ref().unwrap().summary(), "12.50 left");
        assert!(!super::super::routing::available(&unknown, 1_000));
        for (percent, status, available) in [
            (0, UsageStatus::Ok, true),
            (81, UsageStatus::Warning, true),
            (100, UsageStatus::Limited, false),
        ] {
            result["rateLimits"]["primary"] = json!({"usedPercent": percent});
            let reported = parse_rate_limits(&result, 1_001);
            assert_eq!(reported.status, status);
            assert_eq!(reported.plan, unknown.plan);
            assert_eq!(reported.credits, unknown.credits);
            assert_eq!(
                super::super::routing::available(&reported, 1_001),
                available
            );
        }
    }
}
