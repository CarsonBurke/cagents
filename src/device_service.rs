//! Filesystem and Git operations executed by the selected daemon, rather than
//! interpreting a remote path on the computer displaying the window.
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

pub const MAX_TEXT_BYTES: u64 = 512 * 1024;
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Operation {
    Home,
    AccountsRead,
    AccountLifecycle {
        operation: crate::account::remote::Operation,
    },
    AccountsCache {
        reports: serde_json::Value,
    },
    AccountsUpdate {
        expected_revision: u64,
        registry: serde_json::Value,
    },
    AccountLogin {
        account: crate::account::Account,
    },
    AccountProbe {
        account: crate::account::Account,
        force: bool,
    },
    Discover {
        path: PathBuf,
    },
    List {
        path: PathBuf,
    },
    Read {
        root: PathBuf,
        path: PathBuf,
    },
    /// Expected bytes make saving optimistic: changed files are never silently
    /// overwritten after the user's read.
    Write {
        root: PathBuf,
        path: PathBuf,
        expected: String,
        content: String,
    },
    Git {
        root: PathBuf,
        operation: GitOperation,
    },
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum GitOperation {
    Status,
    Diff { staged: bool },
    Stage { paths: Vec<PathBuf> },
    Unstage { paths: Vec<PathBuf> },
    Commit { message: String },
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    pub name: String,
    pub directory: bool,
    pub symlink: bool,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Reply {
    Home {
        path: PathBuf,
        shell: String,
    },
    Accounts(serde_json::Value),
    AccountProbe(serde_json::Value),
    AccountRegistry(serde_json::Value),
    AccountLifecycle(serde_json::Value),
    Login(crate::model::LaunchRecipe),
    Project {
        path: PathBuf,
        key: PathBuf,
        kind: crate::model::ProjectKind,
    },
    Directory {
        path: PathBuf,
        entries: Vec<Entry>,
    },
    Text(String),
    Saved,
}

#[cfg(feature = "host")]
pub(crate) fn execute(operation: Operation) -> Result<Reply, String> {
    use crate::fs_safety;
    use std::{fs, io::Write};
    match operation {
        Operation::AccountsRead => accounts(),
        Operation::AccountLifecycle { operation } => {
            crate::account::remote::execute(operation).map(Reply::AccountLifecycle)
        }
        Operation::AccountsCache { reports } => {
            use crate::account::{
                Account,
                cache::{Cache, Catalog, Entry},
            };
            let (entries, catalogs): (
                Vec<(Account, Entry)>,
                Vec<(crate::agent::Provider, Catalog)>,
            ) = serde_json::from_value(reports).map_err(|error| error.to_string())?;
            let cache = Cache::installation().ok_or("The device account cache is unavailable")?;
            for (account, entry) in entries {
                if let Some(key) = account.cache_key() {
                    cache
                        .store(&key, entry)
                        .map_err(|error| error.to_string())?;
                }
            }
            for (provider, catalog) in catalogs {
                cache
                    .store_catalog(provider, catalog)
                    .map_err(|error| error.to_string())?;
            }
            Ok(Reply::Saved)
        }
        Operation::AccountsUpdate {
            expected_revision,
            registry,
        } => update_accounts(expected_revision, registry),
        Operation::AccountLogin { account } => account_login(account),
        Operation::AccountProbe { account, force } => {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|error| error.to_string())?;
            let outcome = runtime.block_on(crate::account::probe::run(
                account,
                crate::account::cache::Cache::installation(),
                force,
            ));
            serde_json::to_value(outcome)
                .map(Reply::AccountProbe)
                .map_err(|error| error.to_string())
        }
        Operation::Home => {
            let path = std::env::var_os("HOME")
                .map(PathBuf::from)
                .ok_or("remote HOME is unavailable")?;
            let shell = crate::model::login_shell().map_err(|error| error.to_string())?;
            Ok(Reply::Home { path, shell })
        }
        Operation::Discover { path } => {
            let path = fs::canonicalize(path).map_err(|error| error.to_string())?;
            let location = crate::project_location::discover(&path)?;
            Ok(Reply::Project {
                path,
                key: location.key,
                kind: location.kind,
            })
        }
        Operation::List { path } => {
            let path = fs::canonicalize(path).map_err(|error| error.to_string())?;
            let mut entries = Vec::new();
            for entry in fs::read_dir(&path)
                .map_err(|error| error.to_string())?
                .take(4097)
            {
                let entry = entry.map_err(|error| error.to_string())?;
                let kind = entry.file_type().map_err(|error| error.to_string())?;
                let name = entry
                    .file_name()
                    .into_string()
                    .map_err(|_| "directory contains a non-UTF-8 name")?;
                entries.push(Entry {
                    name,
                    directory: kind.is_dir(),
                    symlink: kind.is_symlink(),
                });
            }
            if entries.len() > 4096 {
                return Err("directory has more than 4096 entries; open a narrower path".into());
            }
            entries.sort_by(|a, b| {
                b.directory
                    .cmp(&a.directory)
                    .then_with(|| a.name.cmp(&b.name))
            });
            Ok(Reply::Directory { path, entries })
        }
        Operation::Read { root, path } => {
            let root = fs_safety::Root::open(&root).map_err(|error| error.to_string())?;
            Ok(Reply::Text(read(&root, &path)?))
        }
        Operation::Write {
            root,
            path,
            expected,
            content,
        } => {
            if content.len() as u64 > MAX_TEXT_BYTES {
                return Err("text exceeds remote editor limit (512 KiB)".into());
            }
            let root = fs_safety::Root::open(&root).map_err(|error| error.to_string())?;
            if read(&root, &path)? != expected {
                return Err("file changed on the device; reload before saving".into());
            }
            let original =
                fs_safety::open_beneath(&root, &path).map_err(|error| error.to_string())?;
            let metadata = original.metadata().map_err(|error| error.to_string())?;
            let (parent, target) =
                fs_safety::open_parent_beneath(&root, &path).map_err(|error| error.to_string())?;
            use std::os::unix::fs::PermissionsExt;
            let (mut file, temporary) =
                fs_safety::create_temporary_in(&parent, metadata.permissions().mode())
                    .map_err(|error| error.to_string())?;
            let saved = (|| {
                file.write_all(content.as_bytes())
                    .map_err(|error| error.to_string())?;
                file.sync_all().map_err(|error| error.to_string())?;
                // Recheck after writing the temporary, before replacing disk bytes.
                if read(&root, &path)? != expected {
                    return Err("file changed on the device; reload before saving".into());
                }
                fs_safety::rename_within(&parent, &temporary, &target)
                    .map_err(|error| error.to_string())?;
                fs_safety::sync_directory(&parent).map_err(|error| error.to_string())
            })();
            if saved.is_err() {
                let _ = fs_safety::unlink_within(&parent, &temporary);
            }
            saved.map(|_| Reply::Saved)
        }
        Operation::Git { root, operation } => git(root, operation).map(Reply::Text),
    }
}
#[cfg(feature = "host")]
fn read(root: &crate::fs_safety::Root, path: &std::path::Path) -> Result<String, String> {
    let mut file = crate::fs_safety::open_beneath(root, path).map_err(|error| error.to_string())?;
    if !file
        .metadata()
        .map_err(|error| error.to_string())?
        .is_file()
    {
        return Err("only regular text files can be edited".into());
    }
    let bytes = crate::fs_safety::read_bounded(&mut file, MAX_TEXT_BYTES)
        .map_err(|error| error.to_string())?;
    if bytes.contains(&0) {
        return Err("binary files cannot be edited".into());
    }
    String::from_utf8(bytes).map_err(|_| "file is not UTF-8 text".into())
}
#[cfg(feature = "host")]
fn git(root: PathBuf, operation: GitOperation) -> Result<String, String> {
    use crate::process::{CancellationToken, CommandSpec, ProcessExecutor, SystemProcessExecutor};
    let root = std::fs::canonicalize(root).map_err(|error| error.to_string())?;
    let mut command =
        CommandSpec::new("git")
            .cwd(root)
            .args(["--no-pager", "-c", "color.ui=false"]);
    command.stdout_limit = MAX_TEXT_BYTES as usize;
    command.timeout = Some(std::time::Duration::from_secs(30));
    let staging = matches!(&operation, GitOperation::Stage { .. });
    command = match operation {
        GitOperation::Status => command.args(["status", "--short", "--branch"]),
        GitOperation::Diff { staged } => {
            let command = command.args(["diff", "--no-ext-diff", "--no-textconv"]);
            if staged {
                command.arg("--cached")
            } else {
                command
            }
        }
        GitOperation::Stage { paths } | GitOperation::Unstage { paths } => {
            if paths.is_empty()
                || paths.len() > 256
                || paths.iter().any(|path| {
                    path.is_absolute()
                        || path
                            .components()
                            .any(|part| matches!(part, std::path::Component::ParentDir))
                        || path.as_os_str().is_empty()
                })
            {
                return Err("select 1–256 worktree-relative paths".into());
            }
            let command = if staging {
                command.args(["--literal-pathspecs", "add", "--"])
            } else {
                command.args(["--literal-pathspecs", "restore", "--staged", "--"])
            };
            command.args(paths)
        }
        GitOperation::Commit { message } => {
            if message.trim().is_empty() || message.len() > 16 * 1024 {
                return Err("commit message must contain 1–16384 bytes".into());
            }
            command.args(["commit", "-m"]).arg(message)
        }
    };
    let output = SystemProcessExecutor
        .execute(&command, &CancellationToken::new())
        .map_err(|error| error.to_string())?;
    if !output.success {
        return Err(String::from_utf8_lossy(&output.stderr).into_owned());
    }
    String::from_utf8(output.stdout).map_err(|_| "Git output is not UTF-8".into())
}
#[cfg(all(test, feature = "host"))]
mod tests {
    use super::*;
    #[test]
    fn writes_remote_file_and_refuses_conflicts_and_escape() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_owned();
        std::fs::write(root.join("file"), "before").unwrap();
        assert_eq!(
            execute(Operation::Write {
                root: root.clone(),
                path: "file".into(),
                expected: "before".into(),
                content: "after".into()
            })
            .unwrap(),
            Reply::Saved
        );
        assert_eq!(std::fs::read_to_string(root.join("file")).unwrap(), "after");
        assert!(
            execute(Operation::Write {
                root: root.clone(),
                path: "file".into(),
                expected: "before".into(),
                content: "stale".into()
            })
            .is_err()
        );
        assert!(
            execute(Operation::Read {
                root: root.clone(),
                path: "../file".into()
            })
            .is_err()
        );
        std::os::unix::fs::symlink("/etc/passwd", root.join("link")).unwrap();
        assert!(
            execute(Operation::Read {
                root,
                path: "link".into()
            })
            .is_err()
        );
    }
}

/// Convenience boundary for background UI/CLI tasks; all paths are interpreted
/// by the daemon selected for this process.
pub fn request(operation: Operation) -> Result<Reply, String> {
    let socket = crate::terminal::ensure_daemon_ready().map_err(|error| error.to_string())?;
    crate::control::ControlClient::connect(&socket)
        .map_err(|error| error.to_string())?
        .device_operation(operation)
        .map_err(|error| error.to_string())
}
#[cfg(any(feature = "ui", feature = "host"))]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountSnapshot {
    pub registry: crate::account::registry::Registry,
    pub homes: Option<crate::account::home::Homes>,
    pub system_homes: std::collections::BTreeMap<crate::agent::Provider, PathBuf>,
    pub entries: std::collections::BTreeMap<String, crate::account::cache::Entry>,
    pub catalogs:
        std::collections::BTreeMap<crate::agent::Provider, crate::account::cache::Catalog>,
    pub installed: std::collections::BTreeMap<crate::agent::Provider, bool>,
}
#[cfg(feature = "host")]
fn accounts() -> Result<Reply, String> {
    use crate::account::{cache::Cache, home::Homes, probe, registry::Store};
    let homes = Homes::installation();
    let registry = match (Store::installation(), homes.as_ref()) {
        (Some(store), Some(homes)) => store.load_and_sweep(homes),
        (Some(store), None) => store.load(),
        _ => Ok(Default::default()),
    }
    .map_err(|error| error.to_string())?;
    let now = crate::chat::now_ms().unwrap_or_default();
    let cache = Cache::installation();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| error.to_string())?;
    let search_path = runtime.block_on(probe::search_path());
    let installed = [
        crate::agent::Provider::Claude,
        crate::agent::Provider::Codex,
        crate::agent::Provider::Omp,
    ]
    .into_iter()
    .map(|provider| {
        (
            provider,
            probe::find_executable(provider.executable(), search_path.as_deref()).is_some(),
        )
    })
    .collect();
    let snapshot = AccountSnapshot {
        registry,
        homes,
        system_homes: crate::account::Account::PROVIDERS
            .into_iter()
            .filter_map(|provider| {
                crate::account::Account::system(provider)
                    .resolved_home()
                    .map(|home| (provider, home))
            })
            .collect(),
        entries: cache
            .as_ref()
            .map(|cache| cache.load(now))
            .unwrap_or_default(),
        catalogs: cache
            .as_ref()
            .map(|cache| cache.catalogs(now))
            .unwrap_or_default(),
        installed,
    };
    serde_json::to_value(snapshot)
        .map(Reply::Accounts)
        .map_err(|error| error.to_string())
}

#[cfg(feature = "host")]
fn update_accounts(expected_revision: u64, proposal: serde_json::Value) -> Result<Reply, String> {
    use crate::account::registry::{Registry, Store};
    let proposed: Registry = serde_json::from_value(proposal).map_err(|error| error.to_string())?;
    let store = Store::installation().ok_or("remote account registry is unavailable")?;
    let (registry, result) = store
        .update(|current| {
            if current.revision() != expected_revision {
                return Err("accounts changed on the device; reload before saving".to_owned());
            }
            // This endpoint edits preferences. Credential-home ownership and
            // pending create/remove transactions cannot be replaced by a client.
            let strip_preferences = |registry: &Registry| -> Result<serde_json::Value, String> {
                let mut value =
                    serde_json::to_value(registry).map_err(|error| error.to_string())?;
                let document = value.as_object_mut().ok_or("invalid account registry")?;
                for key in ["defaults", "project_defaults", "system_labels", "archived"] {
                    document.remove(key);
                }
                if let Some(accounts) = document
                    .get_mut("account")
                    .and_then(|accounts| accounts.as_array_mut())
                {
                    for account in accounts {
                        if let Some(account) = account.as_object_mut() {
                            for key in ["label", "email", "organization"] {
                                account.remove(key);
                            }
                        }
                    }
                }
                Ok(value)
            };
            if strip_preferences(current)? != strip_preferences(&proposed)? {
                return Err(
                    "remote preference update cannot add, remove or redirect account homes".into(),
                );
            }
            *current = proposed;
            Ok(())
        })
        .map_err(|error| error.to_string())?;
    result?;
    serde_json::to_value(registry)
        .map(Reply::AccountRegistry)
        .map_err(|error| error.to_string())
}
#[cfg(feature = "host")]
fn account_login(account: crate::account::Account) -> Result<Reply, String> {
    use crate::account::{home::Homes, probe, registry::Store};
    use crate::agent::Provider;
    let homes = Homes::installation();
    let registry = Store::installation()
        .map(|store| store.load())
        .transpose()
        .map_err(|error| error.to_string())?
        .unwrap_or_default();
    let account = registry
        .all_accounts(homes.as_ref())
        .into_iter()
        .find(|registered| {
            registered.id() == account.id() && registered.resolved_home() == account.resolved_home()
        })
        .ok_or("account is not registered on this device")?;
    let arguments = match account.provider {
        Provider::Claude => vec!["auth".into(), "login".into(), "--claudeai".into()],
        Provider::Codex => vec!["login".into(), "--device-auth".into()],
        Provider::Omp => return Err("open an Omp terminal on this device and use /login".into()),
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| error.to_string())?;
    let search_path = runtime.block_on(probe::search_path());
    let program = probe::find_executable(account.provider.executable(), search_path.as_deref())
        .ok_or_else(|| {
            format!(
                "{} is not installed on this device",
                account.provider.label()
            )
        })?;
    let mut environment = std::collections::BTreeMap::new();
    if let (Some(variable), Some(home)) = (
        crate::account::home_variable(account.provider),
        account.resolved_home(),
    ) {
        environment.insert(variable.to_owned(), home.display().to_string());
    }
    if let Some(path) = search_path.and_then(|path| path.into_string().ok()) {
        environment.insert("PATH".into(), path);
    }
    Ok(Reply::Login(crate::model::LaunchRecipe {
        program: program
            .into_os_string()
            .into_string()
            .map_err(|_| "provider executable is not UTF-8")?,
        arguments,
        working_directory: std::env::var_os("HOME")
            .map(PathBuf::from)
            .ok_or("remote HOME is unavailable")?,
        environment,
        unset_environment: account
            .credential_overrides()
            .iter()
            .map(|name| (*name).to_owned())
            .collect(),
    }))
}
