//! Bounded daemon probes for automatic account admission and exhaustion.
use super::{
    Account,
    cache::{Cache, Entry},
    home::Homes,
    probe,
    registry::Registry,
    routing,
};
use std::collections::BTreeMap;

pub const ON_DEMAND_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(12);

/// Refresh stale candidates within one total deadline. Only successful provider
/// evidence enters selection; timeouts and login conflicts cannot authorize work.
pub async fn refresh(
    registry: &Registry,
    homes: Option<&Homes>,
    preferred: &Account,
    cache: Option<Cache>,
    entries: &mut BTreeMap<String, Entry>,
    now: u64,
) {
    refresh_with(
        registry,
        homes,
        preferred,
        cache.clone(),
        entries,
        now,
        |account| probe::run(account, cache.clone(), true),
    )
    .await;
}

async fn refresh_with<F, Fut>(
    registry: &Registry,
    homes: Option<&Homes>,
    preferred: &Account,
    cache: Option<Cache>,
    entries: &mut BTreeMap<String, Entry>,
    now: u64,
    mut probe_account: F,
) where
    F: FnMut(Account) -> Fut,
    Fut: std::future::Future<Output = probe::Outcome>,
{
    let deadline = tokio::time::Instant::now() + ON_DEMAND_TIMEOUT;
    for account in registry.accounts(homes).into_iter().filter(|candidate| {
        candidate.provider == preferred.provider && candidate.id() != preferred.id()
    }) {
        let Some(key) = account.cache_key() else {
            continue;
        };
        if entries
            .get(&key)
            .is_some_and(|entry| routing::fresh(&entry.snapshot, now))
        {
            continue;
        }
        loop {
            match tokio::time::timeout_at(deadline, probe_account(account.clone())).await {
                Ok(probe::Outcome::Answered {
                    identity, snapshot, ..
                }) => {
                    let entry = Entry { identity, snapshot };
                    if let Some(cache) = &cache {
                        let _ = cache.store(&key, entry.clone());
                    }
                    entries.insert(key.clone(), entry);
                    break;
                }
                Ok(probe::Outcome::Busy) => {
                    // A focused window or another admission already owns the
                    // provider probe. Wait for its shared evidence; never call
                    // an account unavailable simply because it is being read.
                    if let Some(entry) = cache
                        .as_ref()
                        .and_then(|cache| cache.load(now).remove(&key))
                        .filter(|entry| routing::fresh(&entry.snapshot, now))
                    {
                        entries.insert(key.clone(), entry);
                        break;
                    }
                    if tokio::time::Instant::now() >= deadline {
                        return;
                    }
                    tokio::time::sleep_until(
                        (tokio::time::Instant::now() + std::time::Duration::from_millis(100))
                            .min(deadline),
                    )
                    .await;
                }
                Err(_) => return,
                _ => break,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn exhaustion_refreshes_stale_alternate_then_selects_it() {
        let root = tempfile::tempdir().unwrap();
        let homes = Homes::at(root.path());
        let provider = crate::agent::Provider::Claude;
        let key = uuid::Uuid::new_v4();
        let target = Account::managed(provider, key, homes.path(provider, key));
        let source = Account::system(provider);
        let mut registry = Registry::default();
        registry
            .register(
                crate::account::registry::Registered {
                    key,
                    provider,
                    linked_home: None,
                    label: None,
                    email: Some("fixture@example.invalid".into()),
                    organization: None,
                    added_at: 0,
                },
                &[],
            )
            .unwrap();
        let now = 5_000_000;
        let mut entries = BTreeMap::from([
            (
                source.cache_key().unwrap(),
                Entry {
                    identity: None,
                    snapshot: crate::account::UsageSnapshot::state(
                        crate::account::UsageStatus::Ok,
                        "",
                        0,
                    ),
                },
            ),
            (
                target.cache_key().unwrap(),
                Entry {
                    identity: None,
                    snapshot: crate::account::UsageSnapshot::state(
                        crate::account::UsageStatus::Ok,
                        "",
                        0,
                    ),
                },
            ),
        ]);
        let live = crate::account::LiveReport::Claude(crate::account::claude::LiveUpdate {
            windows: Vec::new(),
            status: Some(crate::account::UsageStatus::Limited),
        });
        assert!(
            live.apply(
                &mut entries
                    .get_mut(&source.cache_key().unwrap())
                    .unwrap()
                    .snapshot,
                now,
                now
            )
        );
        assert_eq!(
            routing::select_cached(&registry, Some(&homes), source.clone(), &entries, now).id(),
            source.id()
        );
        let mut probed = Vec::new();
        let mut busy = true;
        refresh_with(
            &registry,
            Some(&homes),
            &source,
            None,
            &mut entries,
            now,
            |account| {
                probed.push(account.id());
                if std::mem::take(&mut busy) {
                    return std::future::ready(probe::Outcome::Busy);
                }
                std::future::ready(probe::Outcome::Answered {
                    identity: Some(crate::account::Identity {
                        email: Some("fixture@example.invalid".into()),
                        ..Default::default()
                    }),
                    snapshot: crate::account::UsageSnapshot::state(
                        crate::account::UsageStatus::Ok,
                        "",
                        now,
                    ),
                    catalog: None,
                })
            },
        )
        .await;
        assert_eq!(probed, [target.id(), target.id()]);
        assert_eq!(
            routing::select_cached(&registry, Some(&homes), source, &entries, now).id(),
            target.id()
        );
    }
}
