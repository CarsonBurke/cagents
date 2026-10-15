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
    /// Safe default: sign out and archive; saved transcripts stay on disk.
    Remove {
        id: AccountId,
    },
    InspectRemoval {
        id: AccountId,
    },
    MoveAndArchive {
        id: AccountId,
        target: AccountId,
    },
    /// Explicit destructive action, confirmed separately in the interface.
    DeleteFiles {
        id: AccountId,
        /// Optional for legacy direct callers; the UI supplies its exact
        /// daemon preview so a replaced home cannot reuse old consent.
        #[serde(default)]
        expected_home: Option<RemovalHome>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RemovalHome {
    Missing {
        #[serde(with = "crate::selected_file_path::required")]
        path: std::path::PathBuf,
    },
    Present(super::HomeIdentity),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemovalPreview {
    pub conversations: usize,
    pub home: RemovalHome,
}

#[cfg(feature = "host")]
impl RemovalHome {
    fn capture(account: &super::Account) -> Result<Self, String> {
        use std::os::unix::fs::MetadataExt;
        let home = account
            .resolved_home()
            .ok_or("The account has no credential directory")?;
        crate::secure_file::reject_symlink_components(&home).map_err(|error| error.to_string())?;
        match std::fs::metadata(&home) {
            Ok(metadata) if metadata.is_dir() => Ok(Self::Present(super::HomeIdentity {
                provider: account.provider,
                canonical_home: home.canonicalize().map_err(|error| error.to_string())?,
                device: metadata.dev(),
                inode: metadata.ino(),
                purge_ids: vec![account.id()],
            })),
            Ok(_) => Err("The account home is not a directory".into()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let mut parent = home.as_path();
                let mut suffix = Vec::new();
                let path = loop {
                    match parent.canonicalize() {
                        Ok(mut path) => {
                            for name in suffix.into_iter().rev() {
                                path.push(name);
                            }
                            break path;
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                            suffix.push(
                                parent
                                    .file_name()
                                    .ok_or("The account home has no existing parent")?
                                    .to_owned(),
                            );
                            parent = parent
                                .parent()
                                .ok_or("The account home has no existing parent")?;
                        }
                        Err(error) => return Err(error.to_string()),
                    }
                };
                Ok(Self::Missing { path })
            }
            Err(error) => Err(error.to_string()),
        }
    }
}

#[cfg(feature = "host")]
pub(crate) fn affected_account(operation: &Operation) -> Result<Option<super::Account>, String> {
    use super::{Account, home::Homes, registry::Store};
    let homes = Homes::installation().ok_or("Remote account storage is unavailable")?;
    let registry = Store::installation()
        .ok_or("Remote account registry is unavailable")?
        .load()
        .map_err(|error| error.to_string())?;
    match operation {
        Operation::Register { provider, key } | Operation::Abandon { provider, key } => {
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
            let path = if let Some(relative) = path.trim().strip_prefix("~/") {
                std::path::PathBuf::from(std::env::var_os("HOME").ok_or("HOME unavailable")?)
                    .join(relative)
            } else {
                std::path::PathBuf::from(path.trim())
            };
            if !path.is_absolute() {
                return Err("Enter an absolute account home path".into());
            }
            let path = path.canonicalize().map_err(|error| error.to_string())?;
            Ok(Some(Account::linked(*provider, *key, path)))
        }
        Operation::Remove { id }
        | Operation::MoveAndArchive { id, .. }
        | Operation::DeleteFiles { id, .. } => registry
            .all_accounts(Some(&homes))
            .into_iter()
            .find(|account| account.id() == *id)
            .map(Some)
            .ok_or("The device account was removed".into()),
        _ => Ok(None),
    }
}

/// Move writes the destination history as well, so the daemon fences both homes.
#[cfg(feature = "host")]
pub(crate) fn affected_accounts(operation: &Operation) -> Result<Vec<super::Account>, String> {
    let mut accounts: Vec<_> = affected_account(operation)?.into_iter().collect();
    if let Operation::MoveAndArchive { target, .. } = operation {
        let homes = super::home::Homes::installation();
        let registry = super::registry::Store::installation()
            .ok_or("Account registry unavailable")?
            .load()
            .map_err(|error| error.to_string())?;
        let target = registry
            .accounts(homes.as_ref())
            .into_iter()
            .find(|account| account.id() == *target)
            .ok_or("The destination account was removed")?;
        accounts.push(target);
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
pub(crate) fn execute(operation: Operation) -> Result<serde_json::Value, String> {
    execute_with_commits(
        operation,
        &Commits {
            gate: None,
            verify: &|| Ok(()),
        },
    )
}

/// The host installs a scoped reservation before this runs. Long provider and
/// filesystem work stays outside global admission; only final registry/rename
/// transactions take its writer. Unit callers retain their isolated stores.
#[cfg(feature = "host")]
pub(crate) fn execute_reserved(
    operation: Operation,
    gate: std::sync::Arc<std::sync::RwLock<()>>,
    verify: impl Fn() -> Result<(), String>,
) -> Result<serde_json::Value, String> {
    execute_with_commits(
        operation,
        &Commits {
            gate: Some(gate),
            verify: &verify,
        },
    )
}

#[cfg(feature = "host")]
struct Commits<'a> {
    gate: Option<std::sync::Arc<std::sync::RwLock<()>>>,
    verify: &'a dyn Fn() -> Result<(), String>,
}
#[cfg(feature = "host")]
impl Commits<'_> {
    fn apply<T>(&self, change: impl FnOnce() -> std::io::Result<T>) -> Result<T, String> {
        let _writer = self
            .gate
            .as_ref()
            .map(|gate| gate.write().unwrap_or_else(|p| p.into_inner()));
        (self.verify)()?;
        change().map_err(|error| error.to_string())
    }
}

#[cfg(feature = "host")]
fn execute_with_commits(
    operation: Operation,
    commits: &Commits<'_>,
) -> Result<serde_json::Value, String> {
    use super::{
        Account, UsageStatus,
        home::{CodexConfig, Homes},
        probe::{self, Outcome},
        registry::{Registered, Store},
    };
    let homes = Homes::installation().ok_or("The device has no account data directory")?;
    let store = Store::installation().ok_or("The device has no account registry")?;
    let now = crate::chat::now_ms().unwrap_or_default();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| error.to_string())?;
    match operation {
        Operation::Create {
            provider,
            copy_codex_config,
        } => {
            if !Account::PROVIDERS.contains(&provider) {
                return Err("Omp manages its own logins".into());
            }
            let search = runtime.block_on(probe::search_path());
            let program = probe::Cli::discover(provider, search.as_deref())?;
            let system = Account::system(provider).resolved_home();
            let (key, home) = commits.apply(|| {
                store.create_pending(&homes, provider, now, || {
                    homes.create_with_config(
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
            let recipe = match login_recipe(&account, &program) {
                Ok(recipe) => recipe,
                Err(error) => {
                    // A launch recipe failure must not strand an unused home
                    // and pending registry entry before the terminal opens.
                    let _ = commits
                        .apply(|| store.park_home_checked(&homes, provider, key, false, || Ok(())));
                    homes.finish_removals();
                    return Err(error);
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
            let chosen = if let Some(relative) = path.trim().strip_prefix("~/") {
                std::path::PathBuf::from(
                    std::env::var_os("HOME").ok_or("Remote HOME is unavailable")?,
                )
                .join(relative)
            } else {
                std::path::PathBuf::from(path.trim())
            };
            if !chosen.is_absolute() {
                return Err("Enter an absolute device path or one starting with ~/".into());
            }
            let path = chosen.canonicalize().map_err(|error| error.to_string())?;
            crate::secure_file::validate_owned_directory(&path)
                .map_err(|error| error.to_string())?;
            let system = Account::system(provider).resolved_home();
            let (registry, result) = commits.apply(|| {
                let _lifecycle = homes.lifecycle_lock()?;
                store.update(|registry| {
                    registry.link(provider, key, path, system.as_deref(), Some(&homes), now)
                })
            })?;
            let key = result?;
            serde_json::to_value((registry, key)).map_err(|error| error.to_string())
        }
        Operation::Register { provider, key } => {
            let registry = store.load().map_err(|error| error.to_string())?;
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
            let lock_cache =
                super::cache::Cache::installation().ok_or("Account storage cannot be locked")?;
            let _probe_locks = probe::lock_accounts(&lock_cache, &[&account])?;
            (commits.verify)()?;
            if identity.is_some() && snapshot.status == UsageStatus::NeedsLogin {
                return Err("The provider needs this account to sign in again".into());
            }
            if !probe::confirms_login(identity.as_ref(), snapshot.status) {
                let (registry, _) = commits
                    .apply(|| store.park_home_checked(&homes, provider, key, false, || Ok(())))?;
                homes.finish_removals();
                return serde_json::to_value(Settled::Abandoned(Some(registry)))
                    .map_err(|error| error.to_string());
            }
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
            let (registry, registered) =
                commits.apply(|| store.register_pending(&homes, managed, &system_identity))?;
            let settled = match registered {
                Ok(()) => Settled::Registered {
                    registry,
                    identity,
                    snapshot: Some(snapshot),
                },
                Err(existing) => {
                    (commits.verify)()?;
                    runtime.block_on(probe::logout_locked(account))?;
                    let (registry, _) = commits.apply(|| {
                        store.park_home_checked(&homes, provider, key, false, || Ok(()))
                    })?;
                    homes.finish_removals();
                    Settled::Duplicate {
                        existing,
                        registry: Some(registry),
                    }
                }
            };
            serde_json::to_value(settled).map_err(|error| error.to_string())
        }
        Operation::Abandon { provider, key } => {
            let registry = store.load().map_err(|error| error.to_string())?;
            if !registry
                .pending()
                .iter()
                .any(|pending| pending.provider == provider && pending.key == key)
            {
                return Err("This pending sign-in changed on the device".into());
            }
            let account = Account::managed(provider, key, homes.path(provider, key));
            let lock_cache =
                super::cache::Cache::installation().ok_or("Account storage cannot be locked")?;
            let _probe_locks = probe::lock_accounts(&lock_cache, &[&account])?;
            let (registry, _) = commits
                .apply(|| store.park_home_checked(&homes, provider, key, false, || Ok(())))?;
            homes.finish_removals();
            serde_json::to_value(Settled::Abandoned(Some(registry)))
                .map_err(|error| error.to_string())
        }
        Operation::InspectRemoval { id } => {
            let registry = store.load().map_err(|error| error.to_string())?;
            let account = registry
                .all_accounts(Some(&homes))
                .into_iter()
                .find(|account| account.id() == id)
                .ok_or("The account is no longer registered")?;
            serde_json::to_value(RemovalPreview {
                conversations: super::handoff::conversation_count(&account)?,
                home: RemovalHome::capture(&account)?,
            })
            .map_err(|error| error.to_string())
        }
        operation @ (Operation::Remove { .. }
        | Operation::MoveAndArchive { .. }
        | Operation::DeleteFiles { .. }) => {
            let cache = super::cache::Cache::installation().ok_or(
                "Account storage cannot be locked; restore the cache directory and retry.",
            )?;
            let result = remove_account_with_commits(
                operation,
                &store,
                &homes,
                &cache,
                |account| runtime.block_on(probe::logout_locked(account)),
                commits,
            )?;
            serde_json::to_value(result).map_err(|error| error.to_string())
        }
    }
}

#[cfg(feature = "host")]
fn remove_account(
    operation: Operation,
    store: &super::registry::Store,
    homes: &super::home::Homes,
    cache: &super::cache::Cache,
    logout: impl FnMut(super::Account) -> Result<(), String>,
) -> Result<(super::registry::Registry, Option<String>), String> {
    remove_account_with_commits(
        operation,
        store,
        homes,
        cache,
        logout,
        &Commits {
            gate: None,
            verify: &|| Ok(()),
        },
    )
}

#[cfg(feature = "host")]
fn remove_account_with_commits(
    operation: Operation,
    store: &super::registry::Store,
    homes: &super::home::Homes,
    cache: &super::cache::Cache,
    mut logout: impl FnMut(super::Account) -> Result<(), String>,
    commits: &Commits<'_>,
) -> Result<(super::registry::Registry, Option<String>), String> {
    use super::probe;
    let expected_home = match &operation {
        Operation::DeleteFiles { expected_home, .. } => expected_home.clone(),
        _ => None,
    };
    let (id, target, delete) = match operation {
        Operation::Remove { id } => (id, None, false),
        Operation::MoveAndArchive { id, target } => (id, Some(target), false),
        Operation::DeleteFiles { id, .. } => (id, None, true),
        _ => unreachable!(),
    };
    let registry = store.load().map_err(|error| error.to_string())?;
    let account = registry
        .all_accounts(Some(homes))
        .into_iter()
        .find(|account| account.id() == id)
        .ok_or("The account was already removed from this device")?;
    if let Some(expected) = expected_home {
        if RemovalHome::capture(&account)? != expected {
            return Err("The account home changed after its deletion preview. Reload the dialog and review its files before deleting.".into());
        }
    }
    let key = account
        .registry_key()
        .ok_or("System accounts cannot be removed")?;
    let target = target
        .map(|id| {
            registry
                .accounts(Some(homes))
                .into_iter()
                .find(|account| account.id() == id)
                .ok_or_else(|| "The destination account is unavailable".to_owned())
        })
        .transpose()?;
    let mut affected = vec![&account];
    affected.extend(target.as_ref());
    let _locks = probe::lock_accounts(cache, &affected)?;
    (commits.verify)()?;
    if let Some(target) = &target {
        super::handoff::move_history(&account, target)?;
    }
    (commits.verify)()?;
    let (registry, error) = if account.is_linked() {
        let (registry, ()) = commits.apply(|| {
            store.update(|registry| {
                registry.unregister(key);
            })
        })?;
        (registry, None)
    } else {
        if delete {
            // Explicit file deletion remains available without an installed
            // provider CLI. Report failed sign-out separately from deletion.
            let logout_error = logout(account.clone()).err();
            (commits.verify)()?;
            // Purge is potentially large. Scoped admission and probe locks keep
            // this home quiet without blocking unrelated terminal requests.
            let mut retained = registry.clone();
            retained.unregister(key);
            let annotation_warning = if retained.contains_managed_home(homes, account.provider, key)
            {
                None
            } else {
                purge_deleted_account_annotations(&account)?
            };
            let (registry, error) = commits
                .apply(|| store.park_home_checked(homes, account.provider, key, true, || Ok(())))?;
            homes.finish_removals();
            let error = match (error, annotation_warning) {
                (Some(error), Some(warning)) => Some(format!("{error}; {warning}")),
                (error, warning) => error.or(warning),
            };
            let error = match (error, logout_error) {
                (Some(error), Some(logout)) => Some(format!(
                    "{error}; provider sign-out could not be confirmed: {logout}"
                )),
                (None, Some(logout)) => Some(format!(
                    "Provider sign-out could not be confirmed: {logout}"
                )),
                (error, None) => error,
            };
            (registry, error)
        } else {
            logout(account.clone())?;
            let (registry, _) = commits.apply(|| {
                let _lifecycle = homes.lifecycle_lock()?;
                store.update(|registry| registry.archive(key))
            })?;
            (registry, None)
        }
    };

    Ok((registry, error))
}

/// Physical home identity authorizes deletion of retained originals. A missing
/// home can only use a previously persisted purge-only registration index.
#[cfg(feature = "host")]
fn purge_deleted_account_annotations(account: &super::Account) -> Result<Option<String>, String> {
    use crate::chat::annotations::storage;
    let home = account
        .resolved_home()
        .ok_or("The account has no credential directory")?;
    match std::fs::symlink_metadata(&home) {
        Ok(_) => {
            crate::secure_file::reject_symlink_components(&home).map_err(|error| error.to_string())?;
            let variable = super::home_variable(account.provider)
                .ok_or("This provider has no attested credential-directory scope")?;
            let scope = super::scope::LaunchAccountScope::resolve(
                account.provider,
                &home,
                |name| (name == variable).then(|| home.as_os_str().to_owned()),
            )?;
            storage::purge_account(&scope)?;
            Ok(None)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            match storage::purge_registered(&account.id())? {
                Some(_) => Ok(None),
                None => Ok(Some("The account home was already absent. No prior physical annotation association was available, so removal of older retained prompt data could not be verified".into())),
            }
        }
        Err(error) => Err(error.to_string()),
    }
}

#[cfg(feature = "host")]
pub(crate) fn login_recipe(
    account: &super::Account,
    cli: &super::probe::Cli,
) -> Result<crate::model::LaunchRecipe, String> {
    let mut environment = cli.configuration.environment.clone();
    environment.insert(
        super::SIGN_IN_MARKER.into(),
        account.provider.executable().into(),
    );
    let mut arguments = cli.configuration.arguments.clone();
    arguments.extend(match account.provider {
        Provider::Claude => vec!["auth".into(), "login".into(), "--claudeai".into()],
        Provider::Codex => vec!["login".into(), "--device-auth".into()],
        Provider::Omp => return Err("Omp manages its own sign-in".into()),
    });
    if let (Some(variable), Some(home)) = (
        super::home_variable(account.provider),
        account.resolved_home(),
    ) {
        environment.insert(variable.into(), home.display().to_string());
    }
    if let Some(path) = super::probe::shell_search_path().and_then(|path| path.into_string().ok()) {
        environment.entry("PATH".into()).or_insert(path);
    }
    Ok(crate::model::LaunchRecipe {
        program: cli
            .program
            .to_str()
            .ok_or("Provider path is not UTF-8")?
            .into(),
        arguments,
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

#[cfg(test)]
#[cfg(feature = "host")]
mod tests {
    use super::*;

    #[test]
    fn deletion_preview_refuses_replaced_and_newly_created_homes_before_logout_or_registry_changes()
    {
        use crate::account::{
            Account,
            cache::Cache,
            home::Homes,
            registry::{Registered, Store},
        };
        for initially_missing in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let homes = Homes::at(directory.path().join("homes"));
            let store = Store::at(directory.path().join("config"));
            let cache = Cache::at(directory.path().join("cache"));
            let (key, home) = homes.create(Provider::Claude, None).unwrap();
            let account = Account::managed(Provider::Claude, key, home.clone());
            store
                .update(|registry| {
                    registry
                        .register(
                            Registered {
                                key,
                                provider: Provider::Claude,
                                linked_home: None,
                                label: None,
                                email: Some("preview@fixture.invalid".into()),
                                organization: None,
                                added_at: 0,
                            },
                            &[],
                        )
                        .unwrap()
                })
                .unwrap();
            std::fs::write(home.join("original-private-file"), "preserved original").unwrap();
            let original = directory.path().join("original-home");
            if initially_missing {
                std::fs::rename(&home, &original).unwrap();
            }
            let preview = RemovalHome::capture(&account).unwrap();
            assert_eq!(
                matches!(preview, RemovalHome::Missing { .. }),
                initially_missing
            );
            if !initially_missing {
                std::fs::rename(&home, &original).unwrap();
            }
            std::fs::create_dir(&home).unwrap();
            std::fs::write(
                home.join("replacement-private-file"),
                "preserved replacement",
            )
            .unwrap();
            let before = store.load().unwrap();
            let error = remove_account(
                Operation::DeleteFiles {
                    id: account.id(),
                    expected_home: Some(preview),
                },
                &store,
                &homes,
                &cache,
                |_| panic!("changed preview must refuse before provider logout"),
            )
            .unwrap_err();
            assert!(
                error.contains("changed after its deletion preview"),
                "{error}"
            );
            assert_eq!(store.load().unwrap(), before);
            assert_eq!(
                std::fs::read_to_string(original.join("original-private-file")).unwrap(),
                "preserved original"
            );
            assert_eq!(
                std::fs::read_to_string(home.join("replacement-private-file")).unwrap(),
                "preserved replacement"
            );
        }
    }

    #[test]
    fn account_lifecycle_refuses_probe_owners_without_changing_homes_or_registry() {
        use crate::account::{
            Account,
            cache::Cache,
            home::Homes,
            registry::{Registered, Store},
        };
        for action in 0..3 {
            let directory = tempfile::tempdir().unwrap();
            let homes = Homes::at(directory.path().join("homes"));
            let store = Store::at(directory.path().join("config"));
            let cache = Cache::at(directory.path().join("cache"));
            let mut accounts = Vec::new();
            for email in ["a@fixture.invalid", "b@fixture.invalid"] {
                let (key, home) = homes.create(Provider::Claude, None).unwrap();
                let account = Account::managed(Provider::Claude, key, home);
                store
                    .update(|registry| {
                        registry
                            .register(
                                Registered {
                                    key,
                                    provider: Provider::Claude,
                                    linked_home: None,
                                    label: None,
                                    email: Some(email.into()),
                                    organization: None,
                                    added_at: 0,
                                },
                                &[],
                            )
                            .unwrap()
                    })
                    .unwrap();
                accounts.push(account);
            }
            let source = &accounts[0];
            let target = &accounts[1];
            let relative =
                std::path::Path::new("projects/fixture/11111111-1111-4111-8111-111111111111.jsonl");
            let transcript = source.resolved_home().unwrap().join(relative);
            std::fs::create_dir_all(transcript.parent().unwrap()).unwrap();
            std::fs::write(&transcript, "saved conversation").unwrap();
            let operation = match action {
                0 => Operation::Remove { id: source.id() },
                1 => Operation::MoveAndArchive {
                    id: source.id(),
                    target: target.id(),
                },
                _ => Operation::DeleteFiles {
                    id: source.id(),
                    expected_home: None,
                },
            };
            let before = store.load().unwrap();
            let lock = cache.try_probe_lock(&source.lock_name()).unwrap().unwrap();
            let error = remove_account(operation.clone(), &store, &homes, &cache, |_| {
                panic!("must not sign out a probed home")
            })
            .unwrap_err();
            assert!(error.contains("being checked or changed"));
            assert_eq!(store.load().unwrap(), before);
            assert_eq!(
                std::fs::read_to_string(&transcript).unwrap(),
                "saved conversation"
            );
            assert!(!target.resolved_home().unwrap().join(relative).exists());
            drop(lock);
            if action == 1 {
                let target_lock = cache.try_probe_lock(&target.lock_name()).unwrap().unwrap();
                assert!(
                    remove_account(operation.clone(), &store, &homes, &cache, |_| panic!(
                        "must not move into a probed home"
                    ))
                    .is_err()
                );
                assert_eq!(store.load().unwrap(), before);
                assert!(!target.resolved_home().unwrap().join(relative).exists());
                // A failed multi-home acquisition releases the already acquired
                // source guard rather than leaving either account stuck.
                assert!(cache.try_probe_lock(&source.lock_name()).unwrap().is_some());
                drop(target_lock);
            }
            let (registry, error) =
                remove_account(operation, &store, &homes, &cache, |_| Ok(())).unwrap();
            assert!(error.is_none());
            if action == 2 {
                assert!(!source.resolved_home().unwrap().exists());
                assert!(registry.find(source.registry_key().unwrap()).is_none());
            } else {
                assert_eq!(
                    std::fs::read_to_string(&transcript).unwrap(),
                    "saved conversation"
                );
                assert!(registry.is_archived(source.registry_key().unwrap()));
                assert!(!registry.accounts(Some(&homes)).contains(source));
                if action == 1 {
                    assert_eq!(
                        std::fs::read_to_string(target.resolved_home().unwrap().join(relative))
                            .unwrap(),
                        "saved conversation"
                    );
                }
            }
        }
    }

    #[test]
    fn host_lifecycle_creates_isolated_login_and_never_deletes_linked_files() {
        const CHILD: &str = "CAGENTS_REMOTE_ACCOUNT_TEST";
        // Match the repository's synthetic credential fixtures; the guard
        // forbids naming provider credential files in production sources.
        let credential_file = ["auth", ".json"].concat();
        if std::env::var_os(CHILD).is_none() {
            use std::os::unix::fs::PermissionsExt;
            let directory = tempfile::tempdir().unwrap();
            let root = directory.path();
            std::fs::create_dir_all(root.join("home/.codex")).unwrap();
            std::fs::create_dir_all(root.join("bin")).unwrap();
            std::fs::write(
                root.join("home/.codex").join(&credential_file),
                "fixture-private-login",
            )
            .unwrap();
            let shell = root.join("bin/shell");
            std::fs::write(&shell, "#!/bin/sh\nexec /bin/sh -c \"$2\"\n").unwrap();
            let codex = root.join("bin/codex");
            std::fs::write(&codex, "#!/bin/sh\nexit 0\n").unwrap();
            for program in [&shell, &codex] {
                std::fs::set_permissions(program, std::fs::Permissions::from_mode(0o700)).unwrap();
            }
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "account::remote::tests::host_lifecycle_creates_isolated_login_and_never_deletes_linked_files", "--nocapture"])
                .env_clear().env(CHILD, root).env("HOME", root.join("home"))
                .env("SHELL", shell).env("PATH", format!("{}:/usr/bin:/bin", root.join("bin").display()))
                .env("XDG_CONFIG_HOME", root.join("config")).env("XDG_DATA_HOME", root.join("data"))
                .env("XDG_RUNTIME_DIR", root).output().unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        use crate::account::{
            home::Homes,
            registry::{Registry, Store},
        };
        let root = std::path::PathBuf::from(std::env::var_os(CHILD).unwrap());
        let value = execute(Operation::Create {
            provider: Provider::Codex,
            copy_codex_config: false,
        })
        .unwrap();
        assert!(!value.to_string().contains("fixture-private-login"));
        let created: Created = serde_json::from_value(value).unwrap();
        assert_eq!(created.recipe.arguments, ["login", "--device-auth"]);
        assert!(!created.home.join(&credential_file).exists());
        let store = Store::installation().unwrap();
        assert!(
            store
                .load()
                .unwrap()
                .pending()
                .iter()
                .any(|pending| pending.key == created.key)
        );
        execute(Operation::Abandon {
            provider: Provider::Codex,
            key: created.key,
        })
        .unwrap();
        assert!(!created.home.exists());
        let created: Created = serde_json::from_value(
            execute(Operation::Create {
                provider: Provider::Codex,
                copy_codex_config: false,
            })
            .unwrap(),
        )
        .unwrap();
        let homes = Homes::installation().unwrap();
        store
            .register_pending(
                &homes,
                crate::account::registry::Registered {
                    key: created.key,
                    provider: Provider::Codex,
                    linked_home: None,
                    label: None,
                    email: Some("registered@example.invalid".into()),
                    organization: None,
                    added_at: 0,
                },
                &[],
            )
            .unwrap()
            .1
            .unwrap();
        let operation = Operation::Register {
            provider: Provider::Codex,
            key: created.key,
        };
        assert!(affected_account(&operation).unwrap().is_none());
        let settled: Settled = serde_json::from_value(execute(operation).unwrap()).unwrap();
        assert!(matches!(settled, Settled::Registered { .. }));
        assert!(
            created.home.is_dir(),
            "lost registration replies never remove the login"
        );
        // Archive retains original command metadata and attachment bytes;
        // explicit DeleteFiles purges them while the home is still provable.
        use crate::chat::annotations::{OriginalPrompt, OriginalSettings, Setting, storage};
        let account =
            super::super::Account::managed(Provider::Codex, created.key, created.home.clone());
        let scope =
            super::super::scope::LaunchAccountScope::resolve(Provider::Codex, &root, |name| {
                (name == "CODEX_HOME").then(|| created.home.as_os_str().to_owned())
            })
            .unwrap();
        let mut annotations =
            storage::Store::installation(&scope, "provider-confirmed-fixture").unwrap();
        let image = root.join("picked-image.png");
        std::fs::write(&image, b"original fixture image").unwrap();
        let mut captures = vec![annotations.retain_attachment(&image).unwrap()];
        let retained_image = captures[0].attachment.path.clone();
        let original = OriginalPrompt {
            user_id: "confirmed-user".into(),
            turn_id: "confirmed-turn".into(),
            text_bytes: 16,
            attachments: vec![captures[0].attachment.clone()],
            settings: OriginalSettings {
                model: Setting::Value("fixture-model".into()),
                reasoning: Setting::Unsupported,
                fast: Setting::Unsupported,
                permission: Setting::Value("default".into()),
            },
        };
        annotations
            .accept(&original, "original request", &[], &mut captures)
            .unwrap();
        drop(captures);
        drop(annotations);
        let cache = crate::account::cache::Cache::installation().unwrap();
        remove_account(
            Operation::Remove { id: account.id() },
            &store,
            &homes,
            &cache,
            |_| Ok(()),
        )
        .unwrap();
        assert!(created.home.is_dir());
        assert!(retained_image.is_file());
        let annotations =
            storage::Store::installation(&scope, "provider-confirmed-fixture").unwrap();
        assert_eq!(
            annotations
                .original(Some("confirmed-user"), 0, 4096)
                .unwrap()
                .text,
            "original request"
        );
        drop(annotations);
        let (registry, warning) = remove_account(
            Operation::DeleteFiles {
                id: account.id(),
                expected_home: None,
            },
            &store,
            &homes,
            &cache,
            |_| Ok(()),
        )
        .unwrap();
        assert!(warning.is_none(), "{warning:?}");
        assert!(!created.home.exists());
        assert!(!retained_image.exists());
        assert!(registry.find(created.key).is_none());
        assert!(storage::Store::installation(&scope, "provider-confirmed-fixture").is_err());
        // A prior purge-only association also cleans retained bytes after
        // external home loss, without inventing a new read authority.
        for indexed in [true, false] {
            let (key, home) = homes.create(Provider::Codex, None).unwrap();
            let account = super::super::Account::managed(Provider::Codex, key, home.clone());
            store
                .update(|registry| {
                    registry
                        .register(
                            crate::account::registry::Registered {
                                key,
                                provider: Provider::Codex,
                                linked_home: None,
                                label: None,
                                email: Some(format!("absent-{indexed}@fixture.invalid")),
                                organization: None,
                                added_at: 0,
                            },
                            &[],
                        )
                        .unwrap()
                })
                .unwrap();
            let retained = if indexed {
                let scope = super::super::scope::LaunchAccountScope::resolve(
                    Provider::Codex,
                    &root,
                    |name| (name == "CODEX_HOME").then(|| home.as_os_str().to_owned()),
                )
                .unwrap();
                let mut annotations =
                    storage::Store::installation(&scope, "confirmed-absent-session").unwrap();
                let mut captures = vec![annotations.retain_attachment(&image).unwrap()];
                let retained = captures[0].attachment.path.clone();
                let original = OriginalPrompt {
                    user_id: "confirmed-absent-user".into(),
                    turn_id: "confirmed-absent-turn".into(),
                    text_bytes: 4,
                    attachments: vec![captures[0].attachment.clone()],
                    settings: OriginalSettings {
                        model: Setting::Value("fixture-model".into()),
                        reasoning: Setting::Unsupported,
                        fast: Setting::Unsupported,
                        permission: Setting::Value("default".into()),
                    },
                };
                annotations
                    .accept(&original, "text", &[], &mut captures)
                    .unwrap();
                drop(captures);
                assert!(retained.is_file());
                Some(retained)
            } else {
                None
            };
            std::fs::remove_dir_all(&home).unwrap();
            let (registry, warning) = remove_account(
                Operation::DeleteFiles {
                    id: account.id(),
                    expected_home: None,
                },
                &store,
                &homes,
                &cache,
                |_| Ok(()),
            )
            .unwrap();
            assert!(registry.find(key).is_none());
            if let Some(retained) = retained {
                assert!(!retained.exists());
                assert!(warning.is_none());
            } else {
                assert!(
                    warning
                        .unwrap()
                        .contains("No prior physical annotation association")
                );
            }
        }
        let linked = root.join("home/linked-account");
        std::fs::create_dir_all(&linked).unwrap();
        std::fs::write(linked.join(&credential_file), "linked-private-login").unwrap();
        let key = uuid::Uuid::new_v4();
        let (registry, key): (Registry, uuid::Uuid) = serde_json::from_value(
            execute(Operation::Link {
                provider: Provider::Codex,
                key,
                path: "~/linked-account".into(),
            })
            .unwrap(),
        )
        .unwrap();
        let account = registry
            .accounts(Homes::installation().as_ref())
            .into_iter()
            .find(|account| account.registry_key() == Some(key))
            .unwrap();
        execute(Operation::Remove { id: account.id() }).unwrap();
        assert_eq!(
            std::fs::read_to_string(linked.join(&credential_file)).unwrap(),
            "linked-private-login"
        );
        assert!(store.load().unwrap().find(key).is_none());
    }
}
