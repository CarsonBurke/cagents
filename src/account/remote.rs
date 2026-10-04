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
pub(crate) fn affected_account(operation: &Operation) -> Result<Option<super::Account>, String> {
    use super::{Account, home::Homes, registry::Store};
    let homes = Homes::installation().ok_or("Remote account storage is unavailable")?;
    let registry = Store::installation()
        .ok_or("Remote account registry is unavailable")?
        .load()
        .map_err(|error| error.to_string())?;
    match operation {
        Operation::Register { provider, key } | Operation::Abandon { provider, key } => {
            if !registry
                .pending()
                .iter()
                .any(|pending| pending.provider == *provider && pending.key == *key)
            {
                return Err("The pending sign-in changed on the device".into());
            }
            Ok(Some(Account::managed(
                *provider,
                *key,
                homes.path(*provider, *key),
            )))
        }
        Operation::Remove { id } => registry
            .accounts(Some(&homes))
            .into_iter()
            .find(|account| account.id() == *id)
            .map(Some)
            .ok_or("The device account was removed".into()),
        _ => Ok(None),
    }
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
            let program = probe::find_executable(provider.executable(), search.as_deref())
                .ok_or("The provider CLI is not installed on this device")?;
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
            let recipe = match login_recipe(&account, program) {
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
            if identity.is_none()
                && !matches!(
                    snapshot.status,
                    UsageStatus::Ok | UsageStatus::Warning | UsageStatus::Limited
                )
            {
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
        Operation::Remove { id } => {
            let registry = store.load().map_err(|error| error.to_string())?;
            let account = registry
                .accounts(Some(&homes))
                .into_iter()
                .find(|account| account.id() == id)
                .ok_or("The account was already removed from this device")?;
            let key = account
                .registry_key()
                .ok_or("System accounts cannot be removed")?;
            let (registry, error) = if account.is_linked() {
                let (registry, ()) = store
                    .update(|registry| {
                        registry.unregister(key);
                    })
                    .map_err(|error| error.to_string())?;
                (registry, None)
            } else {
                runtime.block_on(probe::logout(account.clone()))?;
                store
                    .remove_home(&homes, account.provider, key, true)
                    .map_err(|error| error.to_string())?
            };
            serde_json::to_value((registry, error)).map_err(|error| error.to_string())
        }
    }
}

#[cfg(feature = "host")]
fn login_recipe(
    account: &super::Account,
    program: std::path::PathBuf,
) -> Result<crate::model::LaunchRecipe, String> {
    let mut environment = std::collections::BTreeMap::new();
    if let (Some(variable), Some(home)) = (
        super::home_variable(account.provider),
        account.resolved_home(),
    ) {
        environment.insert(variable.into(), home.display().to_string());
    }
    if let Some(path) = super::probe::shell_search_path().and_then(|path| path.to_str()) {
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

#[cfg(test)]
#[cfg(feature = "host")]
mod tests {
    use super::*;

    #[test]
    fn host_lifecycle_creates_isolated_login_and_never_deletes_linked_files() {
        const CHILD: &str = "CAGENTS_REMOTE_ACCOUNT_TEST";
        if std::env::var_os(CHILD).is_none() {
            use std::os::unix::fs::PermissionsExt;
            let directory = tempfile::tempdir().unwrap();
            let root = directory.path();
            std::fs::create_dir_all(root.join("home/.codex")).unwrap();
            std::fs::create_dir_all(root.join("bin")).unwrap();
            std::fs::write(root.join("home/.codex/auth.json"), "fixture-private-login").unwrap();
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
        assert!(!created.home.join("auth.json").exists());
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
        let linked = root.join("home/linked-account");
        std::fs::create_dir_all(&linked).unwrap();
        std::fs::write(linked.join("auth.json"), "linked-private-login").unwrap();
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
            std::fs::read_to_string(linked.join("auth.json")).unwrap(),
            "linked-private-login"
        );
        assert!(store.load().unwrap().find(key).is_none());
    }
}
