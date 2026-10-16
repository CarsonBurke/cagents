//! Account preferences name current daemon-owned accounts, never their homes.
use super::AccountId;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DefaultSelection {
    Automatic,
    Account(AccountId),
}

impl std::str::FromStr for DefaultSelection {
    type Err = String;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value.eq_ignore_ascii_case("automatic") {
            Ok(Self::Automatic)
        } else {
            value.parse().map(Self::Account)
        }
    }
}

/// Resolves and validates under the same registry mutation lease as the write.
/// Cached observations can refuse a known conflict; absent observations never
/// claim that the login was verified and never trigger a provider probe.
#[cfg(feature = "host")]
pub(crate) fn set_default(
    store: &super::registry::Store,
    homes: Option<&super::home::Homes>,
    cached: &std::collections::BTreeMap<String, super::cache::Entry>,
    expected_revision: u64,
    provider: crate::agent::Provider,
    selection: &DefaultSelection,
) -> Result<super::registry::Registry, String> {
    let (registry, result) = store.try_update(|registry| {
        if registry.revision() != expected_revision {
            return Err("accounts changed on the device; reload before saving".to_owned());
        }
        let DefaultSelection::Account(id) = selection else {
            registry.set_automatic(provider);
            return Ok(());
        };
        if provider == crate::agent::Provider::Omp {
            return Err("OMP chooses its own account; use automatic".into());
        }
        let accounts = registry.accounts(homes);
        let account = accounts.iter().find(|account| account.id() == *id)
            .ok_or("No active configured account matches that ID")?;
        if account.provider != provider {
            return Err("The account belongs to a different provider".into());
        }
        let identity = |account: &super::Account| {
            account.cache_key().and_then(|key| cached.get(&key)).and_then(|entry| entry.identity.as_ref())
        };
        let current = identity(account);
        if account.registry_key().and_then(|key| registry.find(key))
            .zip(current).is_some_and(|(registered, current)| registered.conflicts_with(current)) {
            return Err("The account reports a different login; review it in Accounts before choosing a default".into());
        }
        let home = account.resolved_home();
        for other in accounts.iter().filter(|other| other.provider == provider && other.id() != *id) {
            if home.is_some() && other.resolved_home() == home {
                return Err("Another configured account uses the same home; review it in Accounts".into());
            }
            if current.zip(identity(other)).is_some_and(|(mine, theirs)| mine.same_login(theirs)) {
                return Err("Another configured account reports the same login; review it in Accounts".into());
            }
        }
        // Every fallible selection/conflict check precedes mutation.
        registry.set_default(account);
        Ok(())
    }).map_err(|error| error.to_string())?;
    result?;
    Ok(registry)
}

#[cfg(all(test, feature = "host"))]
mod tests {
    use super::*;
    use crate::{
        account::{
            Account, Identity, UsageSnapshot, UsageStatus,
            cache::Entry,
            home::Homes,
            registry::{Registered, Store},
        },
        agent::Provider,
    };
    use std::{collections::BTreeMap, fs};
    use uuid::Uuid;

    fn registered(key: Uuid, email: &str) -> Registered {
        Registered {
            key,
            provider: Provider::Claude,
            linked_home: None,
            label: None,
            email: Some(email.into()),
            organization: None,
            added_at: 1,
        }
    }

    #[test]
    fn default_preference_persists_and_automatic_is_distinct_from_system() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::at(directory.path());
        let homes = Homes::at(directory.path().join("homes"));
        let key = Uuid::new_v4();
        let (original, ()) = store
            .update(|registry| {
                registry
                    .register(registered(key, "one@example.com"), &[])
                    .unwrap();
            })
            .unwrap();
        let account = Account::managed(Provider::Claude, key, homes.path(Provider::Claude, key));
        let saved = set_default(
            &store,
            Some(&homes),
            &BTreeMap::new(),
            original.revision(),
            Provider::Claude,
            &DefaultSelection::Account(account.id()),
        )
        .unwrap();
        assert_eq!(
            store
                .load()
                .unwrap()
                .default_account(Provider::Claude, Some(&homes))
                .id(),
            account.id()
        );
        let system = set_default(
            &store,
            Some(&homes),
            &BTreeMap::new(),
            saved.revision(),
            Provider::Claude,
            &DefaultSelection::Account(Account::system(Provider::Claude).id()),
        )
        .unwrap();
        assert!(!system.automatic(Provider::Claude));
        let automatic = set_default(
            &store,
            Some(&homes),
            &BTreeMap::new(),
            system.revision(),
            Provider::Claude,
            &DefaultSelection::Automatic,
        )
        .unwrap();
        assert!(automatic.automatic(Provider::Claude));
        assert_eq!(automatic.registered(), original.registered());
        assert_eq!(store.load().unwrap(), automatic);
    }

    #[test]
    fn rejected_default_intents_leave_the_original_registry_bytes_unchanged() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::at(directory.path());
        let homes = Homes::at(directory.path().join("homes"));
        let key = Uuid::new_v4();
        let (registry, ()) = store
            .update(|registry| {
                registry
                    .register(registered(key, "one@example.com"), &[])
                    .unwrap();
            })
            .unwrap();
        let before = fs::read(directory.path().join("accounts.toml")).unwrap();
        for (revision, provider, selection, reason) in [
            (
                registry.revision() - 1,
                Provider::Claude,
                DefaultSelection::Automatic,
                "changed",
            ),
            (
                registry.revision(),
                Provider::Codex,
                DefaultSelection::Account(Account::system(Provider::Claude).id()),
                "different provider",
            ),
            (
                registry.revision(),
                Provider::Claude,
                DefaultSelection::Account(
                    Account::managed(Provider::Claude, Uuid::new_v4(), "/unused".into()).id(),
                ),
                "No active",
            ),
            (
                registry.revision(),
                Provider::Omp,
                DefaultSelection::Account(Account::system(Provider::Omp).id()),
                "OMP chooses",
            ),
        ] {
            let error = set_default(
                &store,
                Some(&homes),
                &BTreeMap::new(),
                revision,
                provider,
                &selection,
            )
            .unwrap_err();
            assert!(error.contains(reason), "{error}");
            assert_eq!(
                fs::read(directory.path().join("accounts.toml")).unwrap(),
                before
            );
        }
        let (archived, ()) = store
            .update(|registry| {
                assert!(registry.archive_duplicate(key, &Account::system(Provider::Claude)));
            })
            .unwrap();
        let before = fs::read(directory.path().join("accounts.toml")).unwrap();
        let hidden = Account::managed(Provider::Claude, key, homes.path(Provider::Claude, key));
        assert!(
            set_default(
                &store,
                Some(&homes),
                &BTreeMap::new(),
                archived.revision(),
                Provider::Claude,
                &DefaultSelection::Account(hidden.id())
            )
            .unwrap_err()
            .contains("No active")
        );
        assert_eq!(
            fs::read(directory.path().join("accounts.toml")).unwrap(),
            before
        );
    }

    #[test]
    fn cached_changed_and_duplicate_logins_refuse_without_changing_preferences() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::at(directory.path());
        let homes = Homes::at(directory.path().join("homes"));
        let keys = [Uuid::new_v4(), Uuid::new_v4()];
        let (registry, ()) = store
            .update(|registry| {
                for (key, email) in keys.into_iter().zip(["one@example.com", "two@example.com"]) {
                    registry.register(registered(key, email), &[]).unwrap();
                }
            })
            .unwrap();
        let accounts = keys
            .map(|key| Account::managed(Provider::Claude, key, homes.path(Provider::Claude, key)));
        let entry = |email: &str| Entry {
            identity: Some(Identity {
                email: Some(email.into()),
                ..Default::default()
            }),
            snapshot: UsageSnapshot::state(UsageStatus::Unavailable, "fixture", 1),
        };
        let before = fs::read(directory.path().join("accounts.toml")).unwrap();
        let mut cache = BTreeMap::from([(
            accounts[0].cache_key().unwrap(),
            entry("changed@example.com"),
        )]);
        let choose = DefaultSelection::Account(accounts[0].id());
        assert!(
            set_default(
                &store,
                Some(&homes),
                &cache,
                registry.revision(),
                Provider::Claude,
                &choose
            )
            .unwrap_err()
            .contains("different login")
        );
        cache.insert(accounts[0].cache_key().unwrap(), entry("one@example.com"));
        cache.insert(accounts[1].cache_key().unwrap(), entry("one@example.com"));
        assert!(
            set_default(
                &store,
                Some(&homes),
                &cache,
                registry.revision(),
                Provider::Claude,
                &choose
            )
            .unwrap_err()
            .contains("same login")
        );
        assert_eq!(
            fs::read(directory.path().join("accounts.toml")).unwrap(),
            before
        );
    }

    #[test]
    fn duplicate_home_is_refused_even_without_cached_identity() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::at(directory.path());
        let shared = directory.path().join("linked");
        fs::create_dir(&shared).unwrap();
        let keys = [Uuid::new_v4(), Uuid::new_v4()];
        let (registry, ()) = store
            .update(|registry| {
                for (key, email) in keys.into_iter().zip(["one@example.com", "two@example.com"]) {
                    let mut row = registered(key, email);
                    row.linked_home = Some(shared.clone());
                    registry.register(row, &[]).unwrap();
                }
            })
            .unwrap();
        let before = fs::read(directory.path().join("accounts.toml")).unwrap();
        let account = Account::linked(Provider::Claude, keys[0], shared);
        assert!(
            set_default(
                &store,
                None,
                &BTreeMap::new(),
                registry.revision(),
                Provider::Claude,
                &DefaultSelection::Account(account.id())
            )
            .unwrap_err()
            .contains("same home")
        );
        assert_eq!(
            fs::read(directory.path().join("accounts.toml")).unwrap(),
            before
        );
    }
}
