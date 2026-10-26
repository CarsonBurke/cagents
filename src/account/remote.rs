//! Host-owned account lifecycle operations. Wire responses contain metadata
//! and official CLI launch recipes, never credential material.
use super::AccountId;
use crate::agent::Provider;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Operation {
    Create {
        provider: Provider,
        copy_codex_config: bool,
    },
    Link {
        provider: Provider,
        key: uuid::Uuid,
        path: String,
    },
    Register {
        provider: Provider,
        key: uuid::Uuid,
    },
    Abandon {
        provider: Provider,
        key: uuid::Uuid,
    },
    Remove {
        id: AccountId,
    },
}

#[cfg(feature = "host")]
fn require_sign_in_origin(
    registry: &super::registry::Registry,
    provider: Provider,
    key: uuid::Uuid,
) -> Result<(), String> {
    if registry
        .pending()
        .iter()
        .any(|pending| pending.key == key && pending.provider == provider && pending.is_import())
    {
        return Err("This pending account belongs to an import; use its original import status and explicit verification".into());
    }
    Ok(())
}

#[cfg(feature = "host")]
pub(crate) fn affected_account(operation: &Operation) -> Result<Option<super::Account>, String> {
    use super::{Account, home::Homes, registry::Store};
    let homes = Homes::installation().ok_or("Remote account storage is unavailable")?;
    let registry = Store::installation()
        .ok_or("Remote account registry is unavailable")?
        .try_load()
        .map_err(|error| error.to_string())?;
    match operation {
        Operation::Register { provider, key } | Operation::Abandon { provider, key } => {
            require_sign_in_origin(&registry, *provider, *key)?;
            let pending = registry
                .pending()
                .iter()
                .any(|pending| pending.provider == *provider && pending.key == *key);
            let already_registered = matches!(operation, Operation::Register { .. })
                && registry.find(*key).is_some_and(|registered| {
                    registered.provider == *provider && registered.linked_home.is_none()
                });
            if already_registered {
                return Ok(None);
            }
            if !pending {
                return Err("The pending sign-in changed on the device".into());
            }
            Ok(Some(Account::managed(
                *provider,
                *key,
                homes.path(*provider, *key),
            )))
        }
        Operation::Link {
            provider,
            key,
            path,
        } => {
            let chosen =
                super::expand_linked_home(path.trim(), std::env::var_os("HOME").as_deref())?;
            // Keep the user's ORIGINAL operand. Home capture resolves and pins
            // it while retaining this spelling for every later revalidation.
            Ok(Some(Account::linked(*provider, *key, chosen)))
        }
        Operation::Create { provider, .. } => Ok(Some(Account::system(*provider))),
        Operation::Remove { id } => registry
            .accounts(Some(&homes))
            .into_iter()
            .find(|account| account.id() == *id)
            .map(Some)
            .ok_or("The device account was removed".into()),
        _ => Ok(None),
    }
}

#[cfg(feature = "host")]
pub(crate) fn affected_accounts(operation: &Operation) -> Result<Vec<super::Account>, String> {
    let provider = match operation {
        Operation::Create { provider, .. }
        | Operation::Link { provider, .. }
        | Operation::Register { provider, .. }
        | Operation::Abandon { provider, .. } => Some(provider),
        _ => None,
    };
    if provider.is_some_and(|provider| !super::Account::PROVIDERS.contains(provider)) {
        return Err("Omp manages its own logins".into());
    }
    let mut accounts: Vec<_> = affected_account(operation)?.into_iter().collect();
    // Register reads the pending home and probes System for duplicate login
    // identity. Reserve both before either probe; an idempotent completed
    // registration has no probe and therefore needs no System reservation.
    if let Operation::Register { provider, .. } = operation {
        if !accounts.is_empty() {
            accounts.push(super::Account::system(*provider));
        }
    }
    Ok(accounts)
}

#[cfg(any(feature = "ui", feature = "host"))]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Created {
    pub key: uuid::Uuid,
    pub home: std::path::PathBuf,
    pub recipe: crate::model::LaunchRecipe,
}

#[cfg(any(feature = "ui", feature = "host"))]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Settled {
    Registered {
        registry: super::registry::Registry,
        identity: Option<super::Identity>,
        snapshot: Option<super::UsageSnapshot>,
    },
    Duplicate {
        existing: AccountId,
        registry: Option<super::registry::Registry>,
    },
    Abandoned(Option<super::registry::Registry>),
}

#[cfg(feature = "host")]
pub(crate) fn execute(_operation: Operation) -> Result<serde_json::Value, String> {
    Err("Account lifecycle requires the selected daemon's scoped admission reservation".into())
}

/// The opaque host reservation remains borrowed throughout provider I/O.
#[cfg(feature = "host")]
pub(crate) fn execute_reserved(
    operation: Operation,
    reservation: &crate::host::AccountReservation,
) -> Result<serde_json::Value, String> {
    use super::{home::Homes, registry::Store};
    let homes = Homes::installation().ok_or("The device has no account data directory")?;
    let store = Store::installation().ok_or("The device has no account registry")?;
    execute_at(operation, reservation, homes, store)
}

#[cfg(feature = "host")]
fn execute_at(
    operation: Operation,
    reservation: &crate::host::AccountReservation,
    homes: super::home::Homes,
    store: super::registry::Store,
) -> Result<serde_json::Value, String> {
    use super::{
        Account, UsageStatus,
        home::CodexConfig,
        probe::{self, Outcome},
        registry::Registered,
    };
    let now = crate::chat::now_ms().unwrap_or_default();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| error.to_string())?;
    reservation.verify()?;
    match operation {
        Operation::Create {
            provider,
            copy_codex_config,
        } => {
            if !Account::PROVIDERS.contains(&provider) {
                return Err("Omp manages its own logins".into());
            }
            let search = runtime.block_on(probe::search_path());
            let program = probe::find_executable(provider.executable(), search.as_deref())
                .ok_or("The provider CLI is not installed on this device")?;
            let system = Account::system(provider).resolved_home();
            let (key, home) = reservation.commit(|| {
                store.try_create_pending_reserved(reservation, &homes, provider, now, || {
                    reservation.create_home(
                        &homes,
                        provider,
                        system.as_deref(),
                        if copy_codex_config {
                            CodexConfig::Copy
                        } else {
                            CodexConfig::Shared
                        },
                    )
                })
            })?;
            let account = Account::managed(provider, key, home.clone());
            let recipe = match login_recipe_with_path(&account, program, search.as_deref()) {
                Ok(recipe) => recipe,
                Err(error) => {
                    return Err(format!(
                        "{error}; the new account home and pending registration were retained at {}. Retry sign-in after fixing the launch configuration.",
                        home.display()
                    ));
                }
            };
            serde_json::to_value(Created { key, home, recipe }).map_err(|error| error.to_string())
        }
        Operation::Link {
            provider,
            key,
            path,
        } => {
            if !Account::PROVIDERS.contains(&provider) {
                return Err("Omp manages its own logins".into());
            }
            let chosen =
                super::expand_linked_home(path.trim(), std::env::var_os("HOME").as_deref())?;
            let path = reservation.link_home(provider, key, &chosen)?;
            crate::secure_file::validate_owned_directory(&path)
                .map_err(|error| error.to_string())?;
            let system = Account::system(provider).resolved_home();
            let (registry, result) = reservation.commit(|| {
                let _lifecycle = homes.try_lifecycle_lock()?;
                store.try_update_reserved(reservation, |registry| {
                    registry.link(provider, key, path, system.as_deref(), Some(&homes), now)
                })
            })?;
            let key = result?;
            serde_json::to_value((registry, key)).map_err(|error| error.to_string())
        }
        Operation::Register { provider, key } => {
            let registry = store.try_load().map_err(|error| error.to_string())?;
            require_sign_in_origin(&registry, provider, key)?;
            // A transport interruption can lose a successful reply. Retrying
            // the same pending key must not delete or replace that login.
            if registry.find(key).is_some_and(|registered| {
                registered.provider == provider && registered.linked_home.is_none()
            }) {
                return serde_json::to_value(Settled::Registered {
                    registry,
                    identity: None,
                    snapshot: None,
                })
                .map_err(|error| error.to_string());
            }
            if !registry
                .pending()
                .iter()
                .any(|pending| pending.provider == provider && pending.key == key)
            {
                return Err("This sign-in is no longer pending on the device".into());
            }
            let account = Account::managed(provider, key, homes.path(provider, key));
            let (identity, snapshot) =
                match runtime.block_on(probe::run(account.clone(), None, true)) {
                    Outcome::Answered {
                        identity, snapshot, ..
                    } => (identity, snapshot),
                    Outcome::Failed(error) => return Err(error),
                    Outcome::Busy => {
                        return Err(
                            "The provider is already verifying another account; try again".into(),
                        );
                    }
                };
            if identity.is_some() && snapshot.status == UsageStatus::NeedsLogin {
                return Err("The provider needs this account to sign in again".into());
            }
            if !probe::confirms_login(identity.as_ref(), snapshot.status) {
                let (registry, ()) = reservation.commit(|| {
                    store.try_update_reserved(reservation, |registry| registry.abandon(key))
                })?;
                return serde_json::to_value(Settled::Abandoned(Some(registry)))
                    .map_err(|error| error.to_string());
            }
            // The pending-home probe may have taken a long time. The live
            // token fences daemon ingress, but an external rename/replacement
            // must not redirect this second provider read to a different home.
            reservation.verify()?;
            let system_identity = match runtime.block_on(probe::run(
                Account::system(provider),
                super::cache::Cache::installation(),
                false,
            )) {
                Outcome::Answered {
                    identity: Some(identity),
                    ..
                } => vec![(provider, identity)],
                _ => Vec::new(),
            };
            let managed = Registered {
                key,
                provider,
                linked_home: None,
                label: None,
                email: identity
                    .as_ref()
                    .and_then(|identity| identity.email.clone()),
                organization: identity
                    .as_ref()
                    .and_then(|identity| identity.organization.clone()),
                added_at: now,
            };
            let (registry, registered) = reservation.commit(|| {
                store.try_register_pending_reserved(reservation, &homes, managed, &system_identity)
            })?;
            let settled = match registered {
                Ok(()) => Settled::Registered {
                    registry,
                    identity,
                    snapshot: Some(snapshot),
                },
                Err(existing) => {
                    let (registry, ()) = reservation.commit(|| {
                        store.try_update_reserved(reservation, |registry| registry.abandon(key))
                    })?;
                    Settled::Duplicate {
                        existing,
                        registry: Some(registry),
                    }
                }
            };
            serde_json::to_value(settled).map_err(|error| error.to_string())
        }
        Operation::Abandon { provider, key } => {
            let registry = store.try_load().map_err(|error| error.to_string())?;
            require_sign_in_origin(&registry, provider, key)?;
            if !registry
                .pending()
                .iter()
                .any(|pending| pending.provider == provider && pending.key == key)
            {
                return Err("This pending sign-in changed on the device".into());
            }
            let (registry, ()) = reservation.commit(|| {
                store.try_update_reserved(reservation, |registry| registry.abandon(key))
            })?;
            serde_json::to_value(Settled::Abandoned(Some(registry)))
                .map_err(|error| error.to_string())
        }
        Operation::Remove { id } => {
            let registry = store.try_load().map_err(|error| error.to_string())?;
            let account = registry
                .accounts(Some(&homes))
                .into_iter()
                .find(|account| account.id() == id)
                .ok_or("The account was already removed from this device")?;
            let key = account
                .registry_key()
                .ok_or("System accounts cannot be removed")?;
            let (registry, error) = if account.is_linked() {
                let (registry, removed) = reservation.commit(|| {
                    store.try_update_reserved(reservation, |registry| registry.unregister(key))
                })?;
                removed.ok_or(
                    "The linked account registration changed before it could be forgotten",
                )?;
                (registry, None::<String>)
            } else {
                return Err("File deletion is not available in this build. Your account and its files were kept.".into());
            };
            serde_json::to_value((registry, error)).map_err(|error| error.to_string())
        }
    }
}

#[cfg(feature = "host")]
#[cfg(test)]
fn login_recipe(
    account: &super::Account,
    program: std::path::PathBuf,
) -> Result<crate::model::LaunchRecipe, String> {
    login_recipe_with_path(
        account,
        program,
        super::probe::current_search_path().as_deref(),
    )
}

#[cfg(feature = "host")]
fn login_recipe_with_path(
    account: &super::Account,
    program: std::path::PathBuf,
    search_path: Option<&std::ffi::OsStr>,
) -> Result<crate::model::LaunchRecipe, String> {
    let mut environment = std::collections::BTreeMap::new();
    if let (Some(variable), Some(home)) = (
        super::home_variable(account.provider),
        account.resolved_home(),
    ) {
        environment.insert(variable.into(), home.display().to_string());
    }
    if let Some(path) = search_path.and_then(|path| path.to_str()) {
        environment.insert("PATH".into(), path.into());
    }
    Ok(crate::model::LaunchRecipe {
        program: program.to_str().ok_or("Provider path is not UTF-8")?.into(),
        arguments: match account.provider {
            Provider::Claude => vec!["auth".into(), "login".into(), "--claudeai".into()],
            Provider::Codex => vec!["login".into(), "--device-auth".into()],
            Provider::Omp => return Err("Omp manages its own sign-in".into()),
        },
        working_directory: std::env::var_os("HOME")
            .ok_or("Remote HOME is unavailable")?
            .into(),
        environment,
        unset_environment: account
            .credential_overrides()
            .iter()
            .map(|name| (*name).into())
            .collect(),
    })
}

#[cfg(all(test, feature = "host"))]
mod tests {
    use super::*;
    use crate::{
        account::{
            Account,
            home::Homes,
            registry::{Registered, Store},
        },
        agent::Provider,
    };
    use std::os::unix::fs::MetadataExt;

    #[test]
    fn import_pending_refuses_generic_registration_and_abandon_before_any_provider_probe() {
        let root = tempfile::tempdir().unwrap();
        let homes = Homes::at(root.path().join("homes"));
        let store = Store::at(root.path().join("registry"));
        let (key, home) = store
            .create_pending(&homes, Provider::Codex, 1, || {
                homes.create(Provider::Codex, None)
            })
            .unwrap();
        store
            .update(|registry| {
                let mut encoded = serde_json::to_value(&*registry).unwrap();
                let row = encoded["pending"]
                    .as_array_mut()
                    .unwrap()
                    .iter_mut()
                    .find(|row| row["key"] == key.to_string())
                    .unwrap();
                row["origin"] = serde_json::json!({"kind":"cache_import","operation":key});
                *registry = serde_json::from_value(encoded).unwrap();
            })
            .unwrap();
        std::fs::write(home.join("retain-import-fixture"), b"retained").unwrap();
        let host = crate::host::AccountReservation::test_host();
        let reservation = crate::host::AccountReservation::test_install(
            &host,
            &[Account::managed(Provider::Codex, key, home.clone())],
            store.clone(),
            &homes,
        )
        .unwrap();
        let before = store.load().unwrap();
        for operation in [
            Operation::Register {
                provider: Provider::Codex,
                key,
            },
            Operation::Abandon {
                provider: Provider::Codex,
                key,
            },
        ] {
            let error =
                execute_at(operation, &reservation, homes.clone(), store.clone()).unwrap_err();
            assert_eq!(
                error,
                "This pending account belongs to an import; use its original import status and explicit verification"
            );
            assert_eq!(store.load().unwrap(), before);
            assert_eq!(
                std::fs::read(home.join("retain-import-fixture")).unwrap(),
                b"retained"
            );
        }
    }

    #[test]
    fn host_lifecycle_never_deletes_managed_or_linked_files_without_a_permit() {
        let root = tempfile::tempdir().unwrap();
        let homes = Homes::at(root.path().join("homes"));
        let store = Store::at(root.path().join("registry"));
        let system = root.path().join("system");
        std::fs::create_dir(&system).unwrap();
        let credential_file = ["auth", ".json"].concat();
        std::fs::write(system.join(&credential_file), b"synthetic-private-login").unwrap();
        let (key, home) = store
            .create_pending(&homes, Provider::Codex, 1, || {
                homes.create(Provider::Codex, Some(&system))
            })
            .unwrap();
        let recipe = login_recipe(
            &Account::managed(Provider::Codex, key, home.clone()),
            "/bin/false".into(),
        )
        .unwrap();
        assert_eq!(recipe.arguments, ["login", "--device-auth"]);
        assert_eq!(
            recipe.environment.get("CODEX_HOME"),
            Some(&home.display().to_string())
        );
        assert!(!home.join(&credential_file).exists());
        let value = serde_json::to_string(&Created {
            key,
            home: home.clone(),
            recipe,
        })
        .unwrap();
        assert!(!value.contains("synthetic-private-login"));
        std::fs::write(home.join("provider-payload"), b"keep managed history").unwrap();
        store
            .update(|registry| {
                registry.register(
                    Registered {
                        key,
                        provider: Provider::Codex,
                        linked_home: None,
                        label: None,
                        email: None,
                        organization: None,
                        added_at: 1,
                    },
                    &[],
                )
            })
            .unwrap()
            .1
            .unwrap();
        let account = Account::managed(Provider::Codex, key, home.clone());
        let host = crate::host::AccountReservation::test_host();
        let reservation = crate::host::AccountReservation::test_install(
            &host,
            &[account.clone()],
            store.clone(),
            &homes,
        )
        .unwrap();
        let settled: Settled = serde_json::from_value(
            execute_at(
                Operation::Register {
                    provider: Provider::Codex,
                    key,
                },
                &reservation,
                homes.clone(),
                store.clone(),
            )
            .unwrap(),
        )
        .unwrap();
        assert!(matches!(settled, Settled::Registered { .. }));
        let before = store.load().unwrap();
        let inode = home.metadata().unwrap().ino();
        let error = execute_at(
            Operation::Remove { id: account.id() },
            &reservation,
            homes.clone(),
            store.clone(),
        )
        .unwrap_err();
        assert!(error.contains("Your account and its files were kept"));
        assert_eq!(before, store.load().unwrap());
        assert_eq!(inode, home.metadata().unwrap().ino());
        assert_eq!(
            std::fs::read(home.join("provider-payload")).unwrap(),
            b"keep managed history"
        );
        drop(reservation);
        let (pending_key, pending_home) = store
            .create_pending(&homes, Provider::Codex, 2, || {
                homes.create(Provider::Codex, None)
            })
            .unwrap();
        let pending = Account::managed(Provider::Codex, pending_key, pending_home.clone());
        let pending_reservation =
            crate::host::AccountReservation::test_install(&host, &[pending], store.clone(), &homes)
                .unwrap();
        execute_at(
            Operation::Abandon {
                provider: Provider::Codex,
                key: pending_key,
            },
            &pending_reservation,
            homes.clone(),
            store.clone(),
        )
        .unwrap();
        assert!(
            pending_home.is_dir(),
            "abandoned sign-in metadata grants no file deletion proof"
        );
        assert!(
            store
                .load()
                .unwrap()
                .pending()
                .iter()
                .all(|pending| pending.key != pending_key)
        );
        drop(pending_reservation);
        let linked = root.path().join("linked");
        std::fs::create_dir(&linked).unwrap();
        std::fs::write(linked.join("history"), b"keep linked history").unwrap();
        let linked_key = uuid::Uuid::new_v4();
        store
            .update(|registry| {
                registry.link(
                    Provider::Codex,
                    linked_key,
                    linked.clone(),
                    None,
                    Some(&homes),
                    1,
                )
            })
            .unwrap()
            .1
            .unwrap();
        let account = Account::linked(Provider::Codex, linked_key, linked.clone());
        let reservation = crate::host::AccountReservation::test_install(
            &host,
            &[account.clone()],
            store.clone(),
            &homes,
        )
        .unwrap();
        execute_at(
            Operation::Remove { id: account.id() },
            &reservation,
            homes,
            store.clone(),
        )
        .unwrap();
        assert!(store.load().unwrap().find(linked_key).is_none());
        assert_eq!(
            std::fs::read(linked.join("history")).unwrap(),
            b"keep linked history"
        );
    }

    #[test]
    fn a_prepared_remote_login_uses_its_selected_path_snapshot() {
        let selected = std::ffi::OsString::from("/old/node/bin:/old/cli/bin");
        let recipe = login_recipe_with_path(
            &Account::system(Provider::Codex),
            "/old/cli/bin/codex".into(),
            Some(&selected),
        )
        .unwrap();
        assert_eq!(recipe.program, "/old/cli/bin/codex");
        assert_eq!(
            recipe.environment.get("PATH").map(String::as_str),
            selected.to_str()
        );
        assert_eq!(recipe.arguments, ["login", "--device-auth"]);
        let no_path = login_recipe_with_path(
            &Account::system(Provider::Codex),
            "/old/cli/bin/codex".into(),
            None,
        )
        .unwrap();
        assert!(!no_path.environment.contains_key("PATH"));
    }
}
