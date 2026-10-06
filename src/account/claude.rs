//! Claude Code's account reports.
//!
//! Two shapes carry the same limits on different scales, and confusing them
//! is the defect this module exists to prevent:
//!
//! - the `get_usage` control response reports percentages, `0..=100`, with
//!   RFC 3339 reset times;
//! - a conversation's `rate_limit_event` reports fractions, usually `0..=1`
//!   but able to exceed one, with reset times in Unix seconds.
//!
//! Both are normalized to a used fraction and Unix seconds here.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{
    Credits, Identity, Money, UsageSnapshot, UsageSource, UsageStatus, UsageWindow, WindowKind,
    humanize, parse_rfc3339, status_for,
};

const FIVE_HOURS: u64 = 5 * 3_600;
const SEVEN_DAYS: u64 = 7 * 86_400;

/// The window keys shared by both shapes, so a live event replaces the window
/// a probe reported instead of listing it twice.
const SESSION: &str = "session";
const WEEKLY: &str = "weekly_all";

/// The identity `claude auth status --json` reports, or why there is none.
pub fn parse_auth_status(status: &Value) -> Result<Identity, UsageStatus> {
    if status["loggedIn"].as_bool() != Some(true) {
        return Err(UsageStatus::NeedsLogin);
    }
    let text = |key: &str| {
        status[key]
            .as_str()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
    };
    Ok(Identity {
        email: text("email"),
        organization: text("orgName"),
        plan: text("subscriptionType"),
    })
}

/// The snapshot a `get_usage` control response describes.
///
/// `response` is the control response's inner `response` object. The
/// normalized `rate_limits.limits` list is preferred: it names each window's
/// kind, marks the binding one, and scopes model allowances. The older keyed
/// windows (`five_hour`, `seven_day`, `seven_day_opus`, ...) are read only
/// when that list is absent, and every other key in `rate_limits` - of which
/// the CLI carries a growing set of internal codenames - is ignored.
pub fn parse_usage(response: &Value, observed_at: u64) -> UsageSnapshot {
    let plan = response["subscription_type"]
        .as_str()
        .filter(|plan| !plan.is_empty())
        .map(str::to_owned);
    let limits = &response["rate_limits"];
    if response["rate_limits_available"].as_bool() == Some(false) || !limits.is_object() {
        let mut snapshot = UsageSnapshot::state(
            UsageStatus::Unavailable,
            "Claude does not report usage limits for this account",
            observed_at,
        );
        snapshot.plan = plan;
        return snapshot;
    }
    let mut provider_warning = false;
    let windows = match limits["limits"].as_array().filter(|list| !list.is_empty()) {
        Some(list) => list
            .iter()
            .filter_map(|limit| {
                let window = normalized_window(limit)?;
                provider_warning |= limit["severity"]
                    .as_str()
                    .is_some_and(|severity| !matches!(severity, "normal" | ""))
                    && window.kind != WindowKind::Scoped;
                Some(window)
            })
            .collect(),
        None => keyed_windows(limits),
    };
    let credits = credits(limits);
    let status = status_for(&windows, false, provider_warning);
    UsageSnapshot {
        windows,
        credits,
        status,
        ordinary_usage_allowed: None,
        observed_at,
        source: UsageSource::Probe,
        plan,
        detail: None,
    }
}

/// A `get_usage` control response error, classified: a state to report, or
/// `None` for an error that may pass - a network failure, an overloaded
/// service - which is retried rather than shown.
///
/// Claude reports a signed-out account as an error to the control request
/// rather than as a state, so the words are all there is to go on.
pub fn error_status(message: &str) -> Option<UsageStatus> {
    let lower = message.to_ascii_lowercase();
    if [
        "log in",
        "login",
        "logged in",
        "sign in",
        "authenticat",
        "oauth",
        "401",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
    {
        Some(UsageStatus::NeedsLogin)
    } else if [
        "unsupported",
        "not supported",
        "unknown subtype",
        "unknown control",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
    {
        // A Claude Code too old to answer `get_usage` will not start to.
        Some(UsageStatus::Unavailable)
    } else {
        None
    }
}

fn normalized_window(limit: &Value) -> Option<UsageWindow> {
    let percent = limit["percent"].as_f64()?;
    if !percent.is_finite() {
        return None;
    }
    let kind_name = limit["kind"].as_str().unwrap_or_default();
    let group = limit["group"].as_str().unwrap_or_default();
    let scope = scope_label(&limit["scope"]);
    let (kind, label, key, span) = match kind_name {
        "session" => (
            WindowKind::Session,
            "Session".to_owned(),
            SESSION.to_owned(),
            Some(FIVE_HOURS),
        ),
        "weekly_all" => (
            WindowKind::Weekly,
            "Weekly".to_owned(),
            WEEKLY.to_owned(),
            Some(SEVEN_DAYS),
        ),
        "weekly_scoped" => {
            let scope = scope.clone().unwrap_or_else(|| "scoped".into());
            (
                WindowKind::Scoped,
                format!("Weekly · {scope}"),
                format!("weekly_scoped:{scope}"),
                Some(SEVEN_DAYS),
            )
        }
        other => {
            // A kind this build does not know is shown under its group and
            // its own name rather than dropped: an unfamiliar limit that is
            // about to stop the account is still one the reader needs.
            let name = scope.clone().unwrap_or_else(|| humanize(other));
            let (kind, label, span) = match group {
                "session" => (
                    WindowKind::Session,
                    format!("Session · {name}"),
                    Some(FIVE_HOURS),
                ),
                "weekly" => (
                    WindowKind::Scoped,
                    format!("Weekly · {name}"),
                    Some(SEVEN_DAYS),
                ),
                _ => (WindowKind::Other, name, None),
            };
            let key = match &scope {
                Some(scope) => format!("{other}:{scope}"),
                None => other.to_owned(),
            };
            (kind, label, key, span)
        }
    };
    Some(UsageWindow {
        key,
        kind,
        label,
        used: super::fraction(percent / 100.0),
        resets_at: limit["resets_at"].as_str().and_then(parse_rfc3339),
        span_secs: span,
        binding: limit["is_active"].as_bool() == Some(true),
    })
}

/// What a scoped limit applies to: a model's display name, else a surface.
fn scope_label(scope: &Value) -> Option<String> {
    let model = &scope["model"];
    model["display_name"]
        .as_str()
        .or_else(|| model["id"].as_str())
        .or_else(|| scope["surface"].as_str())
        .map(str::trim)
        .filter(|label| !label.is_empty())
        .map(str::to_owned)
}

/// The windows of a response that predates the normalized list.
fn keyed_windows(limits: &Value) -> Vec<UsageWindow> {
    let mut windows = Vec::new();
    let mut keyed = |name: &str, kind, label: &str, key: &str, span| {
        let window = &limits[name];
        let Some(percent) = window["utilization"]
            .as_f64()
            .filter(|value| value.is_finite())
        else {
            return;
        };
        windows.push(UsageWindow {
            key: key.to_owned(),
            kind,
            label: label.to_owned(),
            used: super::fraction(percent / 100.0),
            resets_at: window["resets_at"].as_str().and_then(parse_rfc3339),
            span_secs: Some(span),
            binding: false,
        });
    };
    keyed(
        "five_hour",
        WindowKind::Session,
        "Session",
        SESSION,
        FIVE_HOURS,
    );
    keyed(
        "seven_day",
        WindowKind::Weekly,
        "Weekly",
        WEEKLY,
        SEVEN_DAYS,
    );
    keyed(
        "seven_day_opus",
        WindowKind::Scoped,
        "Weekly · Opus",
        "weekly_scoped:Opus",
        SEVEN_DAYS,
    );
    keyed(
        "seven_day_sonnet",
        WindowKind::Scoped,
        "Weekly · Sonnet",
        "weekly_scoped:Sonnet",
        SEVEN_DAYS,
    );
    for scoped in limits["model_scoped"].as_array().into_iter().flatten() {
        let Some(name) = scoped["display_name"]
            .as_str()
            .filter(|name| !name.is_empty())
        else {
            continue;
        };
        let key = format!("weekly_scoped:{name}");
        if windows.iter().any(|window| window.key == key) {
            continue;
        }
        let Some(percent) = scoped["utilization"]
            .as_f64()
            .filter(|value| value.is_finite())
        else {
            continue;
        };
        windows.push(UsageWindow {
            key,
            kind: WindowKind::Scoped,
            label: format!("Weekly · {name}"),
            used: super::fraction(percent / 100.0),
            resets_at: scoped["resets_at"].as_str().and_then(parse_rfc3339),
            span_secs: Some(SEVEN_DAYS),
            binding: false,
        });
    }
    windows
}

/// Extra usage, from the spend summary where the response carries one.
///
/// The spend summary states its money in minor units with an explicit
/// exponent, so it is the only amount read as money. The older
/// `extra_usage` block's credit counts carry no unit, so only whether it is
/// on and its percentage are taken from it.
fn credits(limits: &Value) -> Option<Credits> {
    let spend = &limits["spend"];
    let extra = &limits["extra_usage"];
    if !spend.is_object() && !extra.is_object() {
        return None;
    }
    let enabled = spend["enabled"].as_bool().unwrap_or(false)
        || extra["is_enabled"].as_bool().unwrap_or(false);
    let used_fraction = spend["percent"]
        .as_f64()
        .or_else(|| extra["utilization"].as_f64())
        .filter(|value| value.is_finite())
        .map(|percent| super::fraction(percent / 100.0));
    Some(Credits {
        enabled,
        unlimited: false,
        used: money(&spend["used"]).filter(|_| enabled),
        limit: money(&spend["limit"]),
        balance: None,
        used_fraction: used_fraction.filter(|_| enabled),
    })
}

fn money(value: &Value) -> Option<Money> {
    Some(Money {
        minor: value["amount_minor"].as_i64()?,
        exponent: u32::try_from(value["exponent"].as_u64()?)
            .ok()
            .filter(|exponent| *exponent <= 6)?,
        currency: value["currency"].as_str()?.to_owned(),
    })
}

/// A live `rate_limit_event`'s `rate_limit_info`, as the windows it moved and
/// the status it reported.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LiveUpdate {
    pub windows: Vec<UsageWindow>,
    /// The status the event states, which is Claude's own reading of the
    /// account: a window run past one while Claude still says `allowed` is
    /// overage being allowed, not a limit.
    pub status: Option<UsageStatus>,
}

/// The windows a conversation's `rate_limit_event` reports.
///
/// `unifiedWindows` holds fractions of each window, which can run past one,
/// with Unix-second resets. The event is absent for API-key, Bedrock and
/// Vertex sessions and until the first response carrying the headers.
/// A rejection still carries authoritative exhaustion evidence when the
/// optional windows are absent. An empty allowed event cannot refresh quota.
pub fn parse_rate_limit_event(info: &Value) -> Option<LiveUpdate> {
    let unified = &info["unifiedWindows"];
    let mut windows = Vec::new();
    for (name, key, kind, label) in [
        ("five_hour", SESSION, WindowKind::Session, "Session"),
        ("seven_day", WEEKLY, WindowKind::Weekly, "Weekly"),
        (
            "seven_day_overage_included",
            "weekly_overage",
            WindowKind::Other,
            "Weekly incl. extra usage",
        ),
    ] {
        let window = &unified[name];
        let Some(used) = window["utilization"]
            .as_f64()
            .filter(|value| value.is_finite())
        else {
            continue;
        };
        windows.push(UsageWindow {
            key: key.to_owned(),
            kind,
            label: label.to_owned(),
            used: super::fraction(used),
            resets_at: window["resetsAt"].as_i64(),
            span_secs: Some(if kind == WindowKind::Session {
                FIVE_HOURS
            } else {
                SEVEN_DAYS
            }),
            binding: false,
        });
    }
    let status = match info["status"].as_str() {
        Some("rejected") => Some(UsageStatus::Limited),
        Some("allowed_warning") => Some(UsageStatus::Warning),
        Some("allowed") => Some(UsageStatus::Ok),
        _ => None,
    };
    if windows.is_empty()
        && let Some(used) = info["utilization"]
            .as_f64()
            .filter(|value| value.is_finite())
        && let Some((key, kind, label, span_secs)) = match info["rateLimitType"].as_str() {
            Some("five_hour") => Some((SESSION, WindowKind::Session, "Session", FIVE_HOURS)),
            Some("seven_day") => Some((WEEKLY, WindowKind::Weekly, "Weekly", SEVEN_DAYS)),
            Some("seven_day_overage_included") => Some((
                "weekly_overage",
                WindowKind::Other,
                "Weekly incl. extra usage",
                SEVEN_DAYS,
            )),
            _ => None,
        }
    {
        windows.push(UsageWindow {
            key: key.to_owned(),
            kind,
            label: label.to_owned(),
            used: super::fraction(used),
            resets_at: info["resetsAt"].as_i64(),
            span_secs: Some(span_secs),
            binding: true,
        });
    }
    // A model-specific rejection does not exhaust the whole account. When
    // only that scope is reported, wait for an account-wide quota report.
    if windows.is_empty()
        && info["rateLimitType"].as_str().is_some_and(|kind| {
            !kind.is_empty()
                && !matches!(
                    kind,
                    "five_hour" | "seven_day" | "seven_day_overage_included"
                )
        })
    {
        return None;
    }
    if windows.is_empty() && status != Some(UsageStatus::Limited) {
        return None;
    }
    Some(LiveUpdate { windows, status })
}

/// Folds a live update into the snapshot a probe produced.
///
/// A live event reports only the account-wide windows, so every window it
/// does not mention - a model's weekly allowance, most of all - keeps its
/// probed figure. The binding mark is dropped because the event does not say
/// which window binds; the fullest window stands in until the next probe.
pub fn merge_live(snapshot: &mut UsageSnapshot, update: LiveUpdate, observed_at: u64) {
    for window in update.windows {
        match snapshot
            .windows
            .iter_mut()
            .find(|existing| existing.key == window.key)
        {
            Some(existing) => {
                existing.used = window.used;
                existing.resets_at = window.resets_at.or(existing.resets_at);
            }
            None => snapshot.windows.push(window),
        }
    }
    for window in &mut snapshot.windows {
        window.binding = false;
    }
    snapshot.observed_at = snapshot.observed_at.max(observed_at);
    snapshot.source = UsageSource::Live;
    snapshot.detail = None;
    match update.status {
        Some(status) => snapshot.status = status,
        None => {
            snapshot.status = UsageStatus::Ok;
            snapshot.refresh_status(false, false);
        }
    }
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

    /// The recorded response carries a dozen codename keys beside the
    /// normalized list; only the list's three limits are windows, the
    /// provider's binding mark survives, and percentages become fractions.
    #[test]
    fn get_usage_reads_the_normalized_limits_and_ignores_codenames() {
        let usage = parse_usage(&fixture("claude-get-usage.json"), 7);
        let keys: Vec<_> = usage
            .windows
            .iter()
            .map(|window| window.key.as_str())
            .collect();
        assert_eq!(keys, ["session", "weekly_all", "weekly_scoped:Fable"]);
        let session = &usage.windows[0];
        assert_eq!(session.kind, WindowKind::Session);
        assert!((session.used - 0.25).abs() < 1e-6);
        assert_eq!(session.resets_at, Some(1_790_119_800));
        assert_eq!(session.span_secs, Some(5 * 3_600));
        let fable = &usage.windows[2];
        assert_eq!(fable.label, "Weekly · Fable");
        assert!(fable.binding);
        assert_eq!(usage.binding_window().unwrap().key, "weekly_scoped:Fable");
        assert_eq!(usage.plan.as_deref(), Some("max"));
        assert_eq!(usage.observed_at, 7);
        assert_eq!(usage.source, UsageSource::Probe);
        // The weekly window's critical severity is a warning; only the
        // scoped model is exhausted, and that does not stop the account.
        assert_eq!(usage.status, UsageStatus::Warning);
        let credits = usage.credits.expect("the response describes extra usage");
        assert!(!credits.enabled);
        assert_eq!(credits.summary(), "Off");
    }

    #[test]
    fn get_usage_falls_back_to_keyed_windows_without_the_list() {
        let usage = parse_usage(
            &json!({
                "subscription_type": "pro",
                "rate_limits": {
                    "five_hour": {"utilization": 100, "resets_at": "2026-09-22T23:30:00Z"},
                    "seven_day": {"utilization": 12.5, "resets_at": null},
                    "seven_day_opus": null,
                    "nimbus_quill": {"utilization": 0, "resets_at": null},
                    "model_scoped": [{"display_name": "Opus", "utilization": 3, "resets_at": null}],
                }
            }),
            0,
        );
        let keys: Vec<_> = usage
            .windows
            .iter()
            .map(|window| window.key.as_str())
            .collect();
        assert_eq!(keys, ["session", "weekly_all", "weekly_scoped:Opus"]);
        assert_eq!(usage.status, UsageStatus::Limited);
        assert_eq!(usage.windows[1].resets_at, None);
        assert!(usage.credits.is_none());
    }

    #[test]
    fn unknown_limit_kinds_are_shown_under_their_group() {
        let usage = parse_usage(
            &json!({"rate_limits": {"limits": [
                {"kind": "daily_burst", "group": "session", "percent": 10, "resets_at": null},
                {"kind": "weekly_surface", "group": "weekly", "percent": 20,
                 "scope": {"model": null, "surface": "Cowork"}},
                {"kind": "monthly_pool", "group": "monthly", "percent": 30},
                {"kind": "broken", "group": "weekly", "percent": null},
            ]}}),
            0,
        );
        let labels: Vec<_> = usage
            .windows
            .iter()
            .map(|window| window.label.as_str())
            .collect();
        assert_eq!(
            labels,
            ["Session · Daily burst", "Weekly · Cowork", "Monthly pool"]
        );
        assert_eq!(usage.windows[2].kind, WindowKind::Other);
    }

    #[test]
    fn an_account_without_limits_is_unavailable_rather_than_empty() {
        let usage = parse_usage(
            &json!({"rate_limits_available": false, "rate_limits": null}),
            1,
        );
        assert_eq!(usage.status, UsageStatus::Unavailable);
        assert!(usage.windows.is_empty());
        assert!(usage.detail.is_some());
        assert_eq!(
            parse_usage(&json!({"subscription_type": "max"}), 1).status,
            UsageStatus::Unavailable
        );
    }

    #[test]
    fn enabled_spend_reports_money_with_its_exponent() {
        let usage = parse_usage(
            &json!({"rate_limits": {
                "limits": [],
                "spend": {
                    "enabled": true,
                    "used": {"amount_minor": 1234, "currency": "USD", "exponent": 2},
                    "limit": {"amount_minor": 5000, "currency": "USD", "exponent": 2},
                    "percent": 24.68,
                }
            }}),
            0,
        );
        let credits = usage.credits.unwrap();
        assert_eq!(credits.summary(), "$12.34 of $50.00 used");
        assert!((credits.used_fraction.unwrap() - 0.2468).abs() < 1e-6);
    }

    #[test]
    fn auth_status_yields_identity_or_a_login_prompt() {
        let identity = parse_auth_status(&fixture("claude-auth-status.json")).unwrap();
        assert_eq!(identity.email.as_deref(), Some("user@example.com"));
        assert_eq!(identity.organization.as_deref(), Some("Example Org"));
        assert_eq!(identity.plan.as_deref(), Some("max"));
        assert_eq!(
            parse_auth_status(&json!({"loggedIn": false})),
            Err(UsageStatus::NeedsLogin)
        );
        assert_eq!(parse_auth_status(&json!({})), Err(UsageStatus::NeedsLogin));
    }

    #[test]
    fn control_errors_that_name_a_login_ask_for_one() {
        assert_eq!(
            error_status("Not logged in · Please run /login"),
            Some(UsageStatus::NeedsLogin)
        );
        assert_eq!(
            error_status("OAuth token has expired"),
            Some(UsageStatus::NeedsLogin)
        );
        assert_eq!(
            error_status("Unsupported control request subtype: get_usage"),
            Some(UsageStatus::Unavailable)
        );
        assert_eq!(error_status("Request failed: 529 overloaded"), None);
    }

    /// The live event's scale is a fraction and its resets are epoch
    /// seconds; reading it as the probe's percentage would report a nearly
    /// full window as nearly empty.
    #[test]
    fn a_rate_limit_event_is_read_on_its_own_scale() {
        let update =
            parse_rate_limit_event(&fixture("claude-rate-limit-event.json")["rate_limit_info"])
                .expect("the event reports windows");
        assert_eq!(update.windows.len(), 2);
        assert!((update.windows[0].used - 0.43).abs() < 1e-6);
        assert_eq!(update.windows[0].resets_at, Some(1_790_119_800));
        assert!(
            (update.windows[1].used - 1.04).abs() < 1e-6,
            "an overrun is kept"
        );
        assert_eq!(update.status, Some(UsageStatus::Warning));
        assert_eq!(parse_rate_limit_event(&json!({"status": "allowed"})), None);
    }

    #[test]
    fn a_rejection_without_optional_windows_authorizes_routing() {
        let now = 1_000;
        let mut usage = UsageSnapshot::state(UsageStatus::Ok, "", now);
        let rejection = parse_rate_limit_event(&json!({"status": "rejected"})).unwrap();
        assert!(rejection.windows.is_empty());
        merge_live(&mut usage, rejection, now);
        assert!(super::super::routing::needs_alternate(&usage, now));
        assert!(!super::super::routing::available(&usage, now));
        assert_eq!(parse_rate_limit_event(&json!({"status": "allowed"})), None);
    }

    #[test]
    fn a_scoped_rejection_without_account_windows_does_not_exhaust_the_account() {
        for scope in ["seven_day_opus", "seven_day_sonnet", "future_model_limit"] {
            assert_eq!(
                parse_rate_limit_event(&json!({
                    "status": "rejected", "rateLimitType": scope, "utilization": 1.0,
                })),
                None,
                "a rejection for {scope} cannot authorize account-wide rotation"
            );
        }
    }

    #[test]
    fn a_top_level_window_supplies_usage_and_reset_without_unified_windows() {
        let update = parse_rate_limit_event(&json!({
            "status": "rejected",
            "rateLimitType": "five_hour",
            "utilization": 1.04,
            "resetsAt": 20,
        }))
        .unwrap();
        assert_eq!(update.windows.len(), 1);
        assert_eq!(update.windows[0].key, SESSION);
        assert_eq!(update.windows[0].used, 1.04);
        assert_eq!(update.windows[0].resets_at, Some(20));
        let mut usage = UsageSnapshot::state(UsageStatus::Ok, "", 10_000);
        merge_live(&mut usage, update, 10_000);
        assert!(super::super::routing::needs_alternate(&usage, 19_999));
        assert!(!super::super::routing::needs_alternate(&usage, 20_000));
    }

    #[test]
    fn a_live_update_replaces_account_windows_and_keeps_model_windows() {
        let mut usage = parse_usage(&fixture("claude-get-usage.json"), 10);
        let update = parse_rate_limit_event(&json!({
            "status": "rejected",
            "unifiedWindows": {"five_hour": {"utilization": 1.0, "resetsAt": 1_790_119_900_i64}},
        }))
        .unwrap();
        merge_live(&mut usage, update, 20);
        assert_eq!(usage.windows.len(), 3);
        assert!((usage.windows[0].used - 1.0).abs() < 1e-6);
        assert_eq!(usage.windows[0].resets_at, Some(1_790_119_900));
        assert!(
            (usage.windows[2].used - 1.0).abs() < 1e-6,
            "Fable kept its figure"
        );
        assert!(usage.windows.iter().all(|window| !window.binding));
        assert_eq!(usage.source, UsageSource::Live);
        assert_eq!(usage.observed_at, 20);
        assert_eq!(usage.status, UsageStatus::Limited);

        // An overrun Claude still allows is a warning, as Claude says.
        let update =
            parse_rate_limit_event(&fixture("claude-rate-limit-event.json")["rate_limit_info"])
                .unwrap();
        merge_live(&mut usage, update, 30);
        assert_eq!(usage.status, UsageStatus::Warning);
    }
}
