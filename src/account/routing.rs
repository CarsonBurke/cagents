//! Automatic subscription routing uses provider-reported quota, never credentials.
use super::{UsageSnapshot, UsageStatus, schedule};

/// Persisted with a native launch. Its absence pins legacy and explicitly
/// selected sessions; only new automatically routed chats carry this marker.
pub const AUTOMATIC_MARKER: &str = "CAGENTS_AUTO_ACCOUNT";

/// A recently reported exhausted account cannot admit another turn. A reset
/// invalidates the observation; it does not prove a fresh allowance.
pub fn exhausted(snapshot: &UsageSnapshot, now: u64) -> bool {
    fresh(snapshot, now) && snapshot.status == UsageStatus::Limited
}

pub fn needs_alternate(snapshot: &UsageSnapshot, now: u64) -> bool {
    fresh(snapshot, now)
        && matches!(
            snapshot.status,
            UsageStatus::Limited | UsageStatus::NeedsLogin
        )
}

/// A missing system home is distinct from an unknown provider failure: cagents
/// positively observed that this login was never configured. This observation
/// travels with remote snapshots, so clients must not inspect their own disk.
#[cfg(any(feature = "ui", feature = "host"))]
pub fn needs_alternate_for(account: &super::Account, snapshot: &UsageSnapshot, now: u64) -> bool {
    needs_alternate(snapshot, now)
        || (account.is_system()
            && fresh(snapshot, now)
            && snapshot.status == UsageStatus::Unavailable
            && snapshot.detail.as_deref()
                == Some(
                    format!(
                        "{} has not been set up",
                        super::probe::cli_name(account.provider)
                    )
                    .as_str(),
                ))
}

/// Only positive, current provider evidence can authorize an alternate login.
pub fn available(snapshot: &UsageSnapshot, now: u64) -> bool {
    fresh(snapshot, now) && matches!(snapshot.status, UsageStatus::Ok | UsageStatus::Warning)
}

/// Daemon-side defaults use the same provider evidence as windows. Explicit
/// CLI `--account` requests bypass this function and remain concrete choices.
#[cfg(any(feature = "ui", feature = "host"))]
pub fn select_cached(
    registry: &super::registry::Registry,
    homes: Option<&super::home::Homes>,
    preferred: super::Account,
    entries: &std::collections::BTreeMap<String, super::cache::Entry>,
    now: u64,
) -> super::Account {
    let entry = |account: &super::Account| account.cache_key().and_then(|key| entries.get(&key));
    if !entry(&preferred).is_some_and(|entry| needs_alternate_for(&preferred, &entry.snapshot, now))
    {
        return preferred;
    }
    registry
        .accounts(homes)
        .into_iter()
        .filter(|candidate| {
            candidate.provider == preferred.provider && candidate.id() != preferred.id()
        })
        .filter(|candidate| {
            entry(candidate).is_some_and(|entry| {
                available(&entry.snapshot, now)
                    && candidate
                        .registry_key()
                        .and_then(|key| registry.find(key))
                        .is_none_or(|registered| {
                            entry
                                .identity
                                .as_ref()
                                .is_some_and(|identity| !registered.conflicts_with(identity))
                        })
            })
        })
        .max_by(|left, right| {
            let allowance = |account: &super::Account| {
                entry(account)
                    .and_then(|entry| entry.snapshot.binding_window())
                    .map_or(1.0, super::UsageWindow::left)
            };
            allowance(left)
                .total_cmp(&allowance(right))
                .then_with(|| right.id().cmp(&left.id()))
        })
        .unwrap_or(preferred)
}

pub(super) fn fresh(snapshot: &UsageSnapshot, now: u64) -> bool {
    snapshot.observed_at <= now.saturating_add(schedule::CLOCK_SLACK)
        && now.saturating_sub(snapshot.observed_at) <= schedule::DISPLAY_STALE_AFTER
        && !snapshot.windows.iter().any(|window| {
            window
                .resets_at
                .and_then(|reset| u64::try_from(reset).ok())
                .map(|reset| reset.saturating_mul(1_000))
                .is_some_and(|reset| reset > snapshot.observed_at && reset <= now)
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account::{UsageWindow, WindowKind};

    #[test]
    fn unknown_login_and_stale_or_future_quotas_never_authorize_an_alternate() {
        let now = 2_000_000;
        for status in [
            UsageStatus::NeedsLogin,
            UsageStatus::Unavailable,
            UsageStatus::Limited,
        ] {
            assert!(!available(&UsageSnapshot::state(status, "", now), now));
        }
        let mut quota = UsageSnapshot::state(UsageStatus::Ok, "", now);
        assert!(available(&quota, now));
        assert!(!available(&quota, now + schedule::DISPLAY_STALE_AFTER + 1));
        quota.observed_at = now + schedule::CLOCK_SLACK + 1;
        assert!(!available(&quota, now));
    }

    #[cfg(any(feature = "ui", feature = "host"))]
    #[test]
    fn unconfigured_system_can_route_but_generic_unavailable_cannot() {
        let account = super::super::Account::system(crate::agent::Provider::Codex);
        let now = 10_000;
        let snapshot =
            UsageSnapshot::state(UsageStatus::Unavailable, "Codex has not been set up", now);
        assert!(needs_alternate_for(&account, &snapshot, now));
        assert!(!needs_alternate_for(
            &account,
            &snapshot,
            now + schedule::DISPLAY_STALE_AFTER + 1
        ));
        let unknown = UsageSnapshot::state(UsageStatus::Unavailable, "provider unavailable", now);
        assert!(!needs_alternate_for(&account, &unknown, now));
    }

    #[test]
    fn a_reset_requires_new_provider_evidence() {
        let mut quota = UsageSnapshot::state(UsageStatus::Limited, "", 10_000);
        quota.windows.push(UsageWindow {
            key: "session".into(),
            kind: WindowKind::Session,
            label: "Session".into(),
            used: 1.0,
            resets_at: Some(20),
            span_secs: None,
            binding: true,
        });
        assert!(exhausted(&quota, 19_999));
        assert!(!exhausted(&quota, 20_000));
        quota.status = UsageStatus::Ok;
        assert!(!available(&quota, 20_000));
    }
}
