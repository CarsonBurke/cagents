//! Host-owned account lifecycle operations. Wire responses contain metadata
//! and official CLI launch recipes, never credential material.
use super::AccountId;
use crate::agent::Provider;
#[cfg(feature = "host")]
use crate::chat::annotations::storage::{PurgeFailure, PurgeReceipt};
use serde::{Deserialize, Serialize};

#[cfg(feature = "host")]
type AnnotationPurgeResult = Result<(Option<String>, PurgeReceipt), PurgeFailure>;

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
    },
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
        Operation::Remove { id }
        | Operation::MoveAndArchive { id, .. }
        | Operation::DeleteFiles { id } => registry
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
            let (key, home) = store
                .create_pending(&homes, provider, now, || {
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
                .map_err(|error| error.to_string())?;
            let account = Account::managed(provider, key, home.clone());
            let recipe = match login_recipe(&account, &program) {
                Ok(recipe) => recipe,
                Err(error) => {
                    // A launch recipe failure must not strand an unused home
                    // and pending registry entry before the terminal opens.
                    let _ = store.remove_home(&homes, provider, key, false);
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
            let _lifecycle = homes.lifecycle_lock().map_err(|error| error.to_string())?;
            let system = Account::system(provider).resolved_home();
            let (registry, result) = store
                .update(|registry| {
                    registry.link(provider, key, path, system.as_deref(), Some(&homes), now)
                })
                .map_err(|error| error.to_string())?;
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
            if identity.is_some() && snapshot.status == UsageStatus::NeedsLogin {
                return Err("The provider needs this account to sign in again".into());
            }
            if !probe::confirms_login(identity.as_ref(), snapshot.status) {
                let (registry, _) = store
                    .remove_home(&homes, provider, key, false)
                    .map_err(|error| error.to_string())?;
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
            let (registry, registered) = store
                .register_pending(&homes, managed, &system_identity)
                .map_err(|error| error.to_string())?;
            let settled = match registered {
                Ok(()) => Settled::Registered {
                    registry,
                    identity,
                    snapshot: Some(snapshot),
                },
                Err(existing) => {
                    runtime.block_on(probe::logout(account))?;
                    let (registry, _) = store
                        .remove_home(&homes, provider, key, false)
                        .map_err(|error| error.to_string())?;
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
            let (registry, _) = store
                .remove_home(&homes, provider, key, false)
                .map_err(|error| error.to_string())?;
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
            serde_json::to_value(super::handoff::conversation_count(&account)?)
                .map_err(|error| error.to_string())
        }
        operation @ (Operation::Remove { .. }
        | Operation::MoveAndArchive { .. }
        | Operation::DeleteFiles { .. }) => {
            let cache = super::cache::Cache::installation().ok_or(
                "Account storage cannot be locked; restore the cache directory and retry.",
            )?;
            let result = remove_account(operation, &store, &homes, &cache, |account| {
                runtime.block_on(probe::logout_locked(account))
            })?;
            serde_json::to_value(result).map_err(|error| error.to_string())
        }
    }
}

#[cfg(feature = "host")]
#[derive(Debug)]
pub(crate) struct RemovalOutcome {
    pub(crate) registry: super::registry::Registry,
    pub(crate) warning: Option<String>,
    pub(crate) purge_receipt: PurgeReceipt,
}

#[cfg(feature = "host")]
#[derive(Debug)]
pub(crate) struct RemovalFailure {
    pub(crate) message: String,
    pub(crate) purge_receipt: PurgeReceipt,
}

// Compatibility boundary: the current device reply remains the existing tuple.
#[cfg(feature = "host")]
fn remove_account(
    operation: Operation,
    store: &super::registry::Store,
    homes: &super::home::Homes,
    cache: &super::cache::Cache,
    logout: impl FnMut(super::Account) -> Result<(), String>,
) -> Result<(super::registry::Registry, Option<String>), String> {
    remove_account_with_receipt(operation, store, homes, cache, logout)
        .map(|outcome| (outcome.registry, outcome.warning))
        .map_err(|failure| failure.message)
}

#[cfg(feature = "host")]
pub(crate) fn remove_account_with_receipt(
    operation: Operation,
    store: &super::registry::Store,
    homes: &super::home::Homes,
    cache: &super::cache::Cache,
    logout: impl FnMut(super::Account) -> Result<(), String>,
) -> Result<RemovalOutcome, RemovalFailure> {
    remove_account_with_purge(
        operation,
        store,
        homes,
        cache,
        logout,
        purge_deleted_account_annotations,
    )
}

#[cfg(feature = "host")]
fn remove_account_with_purge(
    operation: Operation,
    store: &super::registry::Store,
    homes: &super::home::Homes,
    cache: &super::cache::Cache,
    mut logout: impl FnMut(super::Account) -> Result<(), String>,
    mut purge: impl FnMut(&super::Account) -> AnnotationPurgeResult,
) -> Result<RemovalOutcome, RemovalFailure> {
    // Capture outside the transaction callback: registry persistence or parking
    // can still fail after annotation markers already began refusing reads.
    let mut purge_receipt = PurgeReceipt::default();
    let result = (|| -> Result<(super::registry::Registry, Option<String>), String> {
        use super::probe;
        let (id, target, delete) = match operation {
            Operation::Remove { id } => (id, None, false),
            Operation::MoveAndArchive { id, target } => (id, Some(target), false),
            Operation::DeleteFiles { id } => (id, None, true),
            _ => unreachable!(),
        };
        let registry = store.load().map_err(|error| error.to_string())?;
        let account = registry
            .all_accounts(Some(homes))
            .into_iter()
            .find(|account| account.id() == id)
            .ok_or("The account was already removed from this device")?;
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
        if let Some(target) = &target {
            super::handoff::move_history(&account, target)?;
        }
        let (registry, error) = if account.is_linked() {
            let (registry, ()) = store
                .update(|registry| {
                    registry.unregister(key);
                })
                .map_err(|error| error.to_string())?;
            (registry, None)
        } else {
            if delete {
                // Explicit file deletion remains available without an installed
                // provider CLI. Report failed sign-out separately from deletion.
                let logout_error = logout(account.clone()).err();
                let mut annotation_warning = None;
                let (registry, error) = store
                    .remove_home_checked(homes, account.provider, key, true, || {
                        match purge(&account) {
                            Ok((warning, receipt)) => {
                                annotation_warning = warning;
                                purge_receipt = receipt;
                                Ok(())
                            }
                            Err(failure) => {
                                purge_receipt = failure.receipt;
                                Err(std::io::Error::other(failure.message))
                            }
                        }
                    })
                    .map_err(|error| error.to_string())?;
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
                let _lifecycle = homes.lifecycle_lock().map_err(|error| error.to_string())?;
                let (registry, _) = store
                    .update(|registry| registry.archive(key))
                    .map_err(|error| error.to_string())?;
                (registry, None)
            }
        };

        Ok((registry, error))
    })();
    match result {
        Ok((registry, warning)) => Ok(RemovalOutcome {
            registry,
            warning,
            purge_receipt,
        }),
        Err(message) => Err(RemovalFailure {
            message,
            purge_receipt,
        }),
    }
}

/// Physical home identity authorizes deletion of retained originals. A missing
/// home can only use a previously persisted purge-only registration index.
#[cfg(feature = "host")]
fn purge_deleted_account_annotations(account: &super::Account) -> AnnotationPurgeResult {
    use crate::chat::annotations::storage;
    purge_deleted_account_annotations_with(
        account,
        storage::purge_account_with_receipt,
        storage::purge_registered_with_receipt,
    )
}

#[cfg(feature = "host")]
fn purge_deleted_account_annotations_with(
    account: &super::Account,
    purge_account: impl FnOnce(&super::scope::LaunchAccountScope) -> Result<PurgeReceipt, PurgeFailure>,
    purge_registered: impl FnOnce(&super::AccountId) -> Result<Option<PurgeReceipt>, PurgeFailure>,
) -> AnnotationPurgeResult {
    use PurgeReceipt;
    let home = account
        .resolved_home()
        .ok_or_else(|| "The account has no credential directory".to_owned())?;
    match std::fs::symlink_metadata(&home) {
        Ok(_) => {
            crate::secure_file::reject_symlink_components(&home)
                .map_err(|error| error.to_string())?;
            // This callback holds the lifecycle and registry write locks.
            // Resolve only physical identity: the general launch resolver loads
            // the installation registry and would reacquire our own lock.
            let scope = capture_purge_scope(account.provider, &home)?;
            Ok((None, purge_account(&scope)?))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            match purge_registered(&account.id())? {
                Some(receipt) => Ok((None, receipt)),
                None => Ok((Some("The account home was already absent. No prior physical annotation association was available, so removal of older retained prompt data could not be verified".into()), PurgeReceipt::default())),
            }
        }
        Err(error) => Err(error.to_string().into()),
    }
}

/// Called only by the checked removal callback while lifecycle custody protects
/// the managed home. Purge needs the physical namespace, not registry aliases or
/// a launch/read capability. Storage revalidates this tuple before any marker.
#[cfg(feature = "host")]
fn capture_purge_scope(
    provider: Provider,
    home: &std::path::Path,
) -> Result<super::scope::LaunchAccountScope, String> {
    use std::os::unix::fs::MetadataExt;
    if !home.is_absolute() {
        return Err("The account home must be absolute".into());
    }
    if super::home_variable(provider).is_none() {
        return Err("This provider has no attested credential-directory scope".into());
    }
    let canonical_home = home.canonicalize().map_err(|error| error.to_string())?;
    let metadata = std::fs::metadata(&canonical_home).map_err(|error| error.to_string())?;
    if !metadata.is_dir() {
        return Err("The selected provider home is not a directory".into());
    }
    let scope = super::scope::LaunchAccountScope {
        provider,
        canonical_home,
        device: metadata.dev(),
        inode: metadata.ino(),
        registered_id: None,
        purge_ids: Vec::new(),
    };
    scope.revalidate()?;
    Ok(scope)
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

    struct PurgeFixture {
        _directory: tempfile::TempDir,
        homes: crate::account::home::Homes,
        store: crate::account::registry::Store,
        cache: crate::account::cache::Cache,
        annotations: std::path::PathBuf,
    }
    impl PurgeFixture {
        fn new() -> Self {
            let directory = tempfile::tempdir().unwrap();
            Self {
                homes: crate::account::home::Homes::at(directory.path().join("homes")),
                store: crate::account::registry::Store::at(directory.path().join("config")),
                cache: crate::account::cache::Cache::at(directory.path().join("cache")),
                annotations: directory.path().join("annotations"),
                _directory: directory,
            }
        }
        fn managed(&self) -> crate::account::Account {
            let (key, home) = self.homes.create(Provider::Claude, None).unwrap();
            let account = crate::account::Account::managed(Provider::Claude, key, home);
            self.store
                .update(|registry| {
                    registry
                        .register(
                            crate::account::registry::Registered {
                                key,
                                provider: Provider::Claude,
                                linked_home: None,
                                label: None,
                                email: None,
                                organization: None,
                                added_at: 0,
                            },
                            &[],
                        )
                        .unwrap()
                })
                .unwrap();
            account
        }
        fn originals(
            &self,
            account: &crate::account::Account,
        ) -> crate::chat::annotations::storage::Store {
            use crate::chat::annotations::{OriginalPrompt, OriginalSettings, Setting, storage};
            use std::os::unix::fs::MetadataExt;
            let home = account.resolved_home().unwrap().canonicalize().unwrap();
            let metadata = std::fs::metadata(&home).unwrap();
            let scope = crate::account::scope::LaunchAccountScope {
                provider: account.provider,
                canonical_home: home,
                device: metadata.dev(),
                inode: metadata.ino(),
                registered_id: Some(account.id()),
                purge_ids: vec![account.id()],
            };
            let mut originals =
                storage::Store::at(&self.annotations, &scope, "fixture-session").unwrap();
            let original = OriginalPrompt {
                user_id: "user".into(),
                turn_id: "turn".into(),
                text_bytes: 4,
                attachments: vec![],
                settings: OriginalSettings {
                    model: Setting::Default,
                    reasoning: Setting::Default,
                    fast: Setting::Unsupported,
                    permission: Setting::Default,
                },
            };
            originals.accept(&original, "text", &[]).unwrap();
            originals
        }
        fn remove(
            &self,
            operation: Operation,
            logout: impl FnMut(crate::account::Account) -> Result<(), String>,
        ) -> Result<RemovalOutcome, RemovalFailure> {
            use crate::chat::annotations::storage;
            remove_account_with_purge(
                operation,
                &self.store,
                &self.homes,
                &self.cache,
                logout,
                |account| {
                    purge_deleted_account_annotations_with(
                        account,
                        |scope| storage::purge_account_receipt_at(&self.annotations, scope),
                        |id| storage::purge_registered_receipt_at(&self.annotations, id),
                    )
                },
            )
        }
    }

    #[test]
    fn lifecycle_purge_receipt_keeps_real_keys_despite_logout_warning_and_missing_home() {
        for missing in [false, true] {
            let fixture = PurgeFixture::new();
            let account = fixture.managed();
            let originals = fixture.originals(&account);
            let lineage = originals.original_lineage();
            if missing {
                std::fs::remove_dir_all(account.resolved_home().unwrap()).unwrap();
            }
            let result = fixture
                .remove(Operation::DeleteFiles { id: account.id() }, |_| {
                    Err("fixture sign-out warning".into())
                })
                .unwrap();
            assert_eq!(result.purge_receipt.revoked, vec![lineage]);
            assert_eq!(result.purge_receipt.removed_namespaces, 1);
            assert!(result.warning.unwrap().contains("fixture sign-out warning"));
            assert!(
                result
                    .registry
                    .find(account.registry_key().unwrap())
                    .is_none()
            );
            assert!(matches!(
                originals.original(Some("user"), 0, 4),
                Err(crate::chat::annotations::OriginalError::Unavailable(_))
            ));
        }
        let fixture = PurgeFixture::new();
        let account = fixture.managed();
        std::fs::remove_dir_all(account.resolved_home().unwrap()).unwrap();
        let result = fixture
            .remove(Operation::DeleteFiles { id: account.id() }, |_| Ok(()))
            .unwrap();
        assert!(result.purge_receipt.revoked.is_empty());
        assert!(
            result
                .warning
                .unwrap()
                .contains("No prior physical annotation association")
        );
    }

    #[test]
    fn lifecycle_no_purge_paths_preserve_originals_and_empty_receipts() {
        let fixture = PurgeFixture::new();
        let account = fixture.managed();
        let originals = fixture.originals(&account);
        let target = fixture.managed();
        for operation in [
            Operation::Remove { id: account.id() },
            Operation::MoveAndArchive {
                id: account.id(),
                target: target.id(),
            },
        ] {
            let result = fixture.remove(operation, |_| Ok(())).unwrap();
            assert!(result.purge_receipt.revoked.is_empty());
            assert_eq!(result.purge_receipt.removed_namespaces, 0);
            assert!(result.registry.is_archived(account.registry_key().unwrap()));
            assert_eq!(originals.original(Some("user"), 0, 4).unwrap().text, "text");
        }
        // A persisted linked alias protects the managed home. The checked
        // callback must be skipped even though DeleteFiles itself succeeds.
        let fixture = PurgeFixture::new();
        let account = fixture.managed();
        let originals = fixture.originals(&account);
        let alias_key = uuid::Uuid::new_v4();
        let home = account.resolved_home().unwrap();
        fixture
            .store
            .update(|registry| {
                registry
                    .register(
                        crate::account::registry::Registered {
                            key: alias_key,
                            provider: Provider::Claude,
                            linked_home: Some(home.clone()),
                            label: None,
                            email: None,
                            organization: None,
                            added_at: 0,
                        },
                        &[],
                    )
                    .unwrap()
            })
            .unwrap();
        let result = fixture
            .remove(Operation::DeleteFiles { id: account.id() }, |_| Ok(()))
            .unwrap();
        assert!(result.purge_receipt.revoked.is_empty());
        assert!(result.warning.unwrap().contains("another account"));
        assert!(home.exists());
        assert_eq!(originals.original(Some("user"), 0, 4).unwrap().text, "text");
        let alias = crate::account::Account::linked(Provider::Claude, alias_key, home);
        let result = fixture
            .remove(Operation::DeleteFiles { id: alias.id() }, |_| {
                panic!("linked files cannot be deleted")
            })
            .unwrap();
        assert!(result.purge_receipt.revoked.is_empty());
        assert_eq!(originals.original(Some("user"), 0, 4).unwrap().text, "text");
    }

    #[test]
    fn lifecycle_probe_refusal_keeps_receipt_empty_and_originals_readable() {
        let fixture = PurgeFixture::new();
        let account = fixture.managed();
        let originals = fixture.originals(&account);
        let before = fixture.store.load().unwrap();
        let _lock = fixture
            .cache
            .try_probe_lock(&account.lock_name())
            .unwrap()
            .unwrap();
        let failure = fixture
            .remove(Operation::DeleteFiles { id: account.id() }, |_| {
                panic!("probe refusal precedes sign-out")
            })
            .unwrap_err();
        assert!(failure.message.contains("being checked or changed"));
        assert!(failure.purge_receipt.revoked.is_empty());
        assert_eq!(fixture.store.load().unwrap(), before);
        assert_eq!(originals.original(Some("user"), 0, 4).unwrap().text, "text");
    }

    #[test]
    fn lifecycle_purge_failure_preserves_marker_effect_without_committing_removal() {
        use std::os::unix::fs::symlink;
        let fixture = PurgeFixture::new();
        let account = fixture.managed();
        let originals = fixture.originals(&account);
        let lineage = originals.original_lineage();
        let markers = fixture.annotations.join("revoked-accounts");
        crate::secure_file::prepare_private_directory(&markers).unwrap();
        symlink(
            fixture.annotations.join("absent-target"),
            markers.join(lineage.as_str()),
        )
        .unwrap();
        let failure = fixture
            .remove(Operation::DeleteFiles { id: account.id() }, |_| Ok(()))
            .unwrap_err();
        assert!(!failure.message.is_empty());
        assert_eq!(failure.purge_receipt.revoked, vec![lineage]);
        assert_eq!(failure.purge_receipt.removed_namespaces, 0);
        assert!(
            fixture
                .store
                .load()
                .unwrap()
                .find(account.registry_key().unwrap())
                .is_some()
        );
        assert!(account.resolved_home().unwrap().exists());
        assert!(matches!(
            originals.original(Some("user"), 0, 4),
            Err(crate::chat::annotations::OriginalError::Unavailable(_))
        ));
    }

    #[test]
    fn lifecycle_registry_publication_failure_retains_completed_purge_receipt() {
        use crate::chat::annotations::storage;
        let fixture = PurgeFixture::new();
        let account = fixture.managed();
        let originals = fixture.originals(&account);
        let lineage = originals.original_lineage();
        let registry_path = fixture._directory.path().join("config/accounts.toml");
        let retained_registry = fixture._directory.path().join("retained-accounts.toml");
        let failure = remove_account_with_purge(
            Operation::DeleteFiles { id: account.id() },
            &fixture.store,
            &fixture.homes,
            &fixture.cache,
            |_| Ok(()),
            |account| {
                let result = purge_deleted_account_annotations_with(
                    account,
                    |scope| storage::purge_account_receipt_at(&fixture.annotations, scope),
                    |id| storage::purge_registered_receipt_at(&fixture.annotations, id),
                )?;
                // A completed real purge precedes registry publication. An
                // owned directory collision deterministically refuses persist.
                std::fs::rename(&registry_path, &retained_registry).unwrap();
                std::fs::create_dir(&registry_path).unwrap();
                Ok(result)
            },
        )
        .unwrap_err();
        assert!(!failure.message.is_empty());
        assert!(
            registry_path.is_dir(),
            "failure followed the injected publication collision"
        );
        assert_eq!(failure.purge_receipt.revoked, vec![lineage]);
        assert_eq!(failure.purge_receipt.removed_namespaces, 1);
        assert!(
            account.resolved_home().unwrap().exists(),
            "parking never ran"
        );
        std::fs::remove_dir(&registry_path).unwrap();
        std::fs::rename(&retained_registry, &registry_path).unwrap();
        assert!(
            fixture
                .store
                .load()
                .unwrap()
                .find(account.registry_key().unwrap())
                .is_some()
        );
        assert!(matches!(
            originals.original(Some("user"), 0, 4),
            Err(crate::chat::annotations::OriginalError::Unavailable(_))
        ));
    }

    #[test]
    fn lifecycle_installation_store_delete_does_not_reenter_its_registry_lock() {
        const CHILD: &str = "CAGENTS_PURGE_INSTALLATION_LOCK_TEST";
        const SELECTOR: &str = "account::remote::tests::lifecycle_installation_store_delete_does_not_reenter_its_registry_lock";
        if std::env::var_os(CHILD).is_none() {
            use std::{
                io::Read,
                process::{Command, Stdio},
                time::{Duration, Instant},
            };
            let directory = tempfile::tempdir().unwrap();
            for name in ["home", "config", "data", "cache", "runtime"] {
                std::fs::create_dir(directory.path().join(name)).unwrap();
            }
            // Only the same-ELF child gets these variables. The parent process
            // and its installation state are never redirected or mutated.
            struct ChildGuard(std::process::Child);
            impl Drop for ChildGuard {
                fn drop(&mut self) {
                    let _ = self.0.kill();
                    let _ = self.0.wait();
                }
            }
            let mut child = ChildGuard(
                Command::new(std::env::current_exe().unwrap())
                    .args(["--exact", SELECTOR, "--nocapture", "--test-threads=1"])
                    .env_clear()
                    .env(CHILD, directory.path())
                    .env("HOME", directory.path().join("home"))
                    .env("XDG_CONFIG_HOME", directory.path().join("config"))
                    .env("XDG_DATA_HOME", directory.path().join("data"))
                    .env("XDG_CACHE_HOME", directory.path().join("cache"))
                    .env("XDG_RUNTIME_DIR", directory.path().join("runtime"))
                    .current_dir(directory.path())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .spawn()
                    .unwrap(),
            );
            let deadline = Instant::now() + Duration::from_secs(5);
            let status = loop {
                if let Some(status) = child.0.try_wait().unwrap() {
                    break Some(status);
                }
                if Instant::now() >= deadline {
                    child.0.kill().unwrap();
                    child.0.wait().unwrap();
                    break None;
                }
                std::thread::sleep(Duration::from_millis(10));
            };
            let mut stdout = String::new();
            let mut stderr = String::new();
            child
                .0
                .stdout
                .take()
                .unwrap()
                .read_to_string(&mut stdout)
                .unwrap();
            child
                .0
                .stderr
                .take()
                .unwrap()
                .read_to_string(&mut stderr)
                .unwrap();
            assert!(
                status.is_some_and(|status| status.success()),
                "same-store DeleteFiles must finish without registry recursion within 5s; status={status:?}\n{stdout}\n{stderr}"
            );
            return;
        }
        // The actual installation constructors select exactly the same store
        // a general LaunchAccountScope::resolve would try to lock recursively.
        let root = std::path::PathBuf::from(std::env::var_os(CHILD).unwrap());
        let fixture = PurgeFixture {
            _directory: tempfile::tempdir_in(&root).unwrap(),
            homes: crate::account::home::Homes::installation().unwrap(),
            store: crate::account::registry::Store::installation().unwrap(),
            cache: crate::account::cache::Cache::installation().unwrap(),
            annotations: crate::instance::project_dirs()
                .unwrap()
                .data_dir()
                .join("conversation-annotations"),
        };
        let account = fixture.managed();
        let originals = fixture.originals(&account);
        let lineage = originals.original_lineage();
        let result = remove_account_with_receipt(
            Operation::DeleteFiles { id: account.id() },
            &fixture.store,
            &fixture.homes,
            &fixture.cache,
            |_| Ok(()),
        )
        .unwrap();
        assert_eq!(result.purge_receipt.revoked, vec![lineage]);
        assert_eq!(result.purge_receipt.removed_namespaces, 1);
        assert!(result.warning.is_none());
        assert!(
            result
                .registry
                .find(account.registry_key().unwrap())
                .is_none()
        );
        assert!(!account.resolved_home().unwrap().exists());
        assert!(matches!(
            originals.original(Some("user"), 0, 4),
            Err(crate::chat::annotations::OriginalError::Unavailable(_))
        ));
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
                _ => Operation::DeleteFiles { id: source.id() },
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
        let attachment = annotations.retain_attachment(&image).unwrap();
        let retained_image = attachment.path.clone();
        let original = OriginalPrompt {
            user_id: "confirmed-user".into(),
            turn_id: "confirmed-turn".into(),
            text_bytes: 16,
            attachments: vec![attachment],
            settings: OriginalSettings {
                model: Setting::Default,
                reasoning: Setting::Default,
                fast: Setting::Unsupported,
                permission: Setting::Default,
            },
        };
        annotations
            .accept(&original, "original request", &[])
            .unwrap();
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
            Operation::DeleteFiles { id: account.id() },
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
                let annotations =
                    storage::Store::installation(&scope, "confirmed-absent-session").unwrap();
                Some(annotations.retain_attachment(&image).unwrap().path)
            } else {
                None
            };
            std::fs::remove_dir_all(&home).unwrap();
            let (registry, warning) = remove_account(
                Operation::DeleteFiles { id: account.id() },
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
