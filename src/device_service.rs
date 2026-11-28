//! Filesystem and Git operations executed by the selected daemon, rather than
//! interpreting a remote path on the computer displaying the window.
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

pub const MAX_TEXT_BYTES: u64 = 512 * 1024;
/// Leave room for the response envelope without increasing the IPC frame cap.
pub const MAX_DEVICE_IMAGE_BYTES: usize = crate::terminal::ipc::MAX_FRAME_BYTES - 4096;
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
    /// Preference-only intent. The daemon resolves the ID in its current registry.
    AccountDefault {
        expected_revision: u64,
        provider: crate::agent::Provider,
        selection: crate::account::preferences::DefaultSelection,
    },
    /// A concrete image path on this device; relative to the conversation cwd.
    /// Appended to preserve existing Postcard discriminants.
    ReadImage {
        root: PathBuf,
        path: PathBuf,
    },
    /// Resolve immutable image identity in this backend's owned store. The
    /// displaying computer supplies no filesystem path. Appended wire tag.
    ReadImageAsset {
        reference: crate::chat::ImageReference,
    },
    /// Spend one explicitly confirmed reset on a daemon-resolved account.
    AccountReset {
        request: crate::account::resets::ConsumeRequest,
    },
    /// A forced probe after a sign-in changed this home, which first discards
    /// its cached profile metadata. Appended rather than added to
    /// `AccountProbe`, whose fields a carried older CLI still encodes.
    AccountProbeAfterLogin {
        account: crate::account::Account,
    },
    /// A device window's source control: a
    /// [`crate::repository::RepositoryRequest`] as JSON text, answered by the
    /// device's Git with the service a local window uses. Its types keep
    /// Serde forms Postcard cannot read, so it travels as JSON, and as text
    /// so that every build, the CLI's too, has this variant and numbers any
    /// appended after it alike. Appended wire tag.
    Repository(String),
    CloseSessions(SessionCloseScope),
    /// Refresh the owning daemon's provider discovery before reading Accounts.
    AccountsRefresh,
}
/// Removing a project preserves independently owned sessions. Deleting a
/// checkout closes all sessions inside the directory that is going away.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum SessionCloseScope {
    Project {
        repository: PathBuf,
        kind: crate::model::ProjectKind,
        roots: Vec<PathBuf>,
        ownership: crate::project_sessions::ProjectSessionOwnership,
        /// A sweep after the shared close must stop if the project reopens.
        only_if_closed: bool,
    },
    Worktree {
        path: PathBuf,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum GitOperation {
    Status,
    Worktrees,
    CreateWorktree {
        name: String,
        branch: String,
    },
    RemoveWorktree {
        name: String,
    },
    Diff {
        staged: bool,
    },
    Stage {
        paths: Vec<PathBuf>,
    },
    Unstage {
        paths: Vec<PathBuf>,
    },
    Commit {
        message: String,
    },
    /// Answers whether [`GitOperation::RemoveWorktree`] would remove the
    /// worktree `name`, changing nothing: empty text when it would, its
    /// refusal as the text otherwise. A refusal is an answer, not an error,
    /// so the window can tell it from a device it could not ask. A window
    /// asks before closing the worktree's sessions, so a refusal leaves them
    /// running.
    CheckWorktreeRemoval {
        name: String,
    },
    /// Create a new branch and checkout starting at a named device branch.
    CreateWorktreeFromBranch {
        repository: PathBuf,
        name: String,
        branch: String,
        base: String,
    },
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
    Image(Vec<u8>),
    AccountReset(#[serde(with = "wire_json")] serde_json::Value),
    /// A [`crate::repository::RepositoryReply`] as JSON text, encoded where
    /// it was answered so its size is known before it is sent.
    Repository(String),
    SessionsClosed(Option<crate::project_sessions::ProjectSessionClosure>),
    WorktreeCreated {
        path: PathBuf,
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
pub fn execute(operation: Operation) -> Result<Reply, String> {
    use crate::fs_safety;
    use std::fs;
    match operation {
        Operation::CloseSessions(_) => Err("Session closure requires daemon admission".into()),
        Operation::AccountReset { .. } => Err("Account resets require daemon admission".into()),
        Operation::RemoveMergedWorktree { .. } => {
            Err("Automatic cleanup requires the daemon lifecycle fence".into())
        }
        Operation::ProjectNavigation { repository, kind } => {
            navigation(repository, kind).map(Reply::ProjectNavigation)
        }
        Operation::AccountsRead | Operation::AccountsRefresh => accounts(),
        // Under the daemon, account preference changes run typed, through
        // [`account_default`] and [`update_accounts`], so a held account list
        // reaches the window as a retryable refusal.
        Operation::AccountDefault { .. } | Operation::AccountsUpdate { .. } => {
            Err("Account preference changes require daemon admission".into())
        }
        Operation::AccountLifecycle { operation } => {
            crate::account::remote::execute(operation).map(Reply::AccountLifecycle)
        }
        Operation::AccountsCache { reports } => {
            use crate::account::{
                Account,
                cache::{Cache, CatalogObservation, Entry},
            };
            let (entries, catalogs): (
                Vec<(Account, Entry)>,
                Vec<(crate::agent::Provider, CatalogObservation)>,
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
        Operation::AccountLogin { account } => account_login(account),
        Operation::AccountProbe { account, force } => probe_account(account, force, false),
        Operation::AccountProbeAfterLogin { account } => probe_account(account, true, true),
        Operation::Repository(request) => serde_json::from_str(&request)
            .map_err(|error| format!("The device could not read this request: {error}"))
            .and_then(repository)
            .map(Reply::Repository),
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
                // A link to a folder is a folder to anyone choosing one.
                let directory = kind.is_dir() || (kind.is_symlink() && entry.path().is_dir());
                entries.push(Entry {
                    name,
                    directory,
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
        Operation::ReadImage { root, path } => {
            read_image_file(&root, &path, MAX_DEVICE_IMAGE_BYTES).map(Reply::Image)
        }
        Operation::ReadImageAsset { reference } => {
            crate::chat::attachment_assets::read(&reference, MAX_DEVICE_IMAGE_BYTES)
                .map(Reply::Image)
        }
        Operation::Write {
            root,
            path,
            expected,
            content,
        } => write_with_sync(&root, &path, &expected, &content, fs_safety::sync_directory),
        Operation::Git { root, operation } => git(root, operation),
    }
}

/// Read the producer's exact concrete filename on its owning device. Resolve
/// relative names only against the verified conversation cwd; never search
/// directories, expand patterns, or reinterpret a path on the UI computer.
pub fn read_image_file(
    root: &std::path::Path,
    path: &std::path::Path,
    limit: usize,
) -> Result<Vec<u8>, String> {
    let limit = limit.min(crate::chat::MAX_INLINE_IMAGE_BYTES);
    use std::os::unix::fs::OpenOptionsExt;
    if !path.is_absolute() && !root.is_absolute() {
        return Err("the conversation working directory is unavailable".into());
    }
    // Preserve kernel symlink/.. semantics. Explicit provider image paths can
    // legitimately name /tmp artifacts or a sibling worktree.
    let path = root.join(path);
    let io_error = |error: std::io::Error| format!("{}: {error}", path.display());
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(&path)
        .map_err(io_error)?;
    let metadata = file.metadata().map_err(io_error)?;
    if !metadata.is_file() {
        return Err(format!(
            "{}: the image must be a regular file",
            path.display()
        ));
    }
    let too_large = || {
        format!(
            "{}: the image exceeds the {} KiB file preview limit",
            path.display(),
            limit / 1024
        )
    };
    if metadata.len() > limit as u64 {
        return Err(too_large());
    }
    let bytes = crate::fs_safety::read_bounded(&mut file, limit as u64)
        .map_err(|error| error.to_string())?;
    if bytes.len() > limit {
        return Err(too_large());
    }
    Ok(bytes)
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
/// The largest diff side a device sends. Two of them, escaped as JSON, stay
/// well inside one IPC frame; a larger side is reported as too large to show,
/// as one past the service's own limit is.
#[cfg(feature = "host")]
const MAX_REMOTE_DIFF_SIDE_BYTES: u64 = 2 * 1024 * 1024;

#[cfg(feature = "host")]
fn select_repository_worktree(
    request: &mut crate::repository::RepositoryRequest,
) -> Result<PathBuf, String> {
    let requested = request.worktree();
    let worktree = std::fs::canonicalize(requested).map_err(|error| {
        if matches!(
            error.kind(),
            std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
        ) {
            format!(
                "The device checkout could not be found at {}. Open an existing checkout to see its changes.",
                requested.display()
            )
        } else {
            format!("Couldn't read the device checkout at {}: {error}", requested.display())
        }
    })?;
    // Confirmed discard must stay in the checkout selected for the mutation
    // lock, even if the user-facing link changes while the request waits.
    // Preserve discard's validation of the original absolute path.
    if let crate::repository::RepositoryRequest::Discard(discard) = request
        && discard.worktree.is_absolute()
    {
        discard.worktree.clone_from(&worktree);
    }
    Ok(worktree)
}

/// Answers a device window's source-control request, as JSON text.
#[cfg(feature = "host")]
fn repository(mut request: crate::repository::RepositoryRequest) -> Result<String, String> {
    use crate::repository::{CancellationToken, RepositoryService};
    let worktree = select_repository_worktree(&mut request)?;
    // A change waits for any other change to the same worktree, from this
    // window or another, as Device files' Git operations do.
    let too_large = match &request {
        crate::repository::RepositoryRequest::Snapshot { .. } => {
            "There are too many changes to send from the device. Collapse Untracked, or run git status in a terminal on the device."
        }
        crate::repository::RepositoryRequest::Diff(_) => {
            "This comparison is too large to send from the device. Open the file on the device instead."
        }
        _ => {
            "The device's answer is too large to send. Run git in a terminal on the device instead."
        }
    };
    let lock = request.mutates().then(|| mutation_lock(worktree));
    let _guard = lock
        .as_ref()
        .map(|lock| lock.lock().unwrap_or_else(|poison| poison.into_inner()));
    let cancellation = CancellationToken::new();
    let reply = if matches!(&request, crate::repository::RepositoryRequest::Push(_)) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(45);
        let service =
            RepositoryService::new("git", std::sync::Arc::new(DevicePushExecutor { deadline }));
        request.run(&service, &cancellation)
    } else {
        request.run(&RepositoryService::system("git"), &cancellation)
    }
    .map_err(|error| error.to_string())?
    .bounded(MAX_REMOTE_DIFF_SIDE_BYTES);
    let encoded = serde_json::to_string(&reply).map_err(|error| error.to_string())?;
    if encoded.len() > MAX_DEVICE_IMAGE_BYTES {
        return Err(too_large.into());
    }
    Ok(encoded)
}

/// Bound the device's mutation lock across the push and every reconciliation
/// command, even those that deliberately use a fresh cancellation token.
#[cfg(feature = "host")]
struct DevicePushExecutor {
    deadline: std::time::Instant,
}

#[cfg(feature = "host")]
impl crate::process::ProcessExecutor for DevicePushExecutor {
    fn execute(
        &self,
        command: &crate::process::CommandSpec,
        cancellation: &crate::process::CancellationToken,
    ) -> Result<crate::process::ProcessOutput, crate::process::ProcessFailure> {
        use crate::process::{ProcessFailureKind, SystemProcessExecutor, process_failure};
        let remaining = self
            .deadline
            .saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            return Err(process_failure(
                command,
                ProcessFailureKind::TimedOut,
                "the device push exceeded its time limit",
                false,
            ));
        }
        // Cap each command at 30 seconds to leave room for a follow-up
        // check within the overall budget if the push times out.
        let limit = remaining.min(std::time::Duration::from_secs(30));
        let command = command
            .clone()
            .timeout(command.timeout.map_or(limit, |timeout| timeout.min(limit)));
        SystemProcessExecutor.execute(&command, cancellation)
    }
}

#[cfg(feature = "host")]
fn git(root: PathBuf, operation: GitOperation) -> Result<Reply, String> {
    use crate::process::{CancellationToken, CommandSpec, ProcessExecutor, SystemProcessExecutor};
    let root = std::fs::canonicalize(root).map_err(|error| error.to_string())?;
    let lock = mutation_lock(root.clone());
    let _guard = lock.lock().unwrap_or_else(|poison| poison.into_inner());
    let mut command =
        CommandSpec::new("git")
            .cwd(root.clone())
            .args(["--no-pager", "-c", "color.ui=false"]);
    command.stdout_limit = MAX_TEXT_BYTES as usize;
    command.timeout = Some(std::time::Duration::from_secs(30));
    let staging = matches!(&operation, GitOperation::Stage { .. });
    let mut created = None;
    command = match operation {
        GitOperation::Status => command.args(["status", "--short", "--branch"]),
        GitOperation::Worktrees => command.args(["worktree", "list"]),
        GitOperation::CreateWorktree { name, branch } => {
            create_worktree_command(command, &name, &branch, None)?
        }
        GitOperation::CreateWorktreeFromBranch {
            repository,
            name,
            branch,
            base,
        } => {
            let service = crate::repository::RepositoryService::system("git");
            let actual = service
                .common_dir(&root, &CancellationToken::new())
                .map_err(|error| error.to_string())?;
            if actual != repository {
                return Err("The checkout no longer belongs to the selected repository.".into());
            }
            created = Some(
                root.join(crate::settings::DEFAULT_MANAGED_WORKTREE_DIRECTORY)
                    .join(&name),
            );
            create_worktree_command(command, &name, &branch, Some(&base))?
        }
        GitOperation::RemoveWorktree { name } => {
            let root = command
                .current_dir
                .as_ref()
                .ok_or("repository directory unavailable")?;
            let destination = removable_worktree(root, &name)?;
            command.args(["worktree", "remove", "--"]).arg(destination)
        }
        GitOperation::CheckWorktreeRemoval { name } => {
            let root = command
                .current_dir
                .as_ref()
                .ok_or("repository directory unavailable")?;
            return Ok(Reply::Text(
                removable_worktree(root, &name).err().unwrap_or_default(),
            ));
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
    if let Some(path) = created {
        return Ok(Reply::WorktreeCreated { path });
    }
    String::from_utf8(output.stdout)
        .map(Reply::Text)
        .map_err(|_| "Git output is not UTF-8".into())
}
#[cfg(feature = "host")]
fn create_worktree_command(
    command: crate::process::CommandSpec,
    name: &str,
    branch: &str,
    base: Option<&str>,
) -> Result<crate::process::CommandSpec, String> {
    use crate::process::{CancellationToken, CommandSpec, ProcessExecutor, SystemProcessExecutor};
    worktree_name(name)?;
    let root = command
        .current_dir
        .as_ref()
        .ok_or("repository directory unavailable")?;
    let verify = |args: &[&str]| {
        let mut check = CommandSpec::new("git")
            .cwd(root)
            .args(["check-ref-format"])
            .args(args.iter().copied());
        check.timeout = command.timeout;
        SystemProcessExecutor
            .execute(&check, &CancellationToken::new())
            .map(|output| output.success)
            .map_err(|error| error.to_string())
    };
    // The short name is passed to -b. Reject checkout shorthand before Git
    // can expand it while checking a new branch name.
    if branch.starts_with('-')
        || branch.contains("@{")
        || branch.len() > 256
        || !verify(&["--branch", branch])?
    {
        return Err("Enter a valid name for the new branch.".into());
    }
    let start = if let Some(base) = base {
        let reference = format!("refs/heads/{base}");
        if base.len() > 256 || !verify(&[&reference])? {
            return Err("The starting branch name is invalid.".into());
        }
        let mut find = CommandSpec::new("git")
            .cwd(root)
            .args(["show-ref", "--hash", "--verify", "--"])
            .arg(reference);
        find.timeout = command.timeout;
        let output = SystemProcessExecutor
            .execute(&find, &CancellationToken::new())
            .map_err(|error| error.to_string())?;
        if !output.success {
            return Err(format!(
                "The starting branch {base} no longer exists on this device."
            ));
        }
        Some(String::from_utf8_lossy(&output.stdout).trim().to_owned())
    } else {
        None
    };
    let directory = root.join(crate::settings::DEFAULT_MANAGED_WORKTREE_DIRECTORY);
    let service = crate::repository::RepositoryService::system("git");
    service
        .ensure_managed_directory_ignored(root, &directory, &CancellationToken::new())
        .map_err(|error| error.to_string())?;
    service
        .ignore_legacy_managed_worktrees(root, &CancellationToken::new())
        .map_err(|error| error.to_string())?;
    crate::secure_file::reject_symlink_components(&directory).map_err(|error| error.to_string())?;
    match std::fs::create_dir(&directory) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists && directory.is_dir() => {}
        Err(error) => return Err(error.to_string()),
    }
    let mut command = command.args(["worktree", "add"]);
    if start.is_some() {
        command = command.arg("--no-track");
    }
    command = command
        .arg("-b")
        .arg(branch)
        .arg("--")
        .arg(directory.join(name));
    if let Some(start) = start {
        command = command.arg(start);
    }
    Ok(command)
}

/// Where worktree `name` of the repository at `root` is, when the device
/// would remove it: checked without changing anything, for both the removal
/// and the check a window runs before closing the worktree's sessions.
///
/// The removal runs without `--force`, so this refuses what Git would, and
/// more: Git's own check follows `status.showUntrackedFiles`, which can hide
/// untracked files it would then delete, while this counts every one.
#[cfg(feature = "host")]
fn removable_worktree(root: &std::path::Path, name: &str) -> Result<PathBuf, String> {
    use crate::process::CancellationToken;
    worktree_name(name)?;
    let destination = root.join(".worktrees").join(name);
    crate::secure_file::reject_symlink_components(&destination)
        .map_err(|error| error.to_string())?;
    let service = crate::repository::RepositoryService::system("git");
    let cancellation = CancellationToken::new();
    let records = service
        .list_worktrees(root, &cancellation)
        .map_err(|error| error.to_string())?;
    let record = records
        .iter()
        .find(|record| record.path == destination)
        .ok_or("worktree is not registered with Git")?;
    if record.lock.locked {
        return Err("the worktree is locked; unlock it in Git on the device first".into());
    }
    // Git refuses a dirty worktree itself, but not one whose detached HEAD is
    // the only thing holding its commits.
    service.refuse_losing_detached_commits(root, record, &cancellation)?;
    // Git refuses a worktree with a checked-out submodule, one whose
    // directory holds a `.git`; a submodule never checked out there is
    // removed like any directory. Only a gitlink can hold one: every other
    // index entry is a file.
    let index = service
        .index_paths(&destination, &cancellation)
        .map_err(|error| error.to_string())?;
    if index.gitlinks
        && index.paths.iter().any(|path| {
            // A tracked symlink is a file entry, even when it points at
            // a directory holding a `.git`.
            let entry = destination.join(path.as_os_str());
            std::fs::symlink_metadata(&entry).is_ok_and(|metadata| metadata.is_dir())
                && entry.join(".git").exists()
        })
    {
        return Err(
            "it has checked-out submodules, which Git removes only with --force. Use \
             git worktree remove --force in a terminal on the device."
                .into(),
        );
    }
    if service
        .status_for_removal(&destination, &cancellation)
        .map_err(|error| error.to_string())?
        .dirty
        .is_dirty()
    {
        return Err(
            "it has uncommitted or untracked changes. Commit or remove them on the \
             device first, or use git worktree remove --force in a terminal there."
                .into(),
        );
    }
    Ok(destination)
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
    use std::path::Path;

    #[test]
    fn an_expired_device_push_does_not_start_another_git_command() {
        use crate::process::{CancellationToken, CommandSpec, ProcessExecutor, ProcessFailureKind};
        let executor = DevicePushExecutor {
            deadline: std::time::Instant::now(),
        };
        let failure = executor
            .execute(&CommandSpec::new("git"), &CancellationToken::new())
            .unwrap_err();
        assert_eq!(failure.kind, ProcessFailureKind::TimedOut);
        assert!(!failure.started);
    }

    /// Postcard numbers an enum's variants by position, so the CLI, a window
    /// and a daemon of one protocol version agree on a variant only while
    /// every build has every variant in the same place.
    #[test]
    fn a_repository_request_keeps_its_appended_wire_tag() {
        assert_eq!(
            postcard::to_allocvec(&Operation::Repository(String::new())).unwrap(),
            [20, 0]
        );
    }

    /// A project chooser shows folders, and a link to one is a folder too.
    #[test]
    fn listing_counts_a_link_to_a_folder_as_a_folder() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("code")).unwrap();
        std::fs::write(root.path().join("notes"), "").unwrap();
        std::os::unix::fs::symlink(root.path().join("code"), root.path().join("linked")).unwrap();
        std::os::unix::fs::symlink(root.path().join("notes"), root.path().join("note-link"))
            .unwrap();
        std::os::unix::fs::symlink(root.path().join("gone"), root.path().join("dangling")).unwrap();
        let Reply::Directory { entries, .. } = execute(Operation::List {
            path: root.path().to_owned(),
        })
        .unwrap() else {
            panic!("a listing replies with a directory");
        };
        let kind = |name: &str| {
            let entry = entries.iter().find(|entry| entry.name == name).unwrap();
            (entry.directory, entry.symlink)
        };
        assert_eq!(kind("code"), (true, false));
        assert_eq!(kind("linked"), (true, true));
        assert_eq!(kind("notes"), (false, false));
        assert_eq!(kind("note-link"), (false, true));
        assert_eq!(kind("dangling"), (false, true));
    }

    /// Another window saving the account list refuses a preference change,
    /// such as a rename, at once, in its own words and
    /// retryably; the same change is saved once the list is free.
    #[test]
    fn a_held_account_list_refuses_a_preference_change_retryably_and_changes_nothing() {
        use crate::account::{
            Account,
            registry::Store,
            remote::{LifecycleError, ListHeld},
        };
        let system = Account::system(crate::agent::Provider::Claude);
        let directory = tempfile::tempdir().unwrap();
        let store = Store::at(directory.path());
        let (current, ()) = store
            .update(|registry| registry.set_label(&system, Some("Home".into())))
            .unwrap();
        let file = directory.path().join("accounts.toml");
        let before = std::fs::read(&file).unwrap();
        let mut proposed = current.clone();
        proposed.set_label(&system, Some("Work".into()));
        let held = store.lock().unwrap();
        assert_eq!(
            update_accounts_in(&store, current.revision(), proposed.clone()).unwrap_err(),
            LifecycleError::ListHeld(ListHeld::Registry)
        );
        assert_eq!(std::fs::read(&file).unwrap(), before);
        drop(held);
        let Reply::AccountRegistry(saved) =
            update_accounts_in(&store, current.revision(), proposed).unwrap()
        else {
            panic!("a preference change replies with the registry");
        };
        let saved: crate::account::registry::Registry = serde_json::from_value(saved).unwrap();
        assert_eq!(saved.label(&system), Some("Work"));
    }
    #[test]
    fn image_asset_operation_reads_backend_receipts_and_refuses_foreign_or_missing_identity() {
        crate::chat::attachment_assets::with_test_image(|backend| {
            let reference = backend.reference.as_ref().unwrap();
            let expected = std::fs::read(&backend.path).unwrap();
            assert_eq!(
                execute(Operation::ReadImageAsset {
                    reference: reference.clone()
                })
                .unwrap(),
                Reply::Image(expected)
            );
            let mut missing = reference.clone();
            missing.id = uuid::Uuid::new_v4();
            assert!(
                execute(Operation::ReadImageAsset { reference: missing })
                    .unwrap_err()
                    .contains("unavailable")
            );
            crate::chat::attachment_assets::with_test_image(|other| {
                assert_ne!(reference.device, other.reference.as_ref().unwrap().device);
                assert!(
                    execute(Operation::ReadImageAsset {
                        reference: reference.clone()
                    })
                    .unwrap_err()
                    .contains("another device")
                );
            });
            let operation = Operation::ReadImageAsset {
                reference: reference.clone(),
            };
            let encoded = postcard::to_allocvec(&operation).unwrap();
            assert_eq!(
                encoded[0], 17,
                "new operation appends after ReadImage's existing tag16"
            );
            assert_eq!(
                postcard::from_bytes::<Operation>(&encoded).unwrap(),
                operation
            );
            assert_eq!(
                postcard::to_allocvec(&Operation::ReadImage {
                    root: "/backend".into(),
                    path: "image.png".into()
                })
                .unwrap()[0],
                16
            );
            let envelope = crate::terminal::ipc::ServerEnvelope {
                version: crate::terminal::ipc::PROTOCOL_VERSION,
                request_id: Some(u64::MAX),
                proof: crate::model::HostSessionProof {
                    host_session_id: crate::model::HostSessionId::new(
                        uuid::Uuid::new_v4().to_string(),
                    ),
                    boot_id: crate::model::BootId::new(uuid::Uuid::new_v4().to_string()),
                    host_incarnation: crate::model::HostIncarnation::new(
                        uuid::Uuid::new_v4().to_string(),
                    ),
                },
                message: crate::terminal::ipc::ServerMessage::Response {
                    response: crate::terminal::ipc::ServerResponse::DeviceOperation {
                        reply: Reply::Image(vec![0; MAX_DEVICE_IMAGE_BYTES]),
                    },
                },
            };
            let mut frame = Vec::new();
            crate::terminal::ipc::write_frame(&mut frame, &envelope).unwrap();
            assert!(frame.len() <= crate::terminal::ipc::MAX_FRAME_BYTES);
            assert_eq!(
                crate::terminal::ipc::read_frame::<crate::terminal::ipc::ServerEnvelope>(
                    &mut std::io::Cursor::new(frame)
                )
                .unwrap()
                .unwrap(),
                envelope
            );
        });
    }

    #[test]
    fn image_read_returns_literal_binary_files_on_the_selected_device() {
        let device = tempfile::tempdir().unwrap();
        let path = PathBuf::from(".worktrees/review/docs/tree-fixture-dark-expanded.png");
        std::fs::create_dir_all(device.path().join(path.parent().unwrap())).unwrap();
        let expected = b"\x89PNG\0device bytes";
        std::fs::write(device.path().join(&path), expected).unwrap();
        for path in [path.clone(), device.path().join(&path)] {
            assert_eq!(
                execute(Operation::ReadImage {
                    root: device.path().into(),
                    path
                })
                .unwrap(),
                Reply::Image(expected.to_vec())
            );
        }
        for path in [
            PathBuf::from(".worktrees/review/docs/tree-fixture-dark-{collapsed,expanded}.png"),
            PathBuf::from(".worktrees/review/docs/tree-real-*.png"),
            PathBuf::from(".worktrees/review/docs/missing.png"),
        ] {
            assert!(
                execute(Operation::ReadImage {
                    root: device.path().into(),
                    path
                })
                .is_err()
            );
        }
    }

    #[test]
    fn image_read_preserves_explicit_paths_and_rejects_special_files_and_oversize() {
        use std::os::unix::fs::symlink;
        let device = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret.png"), b"outside").unwrap();
        symlink(outside.path(), device.path().join("linked")).unwrap();
        assert_eq!(
            read_image_file(device.path(), Path::new("linked/secret.png"), 32).unwrap(),
            b"outside"
        );
        assert_eq!(
            read_image_file(device.path(), &outside.path().join("secret.png"), 32).unwrap(),
            b"outside"
        );
        assert!(read_image_file(device.path(), Path::new("."), 32).is_err());
        let fifo = device.path().join("pipe.png");
        let name = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        let (sender, receiver) = std::sync::mpsc::channel();
        let root = device.path().to_path_buf();
        let task = std::thread::spawn(move || {
            sender
                .send(read_image_file(&root, Path::new("pipe.png"), 32))
                .unwrap();
        });
        assert!(
            receiver
                .recv_timeout(std::time::Duration::from_secs(2))
                .unwrap()
                .is_err()
        );
        task.join().unwrap();
        std::fs::write(device.path().join("image.png"), vec![0; 32]).unwrap();
        assert_eq!(
            read_image_file(device.path(), Path::new("image.png"), 32)
                .unwrap()
                .len(),
            32
        );
        std::fs::write(device.path().join("image.png"), vec![0; 33]).unwrap();
        assert!(
            read_image_file(device.path(), Path::new("image.png"), 32)
                .unwrap_err()
                .contains("limit")
        );
        std::fs::File::create(device.path().join("large.png"))
            .unwrap()
            .set_len(MAX_DEVICE_IMAGE_BYTES as u64 + 1)
            .unwrap();
        assert!(
            execute(Operation::ReadImage {
                root: device.path().into(),
                path: "large.png".into()
            })
            .unwrap_err()
            .contains("limit")
        );
    }

    #[test]
    fn image_read_preserves_kernel_parent_components_in_symlink_paths() {
        use std::os::unix::fs::symlink;
        let directory = tempfile::tempdir().unwrap();
        let cwd = directory.path().join("selected");
        let actual = directory.path().join("actual");
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::create_dir_all(actual.join("nested")).unwrap();
        std::fs::write(cwd.join("image.png"), b"wrong image").unwrap();
        std::fs::write(actual.join("image.png"), b"correct image").unwrap();
        symlink(actual.join("nested"), cwd.join("link")).unwrap();
        assert_eq!(
            read_image_file(&cwd, Path::new("link/../image.png"), 32).unwrap(),
            b"correct image"
        );
        assert_eq!(
            read_image_file(&cwd, Path::new("../actual/image.png"), 32).unwrap(),
            b"correct image"
        );
    }

    #[test]
    fn appended_image_wire_variants_preserve_existing_tags_and_fit_frame_budget() {
        assert_eq!(postcard::to_allocvec(&Operation::Home).unwrap(), vec![0]);
        let previous_tail = Operation::AccountDefault {
            expected_revision: 0,
            provider: crate::agent::Provider::Claude,
            selection: crate::account::preferences::DefaultSelection::Automatic,
        };
        assert_eq!(postcard::to_allocvec(&previous_tail).unwrap()[0], 15);
        assert_eq!(postcard::to_allocvec(&Reply::Saved).unwrap(), vec![10]);
        assert_eq!(
            postcard::to_allocvec(&Reply::SavedUnconfirmed {
                error: String::new()
            })
            .unwrap(),
            vec![13, 0]
        );
        let operation = Operation::ReadImage {
            root: "/device/project".into(),
            path: "image.png".into(),
        };
        let encoded = postcard::to_allocvec(&operation).unwrap();
        assert_eq!(encoded[0], 16);
        assert_eq!(
            postcard::from_bytes::<Operation>(&encoded).unwrap(),
            operation
        );
        let reply = Reply::Image(vec![0; MAX_DEVICE_IMAGE_BYTES]);
        let encoded = postcard::to_allocvec(&reply).unwrap();
        assert_eq!(encoded[0], 14);
        assert!(encoded.len() < crate::terminal::ipc::MAX_FRAME_BYTES - 4000);
        assert_eq!(postcard::from_bytes::<Reply>(&encoded).unwrap(), reply);
    }

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
            std::fs::set_permissions(&reference, std::fs::Permissions::from_mode(0o000)).unwrap();
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
    fn a_missing_device_checkout_names_its_path() {
        use crate::repository::{RepositoryRequest, SnapshotOptions};

        let directory = tempfile::tempdir().unwrap();
        let checkout = directory.path().join("removed-worktree");
        let error = repository(RepositoryRequest::Snapshot {
            worktree: checkout.clone(),
            options: SnapshotOptions::default(),
            previous: None,
            files: false,
        })
        .unwrap_err();
        assert!(error.contains("The device checkout could not be found"));
        assert!(error.contains(checkout.to_string_lossy().as_ref()));
    }

    #[test]
    fn device_discard_keeps_the_checkout_selected_for_the_mutation_lock() {
        use crate::repository::{DiscardRequest, MutationOutcome, RepoPath, RepositoryRequest};
        use std::{fs, os::unix::fs::symlink};

        let directory = tempfile::tempdir().unwrap();
        let original = directory.path().join("original");
        let replacement = directory.path().join("replacement");
        for checkout in [&original, &replacement] {
            fs::create_dir(checkout).unwrap();
            git_output(checkout, &["init", "-q"]);
            fs::write(checkout.join("draft.txt"), b"draft\n").unwrap();
        }
        let alias = directory.path().join("checkout");
        symlink("original", &alias).unwrap();
        let path = RepoPath::parse("draft.txt").unwrap();
        let mut request = RepositoryRequest::Discard(DiscardRequest {
            worktree: alias.clone(),
            tracked: Vec::new(),
            untracked: vec![path.clone()],
            confirmed: true,
        });
        let selected = select_repository_worktree(&mut request).unwrap();
        assert_eq!(selected, original.canonicalize().unwrap());
        fs::remove_file(&alias).unwrap();
        symlink("replacement", &alias).unwrap();

        let outcome = request
            .run(
                &crate::repository::RepositoryService::system("git"),
                &crate::repository::CancellationToken::new(),
            )
            .unwrap()
            .into_mutation()
            .unwrap();
        assert_eq!(outcome, MutationOutcome::Applied { paths: vec![path] });
        assert!(!original.join("draft.txt").exists());
        assert_eq!(fs::read(replacement.join("draft.txt")).unwrap(), b"draft\n");
    }

    #[test]
    fn source_control_requests_are_answered_by_the_devices_git() {
        use crate::repository::{
            ChangeArea, DiffBasis, DiffContent, DiffRequest, DiffSide, MutationOutcome,
            PathSelection, RepoPath, RepositoryReply, RepositoryRequest, SnapshotOptions,
            SnapshotRead, StageRequest,
        };
        let directory = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(directory.path()).unwrap();
        for arguments in [
            &["init", "-q"][..],
            &["config", "user.name", "Test"],
            &["config", "user.email", "test@example.invalid"],
        ] {
            git_output(&root, arguments);
        }
        std::fs::write(root.join("notes.txt"), "one\n").unwrap();
        git_output(&root, &["add", "notes.txt"]);
        git_output(&root, &["commit", "-qm", "first"]);
        std::fs::write(root.join("notes.txt"), "one\ntwo\n").unwrap();
        // Each request crosses the wire as a window sends it.
        let answer = |request: RepositoryRequest| -> RepositoryReply {
            let operation = Operation::Repository(serde_json::to_string(&request).unwrap());
            let operation: Operation =
                postcard::from_bytes(&postcard::to_allocvec(&operation).unwrap()).unwrap();
            let reply = execute(operation).unwrap();
            let reply: Reply =
                postcard::from_bytes(&postcard::to_allocvec(&reply).unwrap()).unwrap();
            let Reply::Repository(encoded) = reply else {
                panic!("a repository request has a repository reply");
            };
            serde_json::from_str(&encoded).unwrap()
        };
        let snapshot = |answer: &dyn Fn(RepositoryRequest) -> RepositoryReply| {
            let read = answer(RepositoryRequest::Snapshot {
                worktree: root.clone(),
                options: SnapshotOptions::default(),
                previous: None,
                files: false,
            })
            .into_snapshot()
            .unwrap();
            let SnapshotRead::Changed { snapshot, .. } = read else {
                panic!("a read with no previous digest is a change");
            };
            snapshot
        };
        let before = snapshot(&answer);
        assert!(
            before.files.paths.is_empty(),
            "the listing stays on the device"
        );
        assert_eq!(before.files.tracked_count, 1);
        let [entry] = before.status.entries.as_slice() else {
            panic!("one changed file: {:?}", before.status.entries);
        };
        assert_eq!(entry.area, ChangeArea::Unstaged);
        let path = RepoPath::parse(std::path::Path::new("notes.txt")).unwrap();
        let diff = answer(RepositoryRequest::Diff(DiffRequest {
            worktree: root.clone(),
            basis: DiffBasis::IndexToWorktree,
            path: path.clone(),
            old_path: None,
        }))
        .into_diff()
        .unwrap();
        assert!(matches!(
            diff,
            DiffContent::Comparison { new: DiffSide::Text { ref text, .. }, .. } if text == "one\ntwo\n"
        ));
        let staged = answer(RepositoryRequest::Stage(StageRequest {
            worktree: root.clone(),
            selection: vec![PathSelection::new(path)],
        }))
        .into_mutation()
        .unwrap();
        assert!(
            matches!(staged, MutationOutcome::Applied { .. }),
            "{staged:?}"
        );
        let after = snapshot(&answer);
        assert_eq!(after.status.entries[0].area, ChangeArea::Staged);
    }

    #[test]
    fn worktree_creation_uses_the_exact_device_branch_tip() {
        if crate::repository::isolated_git_test(concat!(
            module_path!(),
            "::worktree_creation_uses_the_exact_device_branch_tip"
        )) {
            return;
        }
        let directory = crate::repository::retained_managed_fixture_directory();
        let root = directory.path().to_owned();
        let git = |args: &[&str]| {
            let output = std::process::Command::new("git")
                .args([
                    "-c",
                    "user.name=Test",
                    "-c",
                    "user.email=test@example.invalid",
                ])
                .current_dir(&root)
                .args(args)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            String::from_utf8(output.stdout).unwrap()
        };
        git(&["init", "-q", "-b", "main"]);
        git(&["commit", "--allow-empty", "-m", "main"]);
        git(&["switch", "-c", "feature/start"]);
        git(&["commit", "--allow-empty", "-m", "branch-only commit"]);
        git(&["switch", "main"]);
        // A tag of the same name must not influence the start point.
        git(&["tag", "feature/start", "main"]);
        let create = |repository: PathBuf, name: &str, base: &str| {
            execute(Operation::Git {
                root: root.clone(),
                operation: GitOperation::CreateWorktreeFromBranch {
                    repository,
                    name: name.into(),
                    branch: name.into(),
                    base: base.into(),
                },
            })
        };
        let repository = root.canonicalize().unwrap().join(".git");
        git(&["tag", "missing", "main"]);
        assert!(
            create(repository.clone(), "refused", "missing")
                .unwrap_err()
                .contains("no longer exists")
        );
        assert!(
            create(root.join("wrong.git"), "refused", "feature/start")
                .unwrap_err()
                .contains("selected repository")
        );
        assert!(!root.join(".worktrees").exists());

        assert_eq!(
            create(repository, "derived", "feature/start").unwrap(),
            Reply::WorktreeCreated {
                path: root.canonicalize().unwrap().join(".worktrees/derived")
            },
        );
        assert_eq!(
            git(&["-C", ".worktrees/derived", "rev-parse", "HEAD"]),
            git(&["rev-parse", "refs/heads/feature/start"]),
        );
        assert_eq!(
            git(&["-C", ".worktrees/derived", "branch", "--show-current"]),
            "derived\n"
        );
        assert_eq!(git(&["branch", "--show-current"]), "main\n");
    }

    #[test]
    fn git_operations_and_worktrees_target_the_daemon_directory() {
        if crate::repository::isolated_git_test(concat!(
            module_path!(),
            "::git_operations_and_worktrees_target_the_daemon_directory"
        )) {
            return;
        }
        let directory = crate::repository::retained_managed_fixture_directory();
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
        std::fs::write(root.join(".gitignore"), "!/.worktrees/\n").unwrap();
        assert!(
            invoke(GitOperation::CreateWorktree {
                name: "conflict".into(),
                branch: "feature/conflict".into(),
            })
            .is_err()
        );
        assert!(!root.join(".worktrees/conflict").exists());
        assert_eq!(
            std::fs::read(root.join(".gitignore")).unwrap(),
            b"!/.worktrees/\n"
        );
        std::fs::remove_file(root.join(".gitignore")).unwrap();
        invoke(GitOperation::CreateWorktree {
            name: "dev".into(),
            branch: "feature/dev".into(),
        })
        .unwrap();
        assert!(root.join(".worktrees/dev/-option").exists());
        assert_eq!(
            std::fs::read_to_string(root.join(".git/info/exclude"))
                .unwrap()
                .lines()
                .filter(|line| *line == "/.worktrees/")
                .count(),
            1,
        );
        assert!(
            !std::fs::read_to_string(root.join(".gitignore"))
                .unwrap_or_default()
                .contains(".worktrees")
        );
        std::fs::write(root.join(".worktrees/dev/untracked"), "keep").unwrap();
        let check =
            |name: &str| match invoke(GitOperation::CheckWorktreeRemoval { name: name.into() }) {
                Ok(Reply::Text(refusal)) => refusal,
                other => panic!("{other:?}"),
            };
        let refusal = check("dev");
        assert!(refusal.contains("uncommitted or untracked"), "{refusal}");
        // Hiding untracked files from status hides them from Git's own check,
        // which would then delete them; the device still counts them.
        run(&["config", "status.showUntrackedFiles", "no"]);
        assert!(!check("dev").is_empty());
        assert!(invoke(GitOperation::RemoveWorktree { name: "dev".into() }).is_err());
        assert!(root.join(".worktrees/dev/untracked").exists());
        run(&["config", "--unset", "status.showUntrackedFiles"]);
        std::fs::remove_file(root.join(".worktrees/dev/untracked")).unwrap();
        // The check changes nothing.
        assert_eq!(check("dev"), "");
        assert!(root.join(".worktrees/dev").is_dir());
        assert!(check("missing").contains("not registered"));
        invoke(GitOperation::RemoveWorktree { name: "dev".into() }).unwrap();
        assert!(!root.join(".worktrees/dev").exists());
        // A detached worktree whose HEAD alone holds a commit is kept; once
        // a branch holds that commit it is removed.
        invoke(GitOperation::CreateWorktree {
            name: "detached".into(),
            branch: "feature/detached".into(),
        })
        .unwrap();
        let detached = root.join(".worktrees/detached");
        let run_in = |arguments: &[&str]| {
            let output = std::process::Command::new("git")
                .current_dir(&detached)
                .args([
                    "-c",
                    "user.name=Test",
                    "-c",
                    "user.email=test@example.invalid",
                ])
                .args(arguments)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        };
        run_in(&["switch", "--detach"]);
        run_in(&["commit", "--allow-empty", "-m", "detached only"]);
        let refusal = check("detached");
        assert!(refusal.contains("1 commit on no branch"), "{refusal}");
        let refusal = invoke(GitOperation::RemoveWorktree {
            name: "detached".into(),
        })
        .unwrap_err();
        assert!(refusal.contains("1 commit on no branch"), "{refusal}");
        assert!(detached.is_dir());
        run_in(&["branch", "rescue"]);
        invoke(GitOperation::RemoveWorktree {
            name: "detached".into(),
        })
        .unwrap();
        assert!(!detached.exists());
        // Git removes a worktree with a checked-out submodule only with
        // --force, so the device says so before anything closes; one never
        // checked out there is removed as Git would remove it.
        invoke(GitOperation::CreateWorktree {
            name: "modules".into(),
            branch: "feature/modules".into(),
        })
        .unwrap();
        let modules = root.join(".worktrees/modules");
        let head = String::from_utf8(git_output(&modules, &["rev-parse", "HEAD"])).unwrap();
        git_output(
            &modules,
            &[
                "update-index",
                "--add",
                "--cacheinfo",
                &format!("160000,{},vendored", head.trim()),
            ],
        );
        std::fs::write(
            modules.join(".gitmodules"),
            "[submodule \"vendored\"]\n\tpath = vendored\n\turl = ./vendored\n",
        )
        .unwrap();
        git_output(&modules, &["add", ".gitmodules"]);
        git_output(
            &modules,
            &[
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.invalid",
                "commit",
                "-q",
                "-m",
                "gitlink",
            ],
        );
        // Checking out a worktree leaves a submodule's directory empty.
        std::fs::create_dir(modules.join("vendored")).unwrap();
        assert_eq!(check("modules"), "");
        git_output(&modules.join("vendored"), &["init", "-q"]);
        let refusal = check("modules");
        assert!(refusal.contains("checked-out submodules"), "{refusal}");
        assert!(
            invoke(GitOperation::RemoveWorktree {
                name: "modules".into()
            })
            .is_err()
        );
        assert!(modules.join("vendored/.git").is_dir());
        std::fs::remove_dir_all(modules.join("vendored/.git")).unwrap();
        invoke(GitOperation::RemoveWorktree {
            name: "modules".into(),
        })
        .unwrap();
        assert!(!modules.exists());
        // The remote project can itself be an external linked checkout.
        // Remove the prior rule so this exercises its real policy write.
        let linked_directory = crate::repository::retained_managed_fixture_directory();
        let linked = linked_directory.path().join("checkout");
        run(&[
            "worktree",
            "add",
            "--detach",
            linked.to_str().unwrap(),
            "HEAD",
        ]);
        std::fs::write(root.join(".git/info/exclude"), b"").unwrap();
        execute(Operation::Git {
            root: linked.clone(),
            operation: GitOperation::CreateWorktree {
                name: "nested".into(),
                branch: "feature/nested".into(),
            },
        })
        .unwrap();
        assert!(linked.join(".worktrees/nested/-option").exists());
        assert_eq!(
            std::fs::read(root.join(".git/info/exclude")).unwrap(),
            b"/.worktrees/\n"
        );
        assert!(
            matches!(execute(Operation::Git { root: linked, operation: GitOperation::Status }).unwrap(), Reply::Text(text) if !text.contains(".worktrees"))
        );
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
        let Some(group) = crate::fs_safety::acl_fixture::shared_group(&file) else {
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
            let original_mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o7777;
            #[cfg(target_os = "linux")]
            assert_eq!(original_mode, mode);
            #[cfg(target_os = "macos")]
            assert_eq!(original_mode & 0o777, mode & 0o777);
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
                original_mode
            );
        }
    }

    // The fixture writes Linux's POSIX ACL extended attribute.
    #[cfg(target_os = "linux")]
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
    let authority = crate::agent_execution::Authority::from_environment();
    request_as(
        operation,
        authority,
        crate::control::daemon_autostart_disabled(),
        || crate::terminal::ipc::daemon_socket_path().map_err(|error| error.to_string()),
        ready_client,
    )
}

/// Answers a source-control request where the window's worktrees are: on
/// this computer with its own Git, or in a device window by the device's.
/// `cancellation` stops a local answer; a device's runs to its end.
#[cfg(feature = "ui")]
pub fn answer_repository(
    request: crate::repository::RepositoryRequest,
    cancellation: &crate::repository::CancellationToken,
) -> Result<crate::repository::RepositoryReply, String> {
    if !crate::device::is_remote() {
        return request
            .run(
                &crate::repository::RepositoryService::system("git"),
                cancellation,
            )
            .map_err(|error| error.to_string());
    }
    let request = serde_json::to_string(&request).map_err(|error| error.to_string())?;
    match request_for_reader(Operation::Repository(request))? {
        Reply::Repository(encoded) => serde_json::from_str(&encoded)
            .map_err(|error| format!("The device's answer could not be read: {error}")),
        _ => Err("Unexpected device response".into()),
    }
}

/// [`request`] for an operation whose refusal is shown to the person who
/// asked for it in place of its result. The daemon words its own refusals for
/// them; the protocol code it attaches is for programs, so only a transport
/// failure keeps the client's framing.
#[cfg(feature = "ui")]
pub fn request_for_reader(operation: Operation) -> Result<Reply, String> {
    let authority = crate::agent_execution::Authority::from_environment();
    request_described(
        operation,
        authority,
        crate::control::daemon_autostart_disabled(),
        || crate::terminal::ipc::daemon_socket_path().map_err(|error| error.to_string()),
        ready_client,
        reader_message,
    )
}

/// [`request`] for a caller that leaves its state as it was, and says so,
/// when the daemon was too busy to admit the operation.
#[cfg(feature = "ui")]
pub fn request_unless_busy(operation: Operation) -> Result<Reply, crate::control::RequestError> {
    let authority = crate::agent_execution::Authority::from_environment();
    request_described(
        operation,
        authority,
        crate::control::daemon_autostart_disabled(),
        || crate::terminal::ipc::daemon_socket_path().map_err(|error| error.to_string()),
        ready_client,
        crate::control::RequestError::from,
    )
}

/// A User connection to the daemon, started if need be, on the connection
/// whose handshake proved it ready.
fn ready_client() -> Result<crate::control::ControlClient, ReadyError> {
    crate::terminal::connect_ready_daemon()
        .map(|ready| ready.client)
        .map_err(|error| ready_error(error, crate::device::is_remote()))
}

/// A device's protocol refusal stays typed so it can say which side to
/// update. This computer's own daemon keeps its rebuild guidance.
fn ready_error(error: crate::terminal::HostedTerminalStartError, remote: bool) -> ReadyError {
    use crate::terminal::HostedTerminalStartError;
    match error {
        HostedTerminalStartError::Connect(
            error @ crate::control::ControlError::UnsupportedVersion { .. },
        ) => ReadyError::Control(error),
        HostedTerminalStartError::HostIncompatible { actual } if remote => {
            ReadyError::Control(crate::control::ControlError::UnsupportedVersion { actual })
        }
        error => ReadyError::Other(error.to_string()),
    }
}

/// Why no ready connection was made. A protocol mismatch keeps its type, so
/// a caller can say which side is older; everything else is its message.
enum ReadyError {
    Control(crate::control::ControlError),
    Other(String),
}

impl From<String> for ReadyError {
    fn from(message: String) -> Self {
        Self::Other(message)
    }
}

#[cfg(any(feature = "ui", test))]
fn reader_message(error: crate::control::ControlError) -> String {
    match error {
        crate::control::ControlError::Remote { message, .. } => message,
        error => error.to_string(),
    }
}

fn request_as(
    operation: Operation,
    authority: crate::agent_execution::Authority,
    no_start: bool,
    selected_socket: impl FnOnce() -> Result<PathBuf, String>,
    ready_client: impl FnOnce() -> Result<crate::control::ControlClient, ReadyError>,
) -> Result<Reply, String> {
    request_described(
        operation,
        authority,
        no_start,
        selected_socket,
        ready_client,
        |error| error.to_string(),
    )
}

fn request_described<E: From<String>>(
    operation: Operation,
    authority: crate::agent_execution::Authority,
    no_start: bool,
    selected_socket: impl FnOnce() -> Result<PathBuf, String>,
    ready_client: impl FnOnce() -> Result<crate::control::ControlClient, ReadyError>,
    mut describe: impl FnMut(crate::control::ControlError) -> E,
) -> Result<Reply, E> {
    // A session's capability proves its lineage to the device as the
    // connection authenticates (`ControlClient::for_device`); the device then
    // admits only the path lookups a session needs, and refuses the rest.
    // Presence, including an empty/invalid capability, must never become User
    // startup or scope. Use the same captured authority after resolving a socket.
    let mut client = if authority.token().is_some() || no_start {
        crate::control::ControlClient::connect(&selected_socket()?).map_err(
            |error| match error {
                error @ crate::control::ControlError::UnsupportedVersion { .. } => describe(error),
                error => E::from(error.to_string()),
            },
        )?
    } else {
        ready_client().map_err(|error| match error {
            ReadyError::Control(error) => describe(error),
            ReadyError::Other(message) => E::from(message),
        })?
    };
    client
        .authenticate(authority)
        .and_then(|()| client.device_operation(operation))
        .map_err(describe)
}

/// Why a device operation failed: refused to the caller, or not carried out.
#[derive(Debug)]
pub(crate) enum Failure {
    /// This computer or the device refused it, for the reason given.
    Refused(String),
    /// The device's daemon speaks this other protocol.
    Incompatible(u16),
    Failed(String),
}

impl From<String> for Failure {
    fn from(message: String) -> Self {
        Self::Failed(message)
    }
}

/// [`request`] that tells a refusal from a failure to reach the device.
pub(crate) fn request_refusable(operation: Operation) -> Result<Reply, Failure> {
    request_described(
        operation,
        crate::agent_execution::Authority::from_environment(),
        crate::control::daemon_autostart_disabled(),
        || crate::terminal::ipc::daemon_socket_path().map_err(|error| error.to_string()),
        ready_client,
        refusable,
    )
}

fn refusable(error: crate::control::ControlError) -> Failure {
    match error {
        crate::control::ControlError::DeviceAccess(message)
        | crate::control::ControlError::Remote {
            code: crate::terminal_ipc::ProtocolErrorCode::PermissionDenied,
            message,
            ..
        } => Failure::Refused(message),
        crate::control::ControlError::UnsupportedVersion { actual } => {
            Failure::Incompatible(actual)
        }
        error => Failure::Failed(error.to_string()),
    }
}

#[cfg(test)]
mod no_start_entry_tests;
#[cfg(test)]
mod request_tests;

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
    pub catalogs: std::collections::BTreeMap<
        crate::agent::Provider,
        crate::account::cache::CatalogObservation,
    >,
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
    let search_path = probe::current_search_path();
    let installed = probe::provider_installations(search_path.as_deref());
    let cache_keys = registry
        .accounts(homes.as_ref())
        .into_iter()
        .filter_map(|account| account.cache_key().map(|key| (account.id(), key)))
        .collect();
    let snapshot = AccountSnapshot {
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
            .map(|cache| cache.catalog_observations(now))
            .unwrap_or_default(),
        installed,
    };
    serde_json::to_value(snapshot)
        .map(Reply::Accounts)
        .map_err(|error| error.to_string())
}

/// Chooses the default account for `provider`, as
/// [`Operation::AccountDefault`] asks, under the daemon's shared admission.
#[cfg(feature = "host")]
pub fn account_default(
    expected_revision: u64,
    provider: crate::agent::Provider,
    selection: &crate::account::preferences::DefaultSelection,
) -> Result<Reply, crate::account::remote::LifecycleError> {
    let store = crate::account::registry::Store::installation()
        .ok_or("remote account registry is unavailable")?;
    let homes = crate::account::home::Homes::installation();
    let now = crate::chat::now_ms().unwrap_or_default();
    let cached = crate::account::cache::Cache::installation()
        .map(|cache| cache.load(now))
        .unwrap_or_default();
    let registry = crate::account::preferences::set_default(
        &store,
        homes.as_ref(),
        &cached,
        expected_revision,
        provider,
        selection,
    )?;
    Ok(registry_reply(registry)?)
}

/// Saves the account preferences `proposal` changes, as
/// [`Operation::AccountsUpdate`] asks, under the daemon's shared admission:
/// a held account list refuses at once rather than wait, retryably.
#[cfg(feature = "host")]
pub fn update_accounts(
    expected_revision: u64,
    proposal: serde_json::Value,
) -> Result<Reply, crate::account::remote::LifecycleError> {
    use crate::account::registry::{Registry, Store};
    let proposed: Registry = serde_json::from_value(proposal).map_err(|error| error.to_string())?;
    let store = Store::installation().ok_or("remote account registry is unavailable")?;
    update_accounts_in(&store, expected_revision, proposed)
}

#[cfg(feature = "host")]
fn update_accounts_in(
    store: &crate::account::registry::Store,
    expected_revision: u64,
    proposed: crate::account::registry::Registry,
) -> Result<Reply, crate::account::remote::LifecycleError> {
    use crate::account::registry::Registry;
    let (registry, result) = store.try_update(|current| {
        if current.revision() != expected_revision {
            return Err("accounts changed on the device; reload before saving".to_owned());
        }
        // This endpoint edits preferences. Credential-home ownership and
        // pending create/remove transactions cannot be replaced by a client.
        let strip_preferences = |registry: &Registry| -> Result<serde_json::Value, String> {
            let mut value = serde_json::to_value(registry).map_err(|error| error.to_string())?;
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
    })?;
    result.map_err(crate::account::remote::LifecycleError::Failed)?;
    Ok(registry_reply(registry)?)
}

#[cfg(feature = "host")]
fn registry_reply(registry: crate::account::registry::Registry) -> Result<Reply, String> {
    serde_json::to_value(registry)
        .map(Reply::AccountRegistry)
        .map_err(|error| error.to_string())
}
/// Probe a home's usage. After a sign-in, a Claude home's cached profile
/// metadata belongs to the previous login and is discarded first.
#[cfg(feature = "host")]
fn probe_account(
    account: crate::account::Account,
    force: bool,
    login_changed: bool,
) -> Result<Reply, String> {
    let cache = crate::account::cache::Cache::installation();
    if login_changed
        && account.provider == crate::agent::Provider::Claude
        && let (Some(cache), Some(key)) = (&cache, account.cache_key())
    {
        cache
            .invalidate_plan(&key)
            .map_err(|error| error.to_string())?;
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| error.to_string())?;
    let outcome = runtime.block_on(crate::account::probe::run(account, cache, force));
    serde_json::to_value(outcome)
        .map(Reply::AccountProbe)
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
    if account.provider == Provider::Omp {
        return Err("open an Omp terminal on this device and use /login".into());
    }
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
    // The sign-in this device's own Accounts pane runs, so one made from
    // another device, such as Antigravity's on a headless machine, is the
    // same.
    let recipe =
        crate::account::remote::login_recipe_with_path(&account, program, search_path.as_deref())?;
    if account.provider == Provider::Claude
        && let (Some(cache), Some(key)) = (
            crate::account::cache::Cache::installation(),
            account.cache_key(),
        )
    {
        cache
            .invalidate_plan(&key)
            .map_err(|error| error.to_string())?;
    }
    Ok(Reply::Login(recipe))
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

/// Checks a worktree name the device creates or removes under `.worktrees`.
/// A device window applies it before asking, so it builds without `host`.
pub fn worktree_name(name: &str) -> Result<(), String> {
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
            Operation::AccountsRefresh,
            Operation::AccountDefault {
                expected_revision: 3,
                provider: crate::agent::Provider::Claude,
                selection: crate::account::preferences::DefaultSelection::Automatic,
            },
            Operation::AccountDefault {
                expected_revision: 3,
                provider: crate::agent::Provider::Codex,
                selection: crate::account::preferences::DefaultSelection::Account(
                    "codex/system".parse().unwrap(),
                ),
            },
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
                account: account.clone(),
                force: true,
            });
            roundtrip(&Operation::AccountProbeAfterLogin { account });
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
