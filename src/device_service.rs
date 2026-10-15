//! Filesystem and Git operations executed by the selected daemon, rather than
//! interpreting a remote path on the computer displaying the window.
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

pub const MAX_TEXT_BYTES: u64 = 512 * 1024;
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Operation {
    Home,
    /// Optional cleanup is authorized only by the daemon lifecycle fence.
    RemoveMergedWorktree {
        request: crate::worktree_cleanup::RemovalRequest,
    },
    ProjectNavigation {
        repository: PathBuf,
        kind: crate::model::ProjectKind,
    },
    AccountsRead,
    /// Refresh stale alternates before an automatic send gives up on routing.
    AccountRoute {
        id: crate::account::AccountId,
    },
    AccountLifecycle {
        operation: crate::account::remote::Operation,
    },
    AccountsCache {
        #[serde(with = "wire_json")]
        reports: serde_json::Value,
    },
    Skills {
        launch: crate::chat::Launch,
    },
    AccountsUpdate {
        expected_revision: u64,
        #[serde(with = "wire_json")]
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
    /// overwritten by another device-window save after the user's read.
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
    Worktrees,
    CreateWorktree { name: String, branch: String },
    RemoveWorktree { name: String },
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
        environment: std::collections::BTreeMap<
            crate::agent::Provider,
            std::collections::BTreeMap<String, String>,
        >,
    },
    Accounts(#[serde(with = "wire_json")] serde_json::Value),
    Skills(Vec<crate::chat::Skill>),
    AccountProbe(#[serde(with = "wire_json")] serde_json::Value),
    AccountRegistry(#[serde(with = "wire_json")] serde_json::Value),
    AccountLifecycle(#[serde(with = "wire_json")] serde_json::Value),
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
    MergedWorktreeRemoved(Option<crate::worktree_cleanup::RemovedWorktree>),
    ProjectNavigation(Navigation),
    /// The replacement is published, but its directory entry could not be
    /// synced. Callers must advance their disk baseline and show this warning.
    SavedUnconfirmed {
        error: String,
    },
}

/// Daemon-resolved directories used for workspace navigation. No displaying
/// client may reinterpret these paths through its own filesystem.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Navigation {
    pub repository: PathBuf,
    pub worktrees: Vec<NavigationWorktree>,
}
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct NavigationWorktree {
    pub path: PathBuf,
    pub branch: Option<String>,
    pub head_oid: Option<String>,
    pub detached: bool,
    pub bare: bool,
    pub locked: bool,
    pub lock_reason: Option<String>,
    pub prunable_reason: Option<String>,
}

/// JSON's self-describing value visitor cannot run on Postcard. Keep JSON
/// schemas at the service boundary, but frame each value as an ordinary string
/// on the binary IPC wire. Human-readable serializers keep their existing form.
mod wire_json {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S: Serializer>(
        value: &serde_json::Value,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        if serializer.is_human_readable() {
            value.serialize(serializer)
        } else {
            serde_json::to_string(value)
                .map_err(serde::ser::Error::custom)?
                .serialize(serializer)
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<serde_json::Value, D::Error> {
        if deserializer.is_human_readable() {
            serde_json::Value::deserialize(deserializer)
        } else {
            let encoded = String::deserialize(deserializer)?;
            serde_json::from_str(&encoded).map_err(serde::de::Error::custom)
        }
    }
}

#[cfg(feature = "host")]
pub(crate) fn execute(operation: Operation) -> Result<Reply, String> {
    use crate::fs_safety;
    use std::fs;
    match operation {
        Operation::RemoveMergedWorktree { .. } => {
            Err("Automatic cleanup requires the daemon lifecycle fence".into())
        }
        Operation::ProjectNavigation { repository, kind } => {
            navigation(repository, kind).map(Reply::ProjectNavigation)
        }
        Operation::AccountsRead => accounts(),
        Operation::AccountRoute { id } => {
            use crate::account::{cache::Cache, home::Homes, registry::Store};
            let registry = Store::installation()
                .ok_or("Account registry is unavailable")?
                .load()
                .map_err(|error| error.to_string())?;
            let homes = Homes::installation();
            let preferred = registry
                .all_accounts(homes.as_ref())
                .into_iter()
                .find(|account| account.id() == id)
                .ok_or("The source account was removed")?;
            let cache = Cache::installation();
            let now = crate::chat::now_ms().unwrap_or_default();
            let mut entries = cache
                .as_ref()
                .map(|cache| cache.load(now))
                .unwrap_or_default();
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|error| error.to_string())?;
            runtime.block_on(crate::account::routing_refresh::refresh(
                &registry,
                homes.as_ref(),
                &preferred,
                cache,
                &mut entries,
                now,
            ));
            accounts()
        }
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
        Operation::Skills { launch } => Ok(Reply::Skills(crate::chat::skills::discover(&launch))),
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
            Ok(Reply::Home {
                path,
                shell,
                environment: crate::agent::Provider::ALL
                    .into_iter()
                    .map(|provider| {
                        (
                            provider,
                            crate::model::agent_recovery::fresh_native_environment(provider, true),
                        )
                    })
                    .collect(),
            })
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
        } => write_with_sync(&root, &path, &expected, &content, fs_safety::sync_directory),
        Operation::Git { root, operation } => git(root, operation).map(Reply::Text),
    }
}

#[cfg(feature = "host")]
fn write_with_sync(
    root: &std::path::Path,
    path: &std::path::Path,
    expected: &str,
    content: &str,
    sync_parent: impl FnOnce(&crate::fs_safety::Root) -> std::io::Result<()>,
) -> Result<Reply, String> {
    use crate::fs_safety;
    use std::io::Write;
    let canonical_root = std::fs::canonicalize(root).map_err(|error| error.to_string())?;
    let lock = mutation_lock(canonical_root.join(path));
    let _guard = lock.lock().unwrap_or_else(|poison| poison.into_inner());
    if content.len() as u64 > MAX_TEXT_BYTES {
        return Err("text exceeds remote editor limit (512 KiB)".into());
    }
    let root = fs_safety::Root::open(root).map_err(|error| error.to_string())?;
    if read(&root, path)? != expected {
        return Err("file changed on the device; reload before saving".into());
    }
    let original = fs_safety::open_beneath(&root, path).map_err(|error| error.to_string())?;
    let metadata = original.metadata().map_err(|error| error.to_string())?;
    let (parent, target) =
        fs_safety::open_parent_beneath(&root, path).map_err(|error| error.to_string())?;
    let (mut file, temporary) =
        fs_safety::create_temporary_in(&parent, 0o600).map_err(|error| error.to_string())?;
    let published = (|| {
        // Keep the replacement private until the source's access rules
        // are installed, before writing any sensitive content.
        fs_safety::copy_ownership(&original, &file).map_err(|error| error.to_string())?;
        fs_safety::copy_access_acl(&original, &file).map_err(|error| error.to_string())?;
        file.write_all(content.as_bytes())
            .map_err(|error| error.to_string())?;
        // Creation applies the daemon's umask and writing can clear set-ID bits.
        // Restore the exact source mode after writing, then its ACL because
        // chmod rewrites the ACL mask.
        file.set_permissions(metadata.permissions())
            .map_err(|error| error.to_string())?;
        fs_safety::copy_access_acl(&original, &file).map_err(|error| error.to_string())?;
        if !fs_safety::copy_extended_attributes(&original, &file).is_empty() {
            return Err("remote save cannot preserve this file's extended attributes".into());
        }
        file.sync_all().map_err(|error| error.to_string())?;
        // Recheck after writing the temporary, before replacing disk bytes.
        if read(&root, path)? != expected {
            return Err("file changed on the device; reload before saving".into());
        }
        fs_safety::rename_within(&parent, &temporary, &target).map_err(|error| error.to_string())
    })();
    if let Err(error) = published {
        let _ = fs_safety::unlink_within(&parent, &temporary);
        return Err(error);
    }
    // The new bytes are visible after rename. A directory-sync failure cannot
    // undo publication or leave callers comparing against their old baseline.
    Ok(match sync_parent(&parent) {
        Ok(()) => Reply::Saved,
        Err(error) => Reply::SavedUnconfirmed {
            error: error.to_string(),
        },
    })
}
#[cfg(feature = "host")]
fn navigation(repository: PathBuf, kind: crate::model::ProjectKind) -> Result<Navigation, String> {
    use crate::process::{CancellationToken, CommandSpec, ProcessExecutor, SystemProcessExecutor};
    let repository = std::fs::canonicalize(repository).map_err(|error| error.to_string())?;
    if kind == crate::model::ProjectKind::Folder {
        if !repository.is_dir() {
            return Err("Project directory is unavailable".into());
        }
        return Ok(Navigation {
            worktrees: vec![NavigationWorktree {
                path: repository.clone(),
                ..Default::default()
            }],
            repository,
        });
    }
    let mut command = CommandSpec::new("git").cwd(repository.clone()).args([
        "worktree",
        "list",
        "--porcelain",
        "-z",
    ]);
    command.stdout_limit = MAX_TEXT_BYTES as usize;
    command.timeout = Some(std::time::Duration::from_secs(30));
    let output = SystemProcessExecutor
        .execute(&command, &CancellationToken::new())
        .map_err(|error| error.to_string())?;
    if !output.success {
        return Err(String::from_utf8_lossy(&output.stderr).into_owned());
    }
    let worktrees = parse_navigation(&output.stdout)?;
    Ok(Navigation {
        repository,
        worktrees,
    })
}

#[cfg(any(feature = "host", test))]
fn parse_navigation(bytes: &[u8]) -> Result<Vec<NavigationWorktree>, String> {
    // -z separates both fields and records without quoting directory names.
    let mut result = Vec::new();
    let mut current: Option<NavigationWorktree> = None;
    for field in bytes.split(|byte| *byte == 0) {
        if field.is_empty() {
            if let Some(record) = current.take() {
                result.push(record);
            }
            continue;
        }
        let field = std::str::from_utf8(field).map_err(|_| "Worktree metadata is not UTF-8")?;
        if let Some(path) = field.strip_prefix("worktree ") {
            if current.is_some() {
                return Err("Malformed worktree record".into());
            }
            let path = PathBuf::from(path);
            if !path.is_absolute() {
                return Err("Worktree directory is not absolute".into());
            }
            current = Some(NavigationWorktree {
                path,
                ..Default::default()
            });
        } else {
            let record = current.as_mut().ok_or("Worktree field has no directory")?;
            if let Some(head) = field.strip_prefix("HEAD ") {
                record.head_oid = Some(head.into());
            } else if let Some(branch) = field.strip_prefix("branch ") {
                record.branch = Some(branch.into());
            } else if field == "detached" {
                record.detached = true;
            } else if field == "bare" {
                record.bare = true;
            } else if field == "locked" || field.starts_with("locked ") {
                record.locked = true;
                record.lock_reason = field.strip_prefix("locked ").map(str::to_owned);
            } else if field == "prunable" || field.starts_with("prunable ") {
                record.prunable_reason = Some(field.strip_prefix("prunable ").unwrap_or("").into());
            }
        }
    }
    if let Some(record) = current {
        result.push(record);
    }
    Ok(result)
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
    if bytes.len() as u64 > MAX_TEXT_BYTES {
        return Err("text exceeds remote editor limit (512 KiB)".into());
    }
    if bytes.contains(&0) {
        return Err("binary files cannot be edited".into());
    }
    String::from_utf8(bytes).map_err(|_| "file is not UTF-8 text".into())
}
#[cfg(feature = "host")]
fn git(root: PathBuf, operation: GitOperation) -> Result<String, String> {
    use crate::process::{CancellationToken, CommandSpec, ProcessExecutor, SystemProcessExecutor};
    let root = std::fs::canonicalize(root).map_err(|error| error.to_string())?;
    let lock = mutation_lock(root.clone());
    let _guard = lock.lock().unwrap_or_else(|poison| poison.into_inner());
    let mut command =
        CommandSpec::new("git")
            .cwd(root)
            .args(["--no-pager", "-c", "color.ui=false"]);
    command.stdout_limit = MAX_TEXT_BYTES as usize;
    command.timeout = Some(std::time::Duration::from_secs(30));
    let staging = matches!(&operation, GitOperation::Stage { .. });
    command = match operation {
        GitOperation::Status => command.args(["status", "--short", "--branch"]),
        GitOperation::Worktrees => command.args(["worktree", "list"]),
        GitOperation::CreateWorktree { name, branch } => {
            worktree_name(&name)?;
            if branch.starts_with('-') || branch.len() > 256 {
                return Err("worktree branch must be a valid branch name".into());
            }
            let check = CommandSpec::new("git")
                .cwd(
                    command
                        .current_dir
                        .clone()
                        .ok_or("repository directory unavailable")?,
                )
                .args(["check-ref-format", "--branch"])
                .arg(&branch);
            let check = SystemProcessExecutor
                .execute(&check, &CancellationToken::new())
                .map_err(|error| error.to_string())?;
            if !check.success {
                return Err("worktree branch must be a valid branch name".into());
            }
            let root = command
                .current_dir
                .as_ref()
                .ok_or("repository directory unavailable")?;
            let ignored_path = format!(".worktrees/{name}/cagents-ignore-check");
            let ignore = CommandSpec::new("git")
                .cwd(root)
                .args(["check-ignore", "--quiet", "--"])
                .arg(ignored_path);
            let ignored = SystemProcessExecutor
                .execute(&ignore, &CancellationToken::new())
                .map_err(|error| error.to_string())?;
            if !ignored.success {
                return Err("ignore .worktrees/ in .gitignore or .git/info/exclude before creating a worktree".into());
            }
            let directory = root.join(".worktrees");
            crate::secure_file::reject_symlink_components(&directory)
                .map_err(|error| error.to_string())?;
            match std::fs::create_dir(&directory) {
                Ok(()) => {}
                Err(error)
                    if error.kind() == std::io::ErrorKind::AlreadyExists && directory.is_dir() => {}
                Err(error) => return Err(error.to_string()),
            }
            let destination = directory.join(name);
            command
                .args(["worktree", "add", "-b"])
                .arg(branch)
                .arg("--")
                .arg(destination)
        }
        GitOperation::RemoveWorktree { name } => {
            worktree_name(&name)?;
            let root = command
                .current_dir
                .as_ref()
                .ok_or("repository directory unavailable")?;
            let destination = root.join(".worktrees").join(name);
            crate::secure_file::reject_symlink_components(&destination)
                .map_err(|error| error.to_string())?;
            command.args(["worktree", "remove", "--"]).arg(destination)
        }
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
            if staging {
                command
                    .args(["--literal-pathspecs", "add", "--"])
                    .args(paths)
            } else {
                unstage_command(command, paths)?
            }
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
/// Existing commits use the original restore operation. An unborn branch
/// instead removes selected index entries, leaving even edited files intact.
#[cfg(feature = "host")]
fn unstage_command(
    command: crate::process::CommandSpec,
    paths: Vec<PathBuf>,
) -> Result<crate::process::CommandSpec, String> {
    use crate::process::{CancellationToken, CommandSpec, ProcessExecutor, SystemProcessExecutor};
    use std::{ffi::OsString, os::unix::ffi::OsStringExt};

    let run = |command: CommandSpec| {
        SystemProcessExecutor
            .execute(&command, &CancellationToken::new())
            .map_err(|error| error.to_string())
    };
    let restore = || {
        command
            .clone()
            .args(["--literal-pathspecs", "restore", "--staged", "--"])
            .args(paths.clone())
    };
    let head = run(command
        .clone()
        .args(["rev-parse", "--verify", "--quiet", "HEAD^{commit}"]))?;
    if head.success {
        return Ok(restore());
    }
    if head.exit_code != Some(1) {
        return Err("Cannot determine HEAD; refusing to change the index".into());
    }
    // A failed revision lookup alone is not proof of an unborn branch: an
    // existing ref can contain an invalid object ID or be unreadable.
    let symbolic = run(command.clone().args(["symbolic-ref", "--quiet", "HEAD"]))?;
    let branch = symbolic.stdout.strip_suffix(b"\n");
    let Some(branch) = branch.filter(|branch| {
        symbolic.success && branch.starts_with(b"refs/heads/") && branch.len() > 11
    }) else {
        return Err(
            "HEAD is neither a valid commit nor an unborn branch; the index was left unchanged"
                .into(),
        );
    };
    let reference = run(command
        .clone()
        .args(["show-ref", "--verify", "--quiet", "--"])
        .arg(OsString::from_vec(branch.to_vec())))?;
    if reference.success {
        // Preserve existing-HEAD behavior even when the ref cannot be peeled
        // to a commit; restore itself must accept or refuse that revision.
        return Ok(restore());
    }
    if reference.exit_code != Some(1) || !reference.stderr.is_empty() {
        return Err(
            "The current branch reference is unavailable; the index was left unchanged".into(),
        );
    }
    let indexed = run(command
        .clone()
        .args(["--literal-pathspecs", "ls-files", "--cached", "-z", "--"])
        .args(paths))?;
    if !indexed.success {
        return Err(String::from_utf8_lossy(&indexed.stderr).into_owned());
    }
    if !indexed.stdout.is_empty() && !indexed.stdout.ends_with(b"\0") {
        return Err(
            "Git returned an invalid indexed-path list; the index was left unchanged".into(),
        );
    }
    // NUL-delimited exact indexed names also handle directory selections,
    // arbitrary filename bytes, option-like names and an empty selection.
    Ok(command
        .args([
            "--literal-pathspecs",
            "update-index",
            "--force-remove",
            "-z",
            "--stdin",
        ])
        .stdin(indexed.stdout))
}

#[cfg(all(test, feature = "host"))]
mod tests {
    use super::*;
    fn git_output(root: &std::path::Path, arguments: &[&str]) -> Vec<u8> {
        let output = std::process::Command::new("git")
            .current_dir(root)
            .args(arguments)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        output.stdout
    }

    #[test]
    fn remote_editor_never_saves_a_truncated_prefix_of_an_oversized_file() {
        let directory = tempfile::tempdir().unwrap();
        let path = PathBuf::from("large.txt");
        let mut original = "x".repeat(2 * 1024 * 1024);
        original.push_str("unseen trailing content\n");
        std::fs::write(directory.path().join(&path), &original).unwrap();
        let read = execute(Operation::Read {
            root: directory.path().into(),
            path: path.clone(),
        });
        let expected = match read {
            Ok(Reply::Text(text)) => text,
            Err(error) => {
                assert!(error.contains("512 KiB"));
                // A window opened before the daemon was fixed can retain this old
                // truncated baseline. The save recheck must reject it as well.
                original[..MAX_TEXT_BYTES as usize + 1].to_owned()
            }
            other => panic!("unexpected remote read reply: {other:?}"),
        };
        let content = expected[..MAX_TEXT_BYTES as usize].to_owned();
        let saved = execute(Operation::Write {
            root: directory.path().into(),
            path: path.clone(),
            expected,
            content,
        });
        assert!(saved.unwrap_err().contains("512 KiB"));
        assert_eq!(
            std::fs::read_to_string(directory.path().join(path)).unwrap(),
            original
        );
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
    }

    #[test]
    fn remote_editor_enforces_encoded_byte_limit_at_read_and_save_boundaries() {
        let directory = tempfile::tempdir().unwrap();
        let path = PathBuf::from("boundary.txt");
        let load = || {
            execute(Operation::Read {
                root: directory.path().into(),
                path: path.clone(),
            })
        };
        let exact = "x".repeat(MAX_TEXT_BYTES as usize);
        std::fs::write(directory.path().join(&path), &exact).unwrap();
        assert!(matches!(load(), Ok(Reply::Text(text)) if text == exact));
        let replacement = "y".repeat(MAX_TEXT_BYTES as usize);
        assert!(matches!(
            execute(Operation::Write {
                root: directory.path().into(),
                path: path.clone(),
                expected: exact.clone(),
                content: replacement.clone()
            }),
            Ok(Reply::Saved)
        ));
        assert_eq!(
            std::fs::read_to_string(directory.path().join(&path)).unwrap(),
            replacement
        );
        let too_large = format!("{exact}z");
        std::fs::write(directory.path().join(&path), &too_large).unwrap();
        assert!(load().unwrap_err().contains("512 KiB"));
        let utf8 = "é".repeat(MAX_TEXT_BYTES as usize / 2);
        assert_eq!(utf8.len() as u64, MAX_TEXT_BYTES);
        std::fs::write(directory.path().join(&path), &utf8).unwrap();
        assert!(matches!(load(), Ok(Reply::Text(text)) if text == utf8));
        std::fs::write(directory.path().join(&path), format!("{utf8}é")).unwrap();
        // The sentinel byte cuts a multibyte character; size must be checked before UTF-8 decode.
        assert!(load().unwrap_err().contains("512 KiB"));
        std::fs::write(directory.path().join(&path), b"binary\0text").unwrap();
        assert!(load().unwrap_err().contains("binary"));
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
    }

    #[test]
    fn remote_editor_refuses_save_after_file_grows_beyond_read_limit() {
        let directory = tempfile::tempdir().unwrap();
        let path = PathBuf::from("growing.txt");
        std::fs::write(directory.path().join(&path), "loaded text").unwrap();
        let Ok(Reply::Text(expected)) = execute(Operation::Read {
            root: directory.path().into(),
            path: path.clone(),
        }) else {
            panic!("small file must load")
        };
        let grown = format!("{}unseen trailing content", "x".repeat(2 * 1024 * 1024));
        std::fs::write(directory.path().join(&path), &grown).unwrap();
        let error = execute(Operation::Write {
            root: directory.path().into(),
            path: path.clone(),
            expected,
            content: "edited text".into(),
        })
        .unwrap_err();
        assert!(error.contains("512 KiB"));
        assert_eq!(
            std::fs::read_to_string(directory.path().join(path)).unwrap(),
            grown
        );
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
    }

    #[test]
    fn remote_unstage_before_first_commit_preserves_edits_and_unselected_paths() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        git_output(root, &["init", "-q", "-b", "main"]);
        std::fs::create_dir(root.join("selected dir")).unwrap();
        let selected = [
            "-option",
            "selected dir/literal[1].txt",
            "selected dir/second.txt",
        ];
        for name in selected.into_iter().chain(["keep.txt"]) {
            std::fs::write(root.join(name), "staged version\n").unwrap();
        }
        let invoke = |operation| {
            execute(Operation::Git {
                root: root.into(),
                operation,
            })
        };
        invoke(GitOperation::Stage {
            paths: vec!["-option".into(), "selected dir".into(), "keep.txt".into()],
        })
        .unwrap();
        for name in selected {
            std::fs::write(root.join(name), "edited after staging\n").unwrap();
        }
        invoke(GitOperation::Unstage {
            paths: vec!["-option".into(), "selected dir".into()],
        })
        .unwrap();
        assert_eq!(git_output(root, &["ls-files", "-z"]), b"keep.txt\0");
        for name in selected {
            assert_eq!(
                std::fs::read(root.join(name)).unwrap(),
                b"edited after staging\n"
            );
        }
        assert_eq!(
            std::fs::read(root.join("keep.txt")).unwrap(),
            b"staged version\n"
        );
        // An unmatched selection is a harmless no-op, not an index-wide clear.
        invoke(GitOperation::Unstage {
            paths: vec!["absent.txt".into()],
        })
        .unwrap();
        assert_eq!(git_output(root, &["ls-files", "-z"]), b"keep.txt\0");
    }

    #[test]
    fn remote_unstage_preserves_existing_head_restore_behavior() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        git_output(root, &["init", "-q", "-b", "main"]);
        git_output(root, &["config", "user.name", "Test"]);
        git_output(root, &["config", "user.email", "test@example.invalid"]);
        for name in ["selected.txt", "keep.txt"] {
            std::fs::write(root.join(name), "committed version\n").unwrap();
        }
        git_output(root, &["add", "--", "selected.txt", "keep.txt"]);
        git_output(root, &["commit", "-q", "-m", "initial"]);
        let head = git_output(root, &["rev-parse", "HEAD"]);
        for name in ["selected.txt", "keep.txt"] {
            std::fs::write(root.join(name), "new staged version\n").unwrap();
        }
        git_output(root, &["add", "--", "selected.txt", "keep.txt"]);
        std::fs::write(root.join("selected.txt"), "edited after staging\n").unwrap();
        execute(Operation::Git {
            root: root.into(),
            operation: GitOperation::Unstage {
                paths: vec!["selected.txt".into()],
            },
        })
        .unwrap();
        assert_eq!(git_output(root, &["rev-parse", "HEAD"]), head);
        assert_eq!(
            git_output(root, &["diff", "--cached", "--name-only", "-z"]),
            b"keep.txt\0"
        );
        assert_eq!(
            git_output(root, &["show", ":selected.txt"]),
            b"committed version\n"
        );
        assert_eq!(
            std::fs::read(root.join("selected.txt")).unwrap(),
            b"edited after staging\n"
        );
    }

    #[test]
    fn remote_unstage_refuses_damaged_head_without_changing_the_index() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        git_output(root, &["init", "-q", "-b", "main"]);
        std::fs::write(root.join("selected.txt"), "working bytes\n").unwrap();
        git_output(root, &["add", "--", "selected.txt"]);
        let tree = git_output(root, &["write-tree"]);
        let valid_commit = git_output(
            root,
            &[
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.invalid",
                "commit-tree",
                std::str::from_utf8(&tree).unwrap().trim(),
                "-m",
                "fixture",
            ],
        );
        let index = root.join(".git/index");
        let before = std::fs::read(&index).unwrap();
        let reference = root.join(".git/refs/heads/main");
        let invoke = || {
            execute(Operation::Git {
                root: root.into(),
                operation: GitOperation::Unstage {
                    paths: vec!["selected.txt".into()],
                },
            })
        };
        for content in [
            "broken reference\n",
            "1111111111111111111111111111111111111111\n",
        ] {
            std::fs::write(&reference, content).unwrap();
            assert!(invoke().is_err());
            assert_eq!(std::fs::read(&index).unwrap(), before);
        }
        // An unreadable ref must not become an unborn-branch fallback. Root
        // bypasses ordinary file permissions, so this assertion is non-root.
        if unsafe { libc::geteuid() } != 0 {
            std::fs::write(&reference, valid_commit).unwrap();
            std::fs::set_permissions(&reference, std::fs::Permissions::from_mode(0)).unwrap();
            let outcome = invoke();
            std::fs::set_permissions(&reference, std::fs::Permissions::from_mode(0o600)).unwrap();
            assert!(outcome.is_err());
            assert_eq!(std::fs::read(&index).unwrap(), before);
        }
        std::fs::remove_file(reference).unwrap();
        std::fs::write(root.join(".git/HEAD"), "damaged HEAD\n").unwrap();
        assert!(invoke().is_err());
        assert_eq!(std::fs::read(&index).unwrap(), before);
        assert_eq!(
            std::fs::read(root.join("selected.txt")).unwrap(),
            b"working bytes\n"
        );
    }

    #[test]
    fn git_operations_and_worktrees_target_the_daemon_directory() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().to_owned();
        let run = |arguments: &[&str]| {
            let output = std::process::Command::new("git")
                .current_dir(&root)
                .args(arguments)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        };
        run(&["init", "-q"]);
        run(&["config", "user.name", "Test"]);
        run(&["config", "user.email", "test@example.invalid"]);
        std::fs::write(root.join("-option"), "initial\n").unwrap();
        let invoke = |operation| {
            execute(Operation::Git {
                root: root.clone(),
                operation,
            })
        };
        invoke(GitOperation::Stage {
            paths: vec!["-option".into()],
        })
        .unwrap();
        invoke(GitOperation::Commit {
            message: "initial".into(),
        })
        .unwrap();
        std::fs::write(root.join("-option"), "changed\n").unwrap();
        assert!(
            matches!(invoke(GitOperation::Diff {staged:false}).unwrap(),Reply::Text(text) if text.contains("+changed"))
        );
        assert!(
            invoke(GitOperation::CreateWorktree {
                name: "dev".into(),
                branch: "feature/dev".into()
            })
            .is_err()
        );
        std::fs::write(root.join(".git/info/exclude"), ".worktrees/\n").unwrap();
        invoke(GitOperation::CreateWorktree {
            name: "dev".into(),
            branch: "feature/dev".into(),
        })
        .unwrap();
        assert!(root.join(".worktrees/dev/-option").exists());
        std::fs::write(root.join(".worktrees/dev/untracked"), "keep").unwrap();
        assert!(invoke(GitOperation::RemoveWorktree { name: "dev".into() }).is_err());
        assert!(root.join(".worktrees/dev/untracked").exists());
        std::fs::remove_file(root.join(".worktrees/dev/untracked")).unwrap();
        invoke(GitOperation::RemoveWorktree { name: "dev".into() }).unwrap();
        assert!(!root.join(".worktrees/dev").exists());
        assert!(
            invoke(GitOperation::CreateWorktree {
                name: "../escape".into(),
                branch: "escape".into()
            })
            .is_err()
        );
    }
    #[test]
    fn remote_save_preserves_shared_file_ownership_and_access_mode() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("shared.txt");
        std::fs::write(&path, "before\n").unwrap();
        let file = std::fs::File::open(&path).unwrap();
        let Some(group) = crate::fs_safety::tests::shared_group(&file) else {
            return;
        };
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o660)).unwrap();
        let before = file.metadata().unwrap();
        let reply = execute(Operation::Write {
            root: directory.path().into(),
            path: "shared.txt".into(),
            expected: "before\n".into(),
            content: "after\n".into(),
        })
        .unwrap();
        assert!(matches!(reply, Reply::Saved));
        let after = std::fs::metadata(&path).unwrap();
        assert_eq!((after.uid(), after.gid()), (before.uid(), group));
        assert_eq!(after.mode() & 0o777, before.mode() & 0o777);
        assert_eq!(std::fs::read(&path).unwrap(), b"after\n");
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
    }

    #[test]
    fn concurrent_device_saves_conflict_instead_of_overwriting() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("file"), "old").unwrap();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
        let threads = ["first", "second"].map(|content| {
            let barrier = barrier.clone();
            let root = directory.path().to_owned();
            std::thread::spawn(move || {
                barrier.wait();
                execute(Operation::Write {
                    root,
                    path: "file".into(),
                    expected: "old".into(),
                    content: content.into(),
                })
            })
        });
        barrier.wait();
        assert_eq!(
            threads
                .into_iter()
                .map(|thread| thread.join().unwrap())
                .filter(Result::is_ok)
                .count(),
            1
        );
    }
    #[test]
    fn device_save_preserves_permissions_under_restrictive_umask() {
        const CHILD: &str = "CAGENTS_TEST_REMOTE_SAVE_UMASK";
        if std::env::var_os(CHILD).is_none() {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "device_service::tests::device_save_preserves_permissions_under_restrictive_umask",
                    "--nocapture",
                ])
                .env(CHILD, "1")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "isolated save test failed: {}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        // umask is process-wide: change it only in this isolated test process.
        unsafe { libc::umask(0o077) };
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("file");
        for mode in [0o755, 0o664, 0o4755, 0o2755] {
            std::fs::write(&path, "before").unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
            execute(Operation::Write {
                root: directory.path().to_owned(),
                path: "file".into(),
                expected: "before".into(),
                content: "after".into(),
            })
            .unwrap();
            assert_eq!(std::fs::read_to_string(&path).unwrap(), "after");
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o7777,
                mode
            );
        }
    }

    #[test]
    fn device_save_preserves_access_acl() {
        use std::os::fd::AsRawFd;
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("file");
        std::fs::write(&path, "before").unwrap();
        let original = std::fs::File::open(&path).unwrap();
        // Linux POSIX ACL xattr v2: a named user can read, while the
        // owning group cannot. Mode bits alone cannot express these rules.
        let mut acl = 2_u32.to_le_bytes().to_vec();
        for (tag, permissions, id) in [
            (0x01_u16, 6_u16, u32::MAX),
            (0x02, 4, 12_345),
            (0x04, 0, u32::MAX),
            (0x10, 4, u32::MAX),
            (0x20, 0, u32::MAX),
        ] {
            acl.extend_from_slice(&tag.to_le_bytes());
            acl.extend_from_slice(&permissions.to_le_bytes());
            acl.extend_from_slice(&id.to_le_bytes());
        }
        let attribute = c"system.posix_acl_access";
        let result = unsafe {
            libc::fsetxattr(
                original.as_raw_fd(),
                attribute.as_ptr(),
                acl.as_ptr().cast(),
                acl.len(),
                0,
            )
        };
        if result == -1 {
            let error = std::io::Error::last_os_error();
            if matches!(error.raw_os_error(), Some(libc::EOPNOTSUPP | libc::EINVAL)) {
                return; // The fixture filesystem does not support POSIX ACLs.
            }
            panic!("could not set fixture access ACL: {error}");
        }
        execute(Operation::Write {
            root: directory.path().to_owned(),
            path: "file".into(),
            expected: "before".into(),
            content: "after".into(),
        })
        .unwrap();
        let replacement = std::fs::File::open(&path).unwrap();
        let mut restored = vec![0_u8; acl.len()];
        let length = unsafe {
            libc::fgetxattr(
                replacement.as_raw_fd(),
                attribute.as_ptr(),
                restored.as_mut_ptr().cast(),
                restored.len(),
            )
        };
        assert_eq!(length, acl.len() as libc::ssize_t);
        assert_eq!(restored, acl);
    }

    #[test]
    fn published_save_reports_unconfirmed_durability_and_accepts_the_new_baseline() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("file");
        std::fs::write(&path, "before").unwrap();
        let reply = write_with_sync(
            directory.path(),
            std::path::Path::new("file"),
            "before",
            "after",
            |_| {
                assert_eq!(std::fs::read_to_string(&path).unwrap(), "after");
                Err(std::io::Error::other("injected directory sync failure"))
            },
        )
        .unwrap();
        assert_eq!(
            reply,
            Reply::SavedUnconfirmed {
                error: "injected directory sync failure".into(),
            }
        );
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
        assert_eq!(
            execute(Operation::Write {
                root: directory.path().to_owned(),
                path: "file".into(),
                expected: "after".into(),
                content: "next edit".into(),
            })
            .unwrap(),
            Reply::Saved,
        );
    }

    #[test]
    fn conflicting_save_does_not_publish_or_sync_the_directory() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("file");
        std::fs::write(&path, "current").unwrap();
        assert!(
            write_with_sync(
                directory.path(),
                std::path::Path::new("file"),
                "stale",
                "overwrite",
                |_| panic!("a refused save must not reach the directory sync"),
            )
            .is_err()
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "current");
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
    }

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
    /// Canonical cache keys must be resolved by this daemon, never the UI's filesystem.
    #[serde(default)]
    pub cache_keys: std::collections::BTreeMap<crate::account::AccountId, String>,
    pub catalogs:
        std::collections::BTreeMap<crate::agent::Provider, crate::account::cache::Catalog>,
    pub installed: std::collections::BTreeMap<crate::agent::Provider, bool>,
    /// Existing provider homes, judged on this device rather than the window.
    #[serde(default)]
    pub present: std::collections::BTreeSet<crate::account::AccountId>,
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
    let search_path = runtime.block_on(probe::refresh_search_path());
    let installed = [
        crate::agent::Provider::Claude,
        crate::agent::Provider::Codex,
        crate::agent::Provider::Omp,
    ]
    .into_iter()
    .map(|provider| {
        (
            provider,
            probe::Cli::discover(provider, search_path.as_deref()).is_ok(),
        )
    })
    .collect();
    let accounts = registry.accounts(homes.as_ref());
    let present = accounts.iter()
        .filter(|account| account.resolved_home().is_some_and(|home| home.is_dir()))
        .map(|account| account.id()).collect();
    let cache_keys = accounts
        .into_iter()
        .filter_map(|account| account.cache_key().map(|key| (account.id(), key)))
        .collect();
    let snapshot = AccountSnapshot {
        present,
        cache_keys,
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
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| error.to_string())?;
    let search_path = runtime.block_on(probe::search_path());
    let cli = probe::Cli::discover(account.provider, search_path.as_deref())?;
    Ok(Reply::Login(crate::account::remote::login_recipe(
        &account, &cli,
    )?))
}

#[cfg(feature = "host")]
fn mutation_lock(path: PathBuf) -> std::sync::Arc<std::sync::Mutex<()>> {
    static LOCKS: std::sync::OnceLock<
        std::sync::Mutex<
            std::collections::BTreeMap<PathBuf, std::sync::Weak<std::sync::Mutex<()>>>,
        >,
    > = std::sync::OnceLock::new();
    let mut locks = LOCKS
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    locks.retain(|_, lock| lock.strong_count() > 0);
    if let Some(lock) = locks.get(&path).and_then(|lock| lock.upgrade()) {
        return lock;
    }
    let lock = std::sync::Arc::new(std::sync::Mutex::new(()));
    locks.insert(path, std::sync::Arc::downgrade(&lock));
    lock
}

#[cfg(feature = "host")]
fn worktree_name(name: &str) -> Result<(), String> {
    if name.is_empty()
        || name.len() > 80
        || name == "."
        || name == ".."
        || name.starts_with('-')
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || [b'-', b'_', b'.'].contains(&byte))
    {
        return Err(
            "worktree name must contain 1–80 letters, digits, dots, hyphens or underscores".into(),
        );
    }
    Ok(())
}

#[cfg(test)]
mod wire_tests {
    use super::*;
    use crate::{
        account::{Account, AccountHome},
        agent::Provider,
    };

    fn roundtrip<T>(value: &T)
    where
        T: Serialize + serde::de::DeserializeOwned + PartialEq + std::fmt::Debug,
    {
        let bytes = postcard::to_allocvec(value).expect("serialize binary device frame");
        let decoded: T = postcard::from_bytes(&bytes).expect("deserialize binary device frame");
        assert_eq!(&decoded, value);
        let json = serde_json::to_value(value).unwrap();
        assert_eq!(serde_json::from_value::<T>(json).unwrap(), *value);
    }

    #[test]
    fn published_save_outcomes_roundtrip_on_binary_and_json_wires() {
        roundtrip(&Reply::Saved);
        roundtrip(&Reply::SavedUnconfirmed {
            error: "directory sync failed".into(),
        });
    }

    #[test]
    fn account_device_frames_roundtrip_postcard_and_keep_json_objects() {
        let json = serde_json::json!({
            "registry": {"revision": 3, "accounts": []},
            "identity": {"email": "fixture@example.invalid", "organization": null},
            "usage": [true, false, 0, -3, 0.125, {"status": "limited"}],
            "unicode": "plan · α", "empty": {}
        });
        for reply in [
            Reply::Accounts(json.clone()),
            Reply::AccountProbe(json.clone()),
            Reply::AccountRegistry(json.clone()),
            Reply::AccountLifecycle(json.clone()),
        ] {
            roundtrip(&reply);
            let diagnostic = serde_json::to_value(&reply).unwrap();
            assert!(
                diagnostic
                    .as_object()
                    .unwrap()
                    .values()
                    .all(serde_json::Value::is_object)
            );
        }
        for request in [
            Operation::AccountsRead,
            Operation::AccountsCache {
                reports: json.clone(),
            },
            Operation::AccountsUpdate {
                expected_revision: 3,
                registry: json,
            },
        ] {
            roundtrip(&request);
        }
        let key = uuid::Uuid::new_v4();
        for home in [
            AccountHome::System,
            AccountHome::SystemAt {
                path: "/remote/system".into(),
            },
            AccountHome::Managed {
                key,
                path: "/remote/managed".into(),
            },
            AccountHome::Linked {
                key,
                path: "/remote/linked".into(),
            },
            AccountHome::Brokered {
                service: "fixture".into(),
                key: "fixture".into(),
            },
        ] {
            let account = Account {
                provider: Provider::Codex,
                home,
            };
            roundtrip(&Operation::AccountLogin {
                account: account.clone(),
            });
            roundtrip(&Operation::AccountProbe {
                account,
                force: true,
            });
        }
        use crate::account::remote::Operation as Lifecycle;
        for operation in [
            Lifecycle::Create {
                provider: Provider::Codex,
                copy_codex_config: true,
            },
            Lifecycle::Link {
                provider: Provider::Claude,
                key,
                path: "/remote/linked".into(),
            },
            Lifecycle::Register {
                provider: Provider::Codex,
                key,
            },
            Lifecycle::Abandon {
                provider: Provider::Codex,
                key,
            },
            Lifecycle::Remove {
                id: Account::system(Provider::Codex).id(),
            },
        ] {
            roundtrip(&Operation::AccountLifecycle { operation });
        }
        roundtrip(&Reply::Login(crate::model::LaunchRecipe {
            program: "/remote/bin/codex".into(),
            arguments: vec!["login".into(), "--device-auth".into()],
            working_directory: "/remote".into(),
            environment: [("CODEX_HOME".into(), "/remote/managed".into())].into(),
            unset_environment: ["OPENAI_API_KEY".into()].into(),
        }));
    }
}

#[cfg(test)]
mod navigation_contract_tests {
    use super::*;
    #[test]
    fn nul_worktree_metadata_preserves_paths_and_flags() {
        let rows = parse_navigation(b"worktree /repo with\nnewline\0HEAD 0000000000000000000000000000000000000000\0branch refs/heads/main\0\0worktree /linked\0detached\0locked reason with spaces\0prunable missing directory\0\0").unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].path, PathBuf::from("/repo with\nnewline"));
        assert_eq!(rows[0].branch.as_deref(), Some("refs/heads/main"));
        assert!(rows[1].detached && rows[1].locked);
        assert_eq!(rows[1].lock_reason.as_deref(), Some("reason with spaces"));
        assert!(parse_navigation(b"branch refs/heads/main\0").is_err());
        assert!(parse_navigation(b"worktree relative\0\0").is_err());
    }
    #[test]
    fn navigation_round_trips_on_binary_wire() {
        let operation = Operation::ProjectNavigation {
            repository: "/repo/.git".into(),
            kind: crate::model::ProjectKind::Git,
        };
        let encoded = postcard::to_allocvec(&operation).unwrap();
        assert_eq!(
            postcard::from_bytes::<Operation>(&encoded).unwrap(),
            operation
        );
        let reply = Reply::ProjectNavigation(Navigation {
            repository: "/repo/.git".into(),
            worktrees: vec![NavigationWorktree {
                path: "/repo".into(),
                ..Default::default()
            }],
        });
        let encoded = postcard::to_allocvec(&reply).unwrap();
        assert_eq!(postcard::from_bytes::<Reply>(&encoded).unwrap(), reply);
    }
    #[cfg(feature = "host")]
    #[test]
    fn unborn_git_and_folder_navigation_are_resolved_on_daemon() {
        let root = tempfile::tempdir().unwrap();
        let repo = root.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        assert!(
            std::process::Command::new("git")
                .arg("init")
                .arg("--initial-branch=main")
                .arg(&repo)
                .status()
                .unwrap()
                .success()
        );
        let result = navigation(repo.join(".git"), crate::model::ProjectKind::Git).unwrap();
        assert_eq!(result.worktrees.len(), 1);
        assert_eq!(result.worktrees[0].path, repo.canonicalize().unwrap());
        assert_eq!(
            result.worktrees[0].branch.as_deref(),
            Some("refs/heads/main")
        );
        let result = navigation(root.path().into(), crate::model::ProjectKind::Folder).unwrap();
        assert_eq!(
            result.worktrees[0].path,
            root.path().canonicalize().unwrap()
        );
    }
}
