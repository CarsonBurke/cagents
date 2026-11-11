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
        disputed_by: None,
    })
}

/// What a Claude Code configuration records of who its login is, as
/// `claude auth status` reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordedLogin {
    /// It records no login.
    Nobody,
    /// It names the login with this address.
    Login(String),
    /// The CLI could not say - it failed, or did not answer in time - so it
    /// may name anyone.
    Unknown,
}

impl RecordedLogin {
    /// What the record `claude auth status --json` reported from names.
    pub fn reported(status: &Value) -> Self {
        parse_auth_status(status)
            .ok()
            .and_then(|identity| identity.email)
            .map_or(Self::Nobody, Self::Login)
    }
}

/// How long, in milliseconds, a sign-in that cagents ran settles the next
/// disagreement a probe sees: the probe that follows it runs at once, so a
/// sign-in older than this is not the one that wrote the CLI's record.
#[cfg(any(feature = "ui", feature = "host"))]
pub const SIGN_IN_SETTLES_FOR: u64 = 10 * 60 * 1000;

/// What a probe reports of the system login's two records, and how the
/// settlement recorded for them changes: `reported` is the record the CLI
/// read, `other` what the second record names, `previous` the identity the
/// last probe reported, `settled` the settlement on record and `now` the
/// time in Unix milliseconds.
///
/// A disagreement is reported until it is settled, and settled in favour of
/// the CLI's record when that record is known to be newer than the other:
/// when a sign-in that cagents ran has just finished, or when the CLI's
/// record has changed login since the last probe while the other stayed as
/// it was. Only signing in rewrites the login a record names. A record that
/// cannot be read settles nothing, and leaves what the last probe reported.
///
/// The second element is `None` to leave the settlement as it is, or the
/// settlement to record in its place.
#[cfg(any(feature = "ui", feature = "host"))]
pub fn settle_dispute(
    reported: &Identity,
    other: RecordedLogin,
    previous: Option<&Identity>,
    settled: Option<&super::cache::Settled>,
    now: u64,
) -> (Option<String>, Option<Option<super::cache::Settled>>) {
    use super::cache::Settled;
    let same = |a: &str, b: &str| a.trim().eq_ignore_ascii_case(b.trim());
    // A probe that names no login has nothing to compare.
    let Some(email) = reported.email.as_deref() else {
        return (None, None);
    };
    let previous_email = previous.and_then(|previous| previous.email.as_deref());
    let other = match other {
        RecordedLogin::Login(other) if !same(&other, email) => other,
        RecordedLogin::Unknown => {
            let shown = previous
                .filter(|_| previous_email.is_some_and(|before| same(before, email)))
                .and_then(|previous| previous.disputed_by.clone());
            return (shown, None);
        }
        // The records agree: nothing is left to settle.
        RecordedLogin::Login(_) | RecordedLogin::Nobody => {
            return (None, settled.is_some().then_some(None));
        }
    };
    let settle = Some(Some(Settled::Reported {
        reported: email.trim().to_owned(),
        other: other.clone(),
    }));
    let (known_other, expired) = match settled {
        Some(Settled::SignedIn { at }) => {
            if *at <= now.saturating_add(super::schedule::CLOCK_SLACK)
                && now.saturating_sub(*at) <= SIGN_IN_SETTLES_FOR
            {
                return (None, settle);
            }
            (None, true)
        }
        Some(Settled::Reported {
            reported: settled_reported,
            other: settled_other,
        }) => {
            if same(settled_reported, email) && same(settled_other, &other) {
                return (None, None);
            }
            (Some(settled_other.as_str()), false)
        }
        None => (None, false),
    };
    let rewritten = previous_email.is_some_and(|before| !same(before, email))
        && previous
            .and_then(|previous| previous.disputed_by.as_deref())
            .or(known_other)
            .is_some_and(|before| same(before, &other));
    if rewritten {
        return (None, settle);
    }
    (Some(other), expired.then_some(None))
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
    // An object (or metadata about credits/plan) is not positive quota.
    // Missing or malformed windows must not authorize automatic routing.
    let (status, detail) = if windows.is_empty() {
        (
            UsageStatus::Unavailable,
            Some("Claude did not report usable usage limits".into()),
        )
    } else {
        (status_for(&windows, false, provider_warning), None)
    };
    UsageSnapshot {
        resets: parse_resets(limits, observed_at / 1000),
        windows,
        credits,
        status,
        ordinary_usage_allowed: None,
        observed_at,
        source: UsageSource::Probe,
        plan,
        detail,
    }
}

/// Inventory from the private usage endpoint. Only the provider's explicit
/// `clears` list authorizes clearing a window; display labels never do.
pub fn parse_resets(body: &Value, now_secs: u64) -> Vec<super::UsageReset> {
    use super::{ResetKind, ResetScope, UsageReset};
    let mut resets = Vec::new();
    let program = &body["cedar_ember"];
    for grant in program["grants"].as_array().into_iter().flatten().take(256) {
        let Some(id) = grant["id"].as_str().filter(|id| valid_grant_id(id)) else {
            continue;
        };
        let Some(available_count) = grant["resets_left"].as_u64() else {
            continue;
        };
        let scopes = grant["clears"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|scope| {
                Some(match scope.as_str()? {
                    "five_hour" => ResetScope::AccountSession,
                    "seven_day" => ResetScope::AccountWeekly,
                    "seven_day_overage_included" => ResetScope::Model("Fable".into()),
                    "seven_day_opus" => ResetScope::Model("Opus".into()),
                    "seven_day_sonnet" => ResetScope::Model("Sonnet".into()),
                    "seven_day_cowork" => ResetScope::Model("Cowork".into()),
                    "seven_day_omelette" => ResetScope::Model("Omelette".into()),
                    "seven_day_oauth_apps" => ResetScope::Model("OAuth apps".into()),
                    _ => return None,
                })
            })
            .collect();
        let starts = grant["starts_at"].as_str().and_then(parse_rfc3339);
        let expires_at = grant["ends_at"].as_str().and_then(parse_rfc3339);
        let valid_times = ["starts_at", "ends_at"].iter().all(|key| {
            grant.get(*key).is_none_or(|value| {
                value.is_null() || value.as_str().and_then(parse_rfc3339).is_some()
            })
        });
        let usable = valid_times
            && program["eligible"].as_bool() == Some(true)
            && program["next_grant_id"].as_str() == Some(id)
            && grant["usable_now"].as_bool() == Some(true)
            && grant["paused"].as_bool() != Some(true)
            && starts.is_none_or(|start| start <= now_secs as i64)
            && (!grant["use_requires_limit"].as_bool().unwrap_or(true)
                || program["at_limit"].as_bool() == Some(true));
        resets.push(UsageReset {
            id: format!("cedar_ember/{id}"),
            kind: ResetKind::Granted,
            scopes,
            available_count,
            expires_at,
            usable,
        });
    }
    let session = &body["juniper_tide"];
    if session.is_object() && session["arm"].as_str() == Some("reset") {
        let available = session["available"].as_bool() == Some(true);
        resets.push(UsageReset {
            id: "juniper_tide".into(),
            kind: ResetKind::WeeklySession,
            scopes: vec![ResetScope::AccountSession],
            available_count: u64::from(available),
            expires_at: session["weekly_resets_at"].as_str().and_then(parse_rfc3339),
            usable: available && session["eligible"].as_bool() == Some(true),
        });
    }
    resets
}

pub(super) fn valid_grant_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 40
        && id.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'_' | b'-')
        })
}

/// A probe must distinguish a transient missing reading from the provider
/// explicitly saying that this account does not support usage reporting.
/// Scoped-only windows are display data, not an ordinary-usage observation.
/// Returning an error keeps the prior snapshot and identity under the existing
/// probe failure policy; no replacement quota or catalog is published.
#[cfg(any(feature = "host", feature = "ui"))]
pub(super) fn probe_usage(
    response: &Value,
    observed_at: u64,
) -> Result<UsageSnapshot, &'static str> {
    let snapshot = parse_usage(response, observed_at);
    let ordinary_quota = snapshot
        .windows
        .iter()
        .any(|window| window.kind != WindowKind::Scoped && window.used.is_finite());
    if !ordinary_quota && response["rate_limits_available"].as_bool() != Some(false) {
        Err("Claude Code could not read usage just now")
    } else {
        Ok(snapshot)
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
    let kind_name = limit["kind"]
        .as_str()
        .filter(|kind| !kind.trim().is_empty())?;
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

/// The model or surface a window applies to, where Claude scoped it. A
/// scoped window's key is `{kind}:{scope}`; an account-wide one's has no
/// colon, whatever its label shows.
pub fn window_scope(window: &UsageWindow) -> Option<&str> {
    window.key.split_once(':').map(|(_, scope)| scope)
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
    /// overage being allowed, not a limit. `None` when the event's status
    /// is about one model's allowance rather than the account.
    pub status: Option<UsageStatus>,
}

/// A rate-limit type Claude Code names in a live event, and the window it is.
struct LiveWindow {
    name: &'static str,
    key: &'static str,
    kind: WindowKind,
    label: &'static str,
    span_secs: u64,
}

/// The windows a live event can carry. `seven_day_overage_included` is the
/// weekly allowance of the models that run on extra usage, which Claude
/// Code calls the "Fable limit" and shows as the `get_usage` limit scoped to
/// Fable, so it is that window rather than another one beside it.
const LIVE_WINDOWS: [LiveWindow; 3] = [
    LiveWindow {
        name: "five_hour",
        key: SESSION,
        kind: WindowKind::Session,
        label: "Session",
        span_secs: FIVE_HOURS,
    },
    LiveWindow {
        name: "seven_day",
        key: WEEKLY,
        kind: WindowKind::Weekly,
        label: "Weekly",
        span_secs: SEVEN_DAYS,
    },
    LiveWindow {
        name: "seven_day_overage_included",
        key: FABLE,
        kind: WindowKind::Scoped,
        label: "Weekly · Fable",
        span_secs: SEVEN_DAYS,
    },
];

/// The probe's key for the Fable weekly window, which a live event's
/// `seven_day_overage_included` updates.
const FABLE: &str = "weekly_scoped:Fable";

/// The key builds before this one gave the live Fable window, under the label
/// "Weekly incl. extra usage". A cached reading may still hold it.
const RETIRED_FABLE: &str = "weekly_overage";

/// Whether a scoped window is Fable's, under any name Claude has given it:
/// `Fable`, or a version of it such as `Fable 5.1`.
fn is_fable(window: &UsageWindow) -> bool {
    window.kind == WindowKind::Scoped
        && window_scope(window).is_some_and(|scope| {
            scope
                .split_whitespace()
                .next()
                .is_some_and(|family| family.eq_ignore_ascii_case("fable"))
        })
}

/// The windows a conversation's `rate_limit_event` reports.
///
/// `unifiedWindows` holds fractions of each window, which can run past one,
/// with Unix-second resets. The event is absent for API-key, Bedrock and
/// Vertex sessions and until the first response carrying the headers.
/// A rejection still carries authoritative exhaustion evidence when the
/// optional windows are absent. An empty allowed event cannot refresh quota.
///
/// The event's status describes the window its `rateLimitType` names. Only
/// when that is an account-wide limit, or unnamed, does it describe the
/// account: a rejection for one model's allowance leaves the others usable.
pub fn parse_rate_limit_event(info: &Value) -> Option<LiveUpdate> {
    let unified = &info["unifiedWindows"];
    let window =
        |live: &LiveWindow, used: f64, resets_at: Option<i64>, binding: bool| UsageWindow {
            key: live.key.to_owned(),
            kind: live.kind,
            label: live.label.to_owned(),
            used: super::fraction(used),
            resets_at,
            span_secs: Some(live.span_secs),
            binding,
        };
    let mut windows: Vec<UsageWindow> = LIVE_WINDOWS
        .iter()
        .filter_map(|live| {
            let reported = &unified[live.name];
            let used = reported["utilization"]
                .as_f64()
                .filter(|value| value.is_finite())?;
            Some(window(live, used, reported["resetsAt"].as_i64(), false))
        })
        .collect();
    let named = info["rateLimitType"]
        .as_str()
        .filter(|kind| !kind.is_empty());
    let live = named.and_then(|kind| LIVE_WINDOWS.iter().find(|live| live.name == kind));
    if windows.is_empty()
        && let Some(used) = info["utilization"]
            .as_f64()
            .filter(|value| value.is_finite())
        && let Some(live) = live
    {
        windows.push(window(live, used, info["resetsAt"].as_i64(), true));
    }
    // A report names the limit its status is about; only the account's own
    // windows decide the account's status.
    let account_wide = match named {
        None => true,
        Some(_) => live.is_some_and(|live| live.kind != WindowKind::Scoped),
    };
    let status = match info["status"].as_str() {
        _ if !account_wide => None,
        Some("rejected") => Some(UsageStatus::Limited),
        Some("allowed_warning") => Some(UsageStatus::Warning),
        Some("allowed") => Some(UsageStatus::Ok),
        _ => None,
    };
    // A model-specific report with no figures says nothing about the
    // account; wait for an account-wide quota report.
    if windows.is_empty() && !account_wide {
        return None;
    }
    if windows.is_empty() && status != Some(UsageStatus::Limited) {
        return None;
    }
    Some(LiveUpdate { windows, status })
}

/// Folds a live update into the snapshot a probe produced.
///
/// A live event reports only the account-wide windows and Fable's, so every
/// window it does not mention - another model's weekly allowance, most of
/// all - keeps its probed figure. Fable's window is the probe's, whatever
/// version of the name Claude scoped it to. The binding mark is dropped
/// because the event does not say which window binds; the fullest window
/// stands in until the next probe.
pub fn merge_live(snapshot: &mut UsageSnapshot, update: LiveUpdate, observed_at: u64) {
    snapshot
        .windows
        .retain(|window| window.key != RETIRED_FABLE);
    for window in update.windows {
        let fable = window.key == FABLE;
        match snapshot
            .windows
            .iter_mut()
            .find(|existing| existing.key == window.key || (fable && is_fable(existing)))
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

    fn login(email: &str, disputed_by: Option<&str>) -> Identity {
        Identity {
            email: Some(email.into()),
            disputed_by: disputed_by.map(str::to_owned),
            ..Identity::default()
        }
    }

    #[test]
    fn a_record_names_the_login_its_status_reports() {
        assert_eq!(
            RecordedLogin::reported(&json!({
                "loggedIn": true,
                "email": " other@example.com ",
                "subscriptionType": "max",
            })),
            RecordedLogin::Login("other@example.com".into())
        );
        assert_eq!(
            RecordedLogin::reported(&json!({"loggedIn": true})),
            RecordedLogin::Nobody
        );
        assert_eq!(
            RecordedLogin::reported(&json!({"loggedIn": false, "email": "x@example.com"})),
            RecordedLogin::Nobody
        );
    }

    #[test]
    #[cfg(any(feature = "ui", feature = "host"))]
    fn a_disagreement_is_reported_until_a_sign_in_is_known_to_have_settled_it() {
        use super::super::cache::Settled;
        const NOW: u64 = 1_000_000_000;
        let decide = |reported: &Identity,
                      other: RecordedLogin,
                      previous: Option<&Identity>,
                      settled: Option<&Settled>| {
            settle_dispute(reported, other, previous, settled, NOW)
        };
        let me = login("me@example.com", None);
        let other = || RecordedLogin::Login("other@example.com".to_owned());
        let shown = || Some("other@example.com".to_owned());
        let settled = Settled::Reported {
            reported: "me@example.com".into(),
            other: "other@example.com".into(),
        };
        // First seen: reported, and nothing recorded.
        assert_eq!(decide(&me, other(), None, None), (shown(), None));
        assert_eq!(
            decide(&me, other(), Some(&login("me@example.com", None)), None),
            (shown(), None)
        );
        // The same login, however it is spelled, is no disagreement.
        assert_eq!(
            decide(&login("Other@Example.com ", None), other(), None, None),
            (None, None)
        );
        // Seen again with neither record changed: still reported.
        let seen = login("me@example.com", Some("other@example.com"));
        assert_eq!(decide(&me, other(), Some(&seen), None), (shown(), None));
        // A sign-in cagents ran has just finished: its record wins.
        let signed_in = Settled::SignedIn { at: NOW - 30_000 };
        assert_eq!(
            decide(&me, other(), Some(&seen), Some(&signed_in)),
            (None, Some(Some(settled.clone())))
        );
        // One that finished long ago, whose probe never answered, settles
        // nothing now, and is forgotten.
        let long_ago = Settled::SignedIn {
            at: NOW - SIGN_IN_SETTLES_FOR - 1,
        };
        assert_eq!(
            decide(&me, other(), Some(&seen), Some(&long_ago)),
            (shown(), Some(None))
        );
        // Once settled, exactly that disagreement stays settled.
        assert_eq!(
            decide(&me, other(), Some(&me), Some(&settled)),
            (None, None)
        );
        // The CLI's record changed login while the other stayed: a sign-in
        // outside cagents rewrote it, so it is the newer one.
        let before = login("old@example.com", Some("other@example.com"));
        assert_eq!(
            decide(&me, other(), Some(&before), None),
            (None, Some(Some(settled.clone())))
        );
        let after_settling = Settled::Reported {
            reported: "old@example.com".into(),
            other: "other@example.com".into(),
        };
        assert_eq!(
            decide(
                &me,
                other(),
                Some(&login("old@example.com", None)),
                Some(&after_settling)
            ),
            (None, Some(Some(settled.clone())))
        );
        // The other record changed instead: it may be the newer one, and the
        // disagreement is shown again.
        assert_eq!(
            decide(
                &me,
                RecordedLogin::Login("third@example.com".into()),
                Some(&me),
                Some(&settled)
            ),
            (Some("third@example.com".into()), None)
        );
        // A record that cannot be read, or a probe naming no login, changes
        // nothing: the last report stands, and so does the settlement.
        assert_eq!(
            decide(&me, RecordedLogin::Unknown, Some(&seen), Some(&settled)),
            (shown(), None)
        );
        assert_eq!(
            decide(&me, RecordedLogin::Unknown, Some(&before), None),
            (None, None)
        );
        assert_eq!(
            decide(&Identity::default(), other(), Some(&seen), Some(&settled)),
            (None, None)
        );
        // Records that agree leave nothing to settle.
        assert_eq!(
            decide(&me, RecordedLogin::Nobody, Some(&seen), Some(&settled)),
            (None, Some(None))
        );
        assert_eq!(
            decide(
                &me,
                RecordedLogin::Login("me@example.com".into()),
                None,
                None
            ),
            (None, None)
        );
    }

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
        let scopes: Vec<_> = usage.windows.iter().map(window_scope).collect();
        assert_eq!(scopes, [None, None, Some("Opus")]);
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
        // Only what Claude scoped is scoped, whatever the label shows.
        let scopes: Vec<_> = usage.windows.iter().map(window_scope).collect();
        assert_eq!(scopes, [None, Some("Cowork"), None]);
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

    /// As Claude Code 2.1.296 reported a Fable 5.2 turn: the weekly window
    /// and `seven_day_overage_included`, which is Fable's weekly allowance.
    /// It moves the probed Fable window, as a used Fable reset does, rather
    /// than adding a second window that contradicts it.
    #[test]
    fn a_fable_turn_moves_the_probed_fable_window() {
        let update = parse_rate_limit_event(
            &fixture("claude-rate-limit-event-fable.json")["rate_limit_info"],
        )
        .expect("the event reports windows");
        let keys: Vec<_> = update.windows.iter().map(|w| w.key.as_str()).collect();
        assert_eq!(keys, [WEEKLY, FABLE]);
        assert_eq!(update.windows[1].kind, WindowKind::Scoped);
        assert_eq!(update.windows[1].label, "Weekly · Fable");
        assert_eq!(update.status, Some(UsageStatus::Ok));

        let mut usage = parse_usage(&fixture("claude-get-usage.json"), 10);
        assert!(
            (usage.windows[2].used - 1.0).abs() < 1e-6,
            "Fable was spent"
        );
        merge_live(&mut usage, update, 20);
        let keys: Vec<_> = usage.windows.iter().map(|w| w.key.as_str()).collect();
        assert_eq!(keys, [SESSION, WEEKLY, FABLE]);
        let fable = &usage.windows[2];
        assert_eq!(fable.used, 0.0, "the reset's figure replaces the spent one");
        assert_eq!(fable.resets_at, Some(1_794_758_400));
        assert!((usage.windows[1].used - 0.45).abs() < 1e-6);
        assert_eq!(usage.status, UsageStatus::Ok);
    }

    /// Claude has scoped the window to versions of the name, such as
    /// `Fable 5.1`; the live figure is still that window's.
    #[test]
    fn a_live_fable_figure_finds_a_versioned_fable_window() {
        let mut usage = parse_usage(
            &json!({"rate_limits": {"limits": [
                {"kind": "weekly_all", "group": "weekly", "percent": 40},
                {"kind": "weekly_scoped", "group": "weekly", "percent": 70,
                    "scope": {"model": {"display_name": "Fable 5.1", "id": null}}},
                {"kind": "weekly_scoped", "group": "weekly", "percent": 30,
                    "scope": {"model": {"display_name": "Opus", "id": null}}},
            ]}}),
            10,
        );
        let update = parse_rate_limit_event(&json!({
            "status": "allowed",
            "unifiedWindows": {"seven_day_overage_included": {"utilization": 0.1, "resetsAt": 50}},
        }))
        .unwrap();
        merge_live(&mut usage, update, 20);
        let figures: Vec<_> = usage
            .windows
            .iter()
            .map(|w| (w.key.as_str(), (w.used * 100.0).round() as u32))
            .collect();
        assert_eq!(
            figures,
            [
                ("weekly_all", 40),
                ("weekly_scoped:Fable 5.1", 10),
                ("weekly_scoped:Opus", 30)
            ]
        );
    }

    /// A reading cached by an earlier build holds the live Fable window under
    /// its retired key and label. The next live report retires it, so an
    /// exhausted Fable allowance stops reading as an exhausted account.
    #[test]
    fn a_live_report_retires_the_old_fable_window() {
        let mut usage = parse_usage(&fixture("claude-get-usage.json"), 10);
        usage.windows.push(UsageWindow {
            key: RETIRED_FABLE.into(),
            kind: WindowKind::Other,
            label: "Weekly incl. extra usage".into(),
            used: 1.0,
            resets_at: Some(1_790_186_400),
            span_secs: Some(SEVEN_DAYS),
            binding: false,
        });
        let update = parse_rate_limit_event(&json!({
            "status": "allowed",
            "unifiedWindows": {"five_hour": {"utilization": 0.3, "resetsAt": 1_790_119_800_i64}},
        }))
        .unwrap();
        merge_live(&mut usage, update, 20);
        assert!(usage.windows.iter().all(|w| w.key != RETIRED_FABLE));
        assert_ne!(usage.status, UsageStatus::Limited);
    }

    /// Reaching the Fable limit leaves every other model usable: the window
    /// is full and binds, but the account is neither limited nor rotated
    /// away from, whatever the rejection says about the Fable request.
    #[test]
    fn a_fable_rejection_spends_fable_but_not_the_account() {
        let now = 10_000;
        let update = parse_rate_limit_event(&json!({
            "status": "rejected",
            "rateLimitType": "seven_day_overage_included",
            "utilization": 1.0,
            "resetsAt": 1_794_758_400_i64,
        }))
        .expect("the rejection reports Fable's window");
        assert_eq!(update.status, None);
        assert_eq!(update.windows.len(), 1);
        assert_eq!(update.windows[0].key, FABLE);
        assert!(update.windows[0].binding);
        let mut usage = parse_usage(
            &json!({"rate_limits": {"limits": [
                {"kind": "weekly_all", "group": "weekly", "percent": 40},
                {"kind": "weekly_scoped", "group": "weekly", "percent": 90,
                    "scope": {"model": {"display_name": "Fable", "id": null}}},
            ]}}),
            now,
        );
        merge_live(&mut usage, update, now);
        assert_eq!(usage.windows.len(), 2);
        assert_eq!(usage.windows[1].used, 1.0);
        assert_eq!(usage.status, UsageStatus::Warning);
        assert!(super::super::routing::available(&usage, now));
        assert!(!super::super::routing::needs_alternate(&usage, now));

        // The same holds for any model's rejection that arrives with the
        // account's own windows: those decide the account's status.
        let opus = parse_rate_limit_event(&json!({
            "status": "rejected",
            "rateLimitType": "seven_day_opus",
            "unifiedWindows": {"five_hour": {"utilization": 0.2, "resetsAt": 50}},
        }))
        .unwrap();
        assert_eq!(opus.status, None);
        // An account-wide rejection still limits the account.
        let weekly = parse_rate_limit_event(&json!({
            "status": "rejected",
            "rateLimitType": "seven_day",
            "unifiedWindows": {"seven_day": {"utilization": 1.0, "resetsAt": 50}},
        }))
        .unwrap();
        assert_eq!(weekly.status, Some(UsageStatus::Limited));
        // A rejection of a limit with no figures here, such as extra usage,
        // waits for an account-wide report, as it did before.
        assert!(
            parse_rate_limit_event(&json!({"status": "rejected", "rateLimitType": "overage"}))
                .is_none()
        );
    }

    #[test]
    fn sparse_or_malformed_probe_limits_do_not_authorize_routing() {
        let now = 1_000;
        for response in [
            json!(null),
            json!({}),
            json!({"rate_limits": null}),
            json!({"rate_limits": []}),
            json!({"rate_limits": {}}),
            json!({"rate_limits": {"limits": []}}),
            json!({"rate_limits": {"limits": "unavailable"}}),
            json!({"rate_limits": {"limits": [null, {},
                {"kind": "session", "percent": null},
                {"kind": "session", "percent": "0"},
                {"kind": "session", "percent": false},
                {"kind": "session", "percent": []},
                {"kind": "session", "percent": {}},
            ]}}),
            json!({"rate_limits": {"limits": [
                {"percent": 0}, {"kind": null, "percent": 0},
                {"kind": 1, "percent": 0}, {"kind": "", "percent": 0},
                {"kind": "  ", "percent": 0},
            ]}}),
            json!({"rate_limits": {
                "five_hour": {"utilization": null},
                "seven_day": {"utilization": "0"},
                "model_scoped": [{"display_name": "Opus", "utilization": false}],
            }}),
            json!({"rate_limits": {"future_internal_name": {"utilization": 0}}}),
        ] {
            let usage = parse_usage(&response, now);
            assert_eq!(usage.status, UsageStatus::Unavailable, "{response}");
            assert!(usage.windows.is_empty(), "{response}");
            assert!(usage.detail.is_some(), "{response}");
            assert_eq!(usage.observed_at, now);
            assert_eq!(usage.source, UsageSource::Probe);
            assert!(!super::super::routing::available(&usage, now), "{response}");
            assert!(
                !super::super::routing::needs_alternate(&usage, now),
                "unknown quota is not an exhaustion or sign-in rejection: {response}"
            );
        }
    }

    #[test]
    fn probe_metadata_is_retained_without_inventing_quota_permission() {
        let mut response = json!({"subscription_type": "max", "rate_limits": {
            "limits": [],
            "spend": {"enabled": true, "percent": 25,
                "used": {"amount_minor": 125, "currency": "USD", "exponent": 2},
                "limit": {"amount_minor": 500, "currency": "USD", "exponent": 2}},
        }});
        let unknown = parse_usage(&response, 1_000);
        assert_eq!(unknown.plan.as_deref(), Some("max"));
        assert_eq!(
            unknown.credits.as_ref().unwrap().summary(),
            "$1.25 of $5.00 used"
        );
        assert_eq!(unknown.status, UsageStatus::Unavailable);
        assert!(!super::super::routing::available(&unknown, 1_000));
        for (percent, status, available) in [
            (0, UsageStatus::Ok, true),
            (81, UsageStatus::Warning, true),
            (100, UsageStatus::Limited, false),
        ] {
            response["rate_limits"]["limits"] = json!([
                {"kind": "session", "percent": percent},
            ]);
            let reported = parse_usage(&response, 1_001);
            assert_eq!(reported.status, status);
            assert_eq!(reported.windows.len(), 1);
            assert_eq!(reported.plan, unknown.plan);
            assert_eq!(reported.credits, unknown.credits);
            assert_eq!(
                super::super::routing::available(&reported, 1_001),
                available
            );
        }
        response["rate_limits"]["limits"] = json!([
            {"kind": "future_daily_burst", "group": "session", "percent": 5},
        ]);
        let future = parse_usage(&response, 1_002);
        assert_eq!(future.windows[0].key, "future_daily_burst");
        assert!(super::super::routing::available(&future, 1_002));
    }
}

#[cfg(test)]
mod reset_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn reset_inventory_keeps_counts_scope_expiry_and_next_grant() {
        let resets = parse_resets(
            &json!({"cedar_ember": {"eligible":true,"at_limit":true,
            "next_grant_id":"fable", "grants":[
                {"id":"fable","resets_left":2,"clears":["seven_day_overage_included"],"usable_now":true,"ends_at":"2030-01-01T00:00:00Z"},
                {"id":"weekly","resets_left":1,"clears":["seven_day","five_hour"],"usable_now":true},
                {"id":"bad/id","resets_left":99,"clears":["seven_day"],"usable_now":true}
            ]},"juniper_tide":{"eligible":true,"arm":"reset","available":true,"resets_per_week":5}}),
            1_000,
        );
        assert_eq!(resets.len(), 3);
        assert_eq!(resets[0].available_count, 2);
        assert_eq!(
            resets[0].scopes,
            [super::super::ResetScope::Model("Fable".into())]
        );
        assert_eq!(resets[0].expires_at, Some(1_893_456_000));
        assert!(resets[0].is_available(1_000));
        assert!(!resets[1].is_available(1_000));
        assert_eq!(
            resets[2].available_count, 1,
            "weekly capacity is not current credit count"
        );
    }

    #[test]
    fn grant_labels_never_authorize_an_unknown_scope_or_paused_grant() {
        let resets = parse_resets(
            &json!({"cedar_ember":{"eligible":true,"at_limit":true,"next_grant_id":"grant",
            "grants":[{"id":"grant","label":"Reset all limits","resets_left":2,"clears":["unknown"],"usable_now":true,"paused":true}]}}),
            1,
        );
        assert_eq!(resets.len(), 1);
        assert!(!resets[0].is_available(1));
        assert!(resets[0].scopes.is_empty());
        assert!(
            parse_resets(
                &json!({"cedar_ember":null,"juniper_tide":{"arm":"control","available":true}}),
                1
            )
            .is_empty()
        );
    }
}

#[cfg(test)]
mod reset_timestamp_tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn malformed_grant_timestamps_never_authorize_spending() {
        for field in ["starts_at", "ends_at"] {
            for bad in [json!("tomorrow"), json!(12), json!({})] {
                let mut grant = json!({"id":"grant","resets_left":1,"clears":["seven_day"],"usable_now":true,"use_requires_limit":false});
                grant[field] = bad;
                let resets = parse_resets(
                    &json!({"cedar_ember":{"eligible":true,"next_grant_id":"grant","grants":[grant]}}),
                    100,
                );
                assert_eq!(resets[0].available_count, 1);
                assert!(!resets[0].is_available(100));
            }
        }
    }
}
