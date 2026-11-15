//! Automatic subscription routing uses provider-reported quota, never credentials.
use super::{UsageSnapshot, UsageStatus, WindowKind, schedule};

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
    fresh(snapshot, now)
        && matches!(snapshot.status, UsageStatus::Ok | UsageStatus::Warning)
        && match snapshot.ordinary_usage_allowed {
            Some(allowed) => allowed,
            None => {
                // Old cache entries can say Ok without ever having contained
                // quota. A model-only allowance does not establish ordinary
                // account usage; use the same account-wide kinds as status_for.
                snapshot.windows.iter().any(|window| {
                    window.kind != WindowKind::Scoped
                        && window.used.is_finite()
                        && (0.0..1.0).contains(&window.used)
                }) && super::status_for(&snapshot.windows, false, false) != UsageStatus::Limited
            }
        }
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

fn fresh(snapshot: &UsageSnapshot, now: u64) -> bool {
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
    use crate::account::{UsageWindow, claude, codex};
    use serde_json::json;

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
        assert!(!available(
            &UsageSnapshot::state(UsageStatus::Ok, "", now),
            now
        ));
        let mut quota = codex::parse_rate_limits(&json!({"ordinaryUsageAllowed": true}), now);
        assert!(available(&quota, now));
        assert!(!available(&quota, now + schedule::DISPLAY_STALE_AFTER + 1));
        quota.observed_at = now + schedule::CLOCK_SLACK + 1;
        assert!(!available(&quota, now));
    }

    #[test]
    fn ordinary_evidence_preserves_provider_permission_and_model_scope() {
        let now = 2_000_000;
        for allowed in [false, true] {
            let mut permission =
                codex::parse_rate_limits(&json!({"ordinaryUsageAllowed": allowed}), now);
            assert!(permission.windows.is_empty());
            assert_eq!(permission.ordinary_usage_allowed, Some(allowed));
            assert_eq!(available(&permission, now), allowed);
            // A historical contradictory status cannot erase an explicit denial.
            permission.status = UsageStatus::Ok;
            assert_eq!(available(&permission, now), allowed);
        }
        let codex_permission = codex::parse_rate_limits(
            &json!({"ordinaryUsageAllowed": true, "rateLimits": {
                "primary": {"usedPercent": 100, "windowDurationMins": 300}
            }}),
            now,
        );
        assert_eq!(codex_permission.status, UsageStatus::Warning);
        assert!(available(&codex_permission, now));

        for scoped_used in [20, 100] {
            let scoped = claude::parse_usage(
                &json!({"rate_limits": {"limits": [{
                    "kind": "weekly_scoped", "scope": {"model": {"id": "one-model"}},
                    "percent": scoped_used
                }]}}),
                now,
            );
            assert_eq!(scoped.windows.len(), 1);
            assert_eq!(scoped.windows[0].kind, WindowKind::Scoped);
            assert!(!available(&scoped, now));
            let ordinary = claude::parse_usage(
                &json!({"rate_limits": {"limits": [
                    {"kind": "session", "percent": 20},
                    {"kind": "weekly_scoped", "percent": scoped_used}
                ]}}),
                now,
            );
            assert!(available(&ordinary, now));
        }
        let scoped_codex = codex::parse_rate_limits(
            &json!({"rateLimits": {"limitId": "one-model", "limitName": "One model",
                "primary": {"usedPercent": 20, "windowDurationMins": 10080}
            }}),
            now,
        );
        assert_eq!(scoped_codex.windows[0].kind, WindowKind::Scoped);
        assert!(!available(&scoped_codex, now));

        let mut exhausted = claude::parse_usage(
            &json!({"rate_limits": {"limits": [
                {"kind": "session", "percent": 100},
                {"kind": "weekly_all", "percent": 20}
            ]}}),
            now,
        );
        assert_eq!(exhausted.status, UsageStatus::Limited);
        exhausted.status = UsageStatus::Ok;
        assert!(!available(&exhausted, now));
    }

    #[cfg(all(unix, any(feature = "ui", feature = "host")))]
    #[test]
    fn legacy_and_parser_cache_entries_need_evidence_before_automatic_selection() {
        use crate::{
            account::{Account, AccountHome, Identity, cache, registry},
            agent::Provider,
        };
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::Builder::new()
            .permissions(std::fs::Permissions::from_mode(0o700))
            .tempdir_in("/var/tmp")
            .unwrap();
        let cache = cache::Cache::at(directory.path().join("usage-cache"));
        let now = 2_000_000;
        for provider in [Provider::Claude, Provider::Codex] {
            let preferred_home = directory
                .path()
                .join(provider.executable())
                .join("preferred");
            let alternate_home = directory
                .path()
                .join(provider.executable())
                .join("alternate");
            std::fs::create_dir_all(&preferred_home).unwrap();
            std::fs::create_dir_all(&alternate_home).unwrap();
            // Explicit fixture homes avoid consulting the caller's System home.
            let preferred = Account {
                provider,
                home: AccountHome::SystemAt {
                    path: preferred_home,
                },
            };
            let key = uuid::Uuid::from_u128(1938);
            let identity = Identity {
                email: Some("routing-fixture@example.test".into()),
                ..Identity::default()
            };
            let mut registry = registry::Registry::default();
            registry
                .register(
                    registry::Registered {
                        key,
                        provider,
                        linked_home: Some(alternate_home.clone()),
                        label: None,
                        email: identity.email.clone(),
                        organization: None,
                        added_at: now,
                    },
                    &[],
                )
                .unwrap();
            let alternate = Account::linked(provider, key, alternate_home);
            cache
                .store(
                    &preferred.cache_key().unwrap(),
                    cache::Entry {
                        identity: None,
                        snapshot: UsageSnapshot::state(UsageStatus::Limited, "exhausted", now),
                    },
                )
                .unwrap();
            // The old serialized shape has no permission field. It remains
            // readable display data, but an empty Ok is not an admission grant.
            let legacy: UsageSnapshot = serde_json::from_value(json!({
                "windows": [], "credits": null, "status": "Ok", "observed_at": now,
                "source": "Cache", "plan": null, "detail": "legacy missing quota"
            }))
            .unwrap();
            assert_eq!(legacy.ordinary_usage_allowed, None);
            let (malformed, positive) = match provider {
                Provider::Claude => (
                    claude::parse_usage(
                        &json!({"rate_limits": {"limits": [
                            {"kind": "session", "percent": "not-a-number"}
                        ]}}),
                        now,
                    ),
                    claude::parse_usage(
                        &json!({"rate_limits": {"limits": [
                            {"kind": "session", "percent": 20}
                        ]}}),
                        now,
                    ),
                ),
                Provider::Codex => (
                    codex::parse_rate_limits(
                        &json!({"rateLimits": {
                            "primary": {"usedPercent": "not-a-number"}
                        }}),
                        now,
                    ),
                    codex::parse_rate_limits(
                        &json!({"rateLimits": {
                            "primary": {"usedPercent": 20, "windowDurationMins": 300}
                        }}),
                        now,
                    ),
                ),
                _ => unreachable!(),
            };
            assert!(malformed.windows.is_empty());
            assert!(!positive.windows.is_empty());
            let mut denied = codex::parse_rate_limits(&json!({"ordinaryUsageAllowed": false}), now);
            denied.status = UsageStatus::Ok;
            let allowed = codex::parse_rate_limits(&json!({"ordinaryUsageAllowed": true}), now);
            let mut cases = vec![(legacy, false), (malformed, false), (positive, true)];
            if provider == Provider::Codex {
                cases.extend([(denied, false), (allowed, true)]);
            }
            for (snapshot, expected) in cases {
                cache
                    .store(
                        &alternate.cache_key().unwrap(),
                        cache::Entry {
                            identity: Some(identity.clone()),
                            snapshot: snapshot.clone(),
                        },
                    )
                    .unwrap();
                let entries = cache.load(now);
                let restored = &entries[&alternate.cache_key().unwrap()].snapshot;
                assert_eq!(restored, &snapshot);
                assert_eq!(available(restored, now), expected);
                let selected = select_cached(&registry, None, preferred.clone(), &entries, now);
                assert_eq!(
                    selected,
                    if expected {
                        alternate.clone()
                    } else {
                        preferred.clone()
                    }
                );
                assert_eq!(
                    cache.load(now),
                    entries,
                    "routing must not rewrite cache evidence"
                );
            }
        }
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
