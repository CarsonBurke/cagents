//! Conservative automatic cleanup and process-owned protection of worktrees.
//!
//! Locks live outside repositories and are never unlinked: every process must
//! contend on the same file description, including across daemon restarts.
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    io,
    path::{Path, PathBuf},
};

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct RemovalRequest {
    pub repository: PathBuf,
    pub path: PathBuf,
    pub branch: String,
    pub head_oid: String,
    pub source_repository: crate::github_identity::GitHubRepoKey,
    pub pull_request: crate::github_identity::PullRequestKey,
}

/// A worktree Git has just removed at the user's request, as it was right
/// before the removal.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct RemovedWorktree {
    /// The canonical common directory of its repository.
    pub common_dir: PathBuf,
    /// Its canonical Git administrative directory.
    pub admin_dir: PathBuf,
    pub path: PathBuf,
    /// The branch it had checked out, as a full ref.
    pub branch: Option<String>,
}

#[cfg(feature = "host")]
pub(crate) struct PreparedRemoval {
    request: RemovalRequest,
    target: FencedTarget,
}

/// A worktree identified before a fenced removal takes its fences, which the
/// removal confirms again once it holds them.
#[cfg(feature = "host")]
pub(crate) struct FencedTarget {
    pub(crate) repository: PathBuf,
    pub(crate) path: PathBuf,
    /// The checked-out branch's short name; `None` when detached.
    pub(crate) branch: Option<String>,
    pub(crate) common_dir: PathBuf,
    pub(crate) admin_dir: PathBuf,
    pub(crate) directory_identity: (u64, u64),
}

#[cfg(feature = "host")]
impl FencedTarget {
    /// Identifies the worktree at `path`, a canonical directory registered
    /// with `repository`, as it is now.
    pub(crate) fn capture(
        repository: &Path,
        path: &Path,
        branch: Option<String>,
    ) -> Result<Self, String> {
        use std::os::unix::fs::MetadataExt;
        let cancellation = crate::process::CancellationToken::new();
        let service = crate::repository::RepositoryService::system("git");
        let common_dir = service
            .common_dir(repository, &cancellation)
            .map_err(|error| error.to_string())?;
        if service
            .common_dir(path, &cancellation)
            .map_err(|error| error.to_string())?
            != common_dir
        {
            return Err("Worktree repository changed".into());
        }
        // Files-backend locks do not fence a reftable repository. Preserve
        // unsupported ref storage rather than claiming the same guarantee.
        use crate::process::ProcessExecutor;
        let command = crate::process::CommandSpec::new("git")
            .cwd(repository)
            .args(["config", "--get", "extensions.refStorage"]);
        let output = crate::process::SystemProcessExecutor
            .execute(&command, &cancellation)
            .map_err(|error| error.to_string())?;
        if (!output.stdout.is_empty() && output.stdout.as_slice() != b"files\n")
            || (output.exit_code != Some(0) && output.exit_code != Some(1))
        {
            return Err("Repository reference storage is unsupported".into());
        }
        let admin_dir = service
            .git_dir(path, &cancellation)
            .map_err(|error| error.to_string())?;
        let metadata = path.symlink_metadata().map_err(|error| error.to_string())?;
        if !metadata.is_dir() {
            return Err("Worktree is not a directory".into());
        }
        Ok(Self {
            repository: repository.to_owned(),
            path: path.to_owned(),
            branch,
            common_dir,
            admin_dir,
            directory_identity: (metadata.dev(), metadata.ino()),
        })
    }
}

#[cfg(any(feature = "host", feature = "ui"))]
pub(crate) fn automatic_retention_reason(path: &Path) -> String {
    format!(
        "Kept {}: automatic cleanup cannot exclude new files written by other programs. Review the worktree, then choose Delete worktree and confirm if you want to discard its files. The directory and branch have been retained.",
        path.display()
    )
}

#[cfg(any(feature = "host", feature = "ui"))]
fn exact_merged_head(request: &RemovalRequest, pr: &crate::github::PullRequest) -> bool {
    pr.state.0 == "MERGED"
        && pr.number == request.pull_request.number
        && pr.head_ref_name == request.branch
        && pr.head_ref_oid == request.head_oid
        && pr.belongs_to_head(&request.source_repository)
}

#[cfg(feature = "host")]
fn checked_record(request: &RemovalRequest) -> Result<crate::repository::WorktreeRecord, String> {
    let cancellation = crate::process::CancellationToken::new();
    let service = crate::repository::RepositoryService::system("git");
    crate::repository::ValidatedBranchName::parse(&request.branch)
        .map_err(|error| error.to_string())?;
    if matches!(request.branch.as_str(), "main" | "master") {
        return Err("Main branches are preserved".into());
    }
    crate::repository::Oid::parse_object(&request.head_oid).map_err(|error| error.to_string())?;
    for path in [&request.repository, &request.path] {
        if !path.is_absolute() {
            return Err("Worktree path is not absolute".into());
        }
        crate::secure_file::reject_symlink_components(path).map_err(|error| error.to_string())?;
        if path.canonicalize().map_err(|error| error.to_string())? != *path {
            return Err("Worktree path changed".into());
        }
    }
    let records = service
        .list_worktrees(&request.repository, &cancellation)
        .map_err(|error| error.to_string())?;
    let record = records
        .iter()
        .find(|record| record.path == request.path)
        .ok_or("Worktree is no longer registered")?;
    if records
        .first()
        .is_some_and(|primary| primary.path == record.path)
        || record.bare
        || record.detached
        || record.lock.locked
        || record.prunable_reason.is_some()
    {
        return Err("Primary, detached, locked or unavailable worktrees are preserved".into());
    }
    if record.branch.as_deref() != Some(format!("refs/heads/{}", request.branch).as_str())
        || record.head_oid.as_deref() != Some(request.head_oid.as_str())
    {
        return Err("Worktree branch or commit changed".into());
    }
    let common = service
        .common_dir(&request.repository, &cancellation)
        .map_err(|error| error.to_string())?;
    if service
        .common_dir(&request.path, &cancellation)
        .map_err(|error| error.to_string())?
        != common
    {
        return Err("Worktree repository changed".into());
    }
    let source =
        crate::github::worktree_head_repository(&request.path, &request.branch, &cancellation)
            .ok_or("Worktree source repository is unknown")?;
    if !source.same_repository(&request.source_repository) {
        return Err("Worktree source repository changed".into());
    }
    let status = service
        .status_for_removal(&request.path, &cancellation)
        .map_err(|error| error.to_string())?;
    if status.untracked_omitted || status.dirty.is_dirty() || !status.entries.is_empty() {
        return Err("Worktree has changes or untracked files".into());
    }
    let ignored = service
        .notable_ignored_files(&request.path, &cancellation)
        .map_err(|error| error.to_string())?;
    if !ignored.is_empty() {
        return Err(format!(
            "Kept: ignored files {}",
            ignored
                .iter()
                .take(5)
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    Ok(record.clone())
}

#[cfg(feature = "host")]
impl PreparedRemoval {
    /// GitHub is read afresh before taking exclusive lifecycle fences. Sidebar
    /// branch badges and cached archived PRs never authorize removal.
    pub(crate) fn prepare(request: RemovalRequest) -> Result<Self, String> {
        checked_record(&request)?;
        let cancellation = crate::process::CancellationToken::new();
        let pr = crate::github::GitHubService::system("gh")
            .view(&request.pull_request, &cancellation)
            .map_err(|error| error.to_string())?;
        if request.branch == pr.base_ref_name || !exact_merged_head(&request, &pr) {
            return Err("The exact current worktree commit is not confirmed merged".into());
        }
        Ok(Self {
            target: FencedTarget::capture(
                &request.repository,
                &request.path,
                Some(request.branch.clone()),
            )?,
            request,
        })
    }

    /// Caller retains the daemon's exclusive admission gate through this call.
    /// Advisory leases cannot fence later writes by unrelated programs.
    /// Automatic cleanup retains the checkout for explicit confirmation.
    pub(crate) fn remove(
        self,
        _check_quiescence: impl FnOnce(&[crate::instance::VerifiedWindow]) -> Result<(), String>,
    ) -> Result<crate::repository::RemovedWorktree, String> {
        let Self {
            request,
            target: _target,
        } = self;
        Err(automatic_retention_reason(&request.path))
    }
}

/// Which running windows a fenced removal accepts. Every window must have
/// established directory protection, so none can show the worktree unfenced.
#[cfg(feature = "host")]
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum WindowPolicy {
    /// At least one window, as for a removal a window asked for.
    RequireOne,
    /// Any number, none included, as for the command line.
    AllowNone,
}

/// Removes a worktree behind every fence that keeps its content from changing
/// or coming back into use: the window launch fence, `admit` while windows are
/// verified, the independent-editor census, the exclusive directory lease,
/// and Git's own locks on HEAD and the branch. `verify` runs once the
/// target's identity is confirmed under those locks; it may still refuse, or
/// act on the worktree before it goes. Git's locks on packed refs and config
/// are checked after it. `force` removes
/// it even with changes, which `verify` must have accounted for; Git gets
/// `timeout` to delete the tree.
///
/// Automatic cleanup holds the exclusive control gate through this call,
/// which `admit` checks for global quiescence. A removal the worktree's own
/// reservation fences holds no control admission, so a long inspection and
/// deletion hold off no other request.
#[cfg(feature = "host")]
pub(crate) fn remove_fenced(
    target: FencedTarget,
    windows: WindowPolicy,
    admit: impl FnOnce(&[crate::instance::VerifiedWindow]) -> Result<(), String>,
    verify: impl FnOnce() -> Result<(), String>,
    force: bool,
    timeout: std::time::Duration,
) -> Result<crate::repository::RemovedWorktree, String> {
    use crate::process::ProcessExecutor;
    use std::os::unix::fs::MetadataExt;
    let launch = crate::instance::worktree_mutation_launch_fence().map_err(|_| {
        "An update or another worktree removal is in progress; retry shortly".to_owned()
    })?;
    let policy = windows;
    let windows = crate::instance::verified_running_windows().map_err(|error| error.to_string())?;
    if (policy == WindowPolicy::RequireOne && windows.is_empty())
        || windows
            .iter()
            .any(|window| !window.protects_worktrees().unwrap_or(false))
    {
        return Err("Window directory protection is not ready".into());
    }
    // Busy applications skip before the independent-editor process census.
    // Admission remains fenced while directory leases are then acquired.
    admit(&windows)?;
    let _editors =
        crate::instance::verified_running_editors().map_err(|error| error.to_string())?;
    let _directory = acquire(
        &crate::instance::runtime_directory().map_err(|error| error.to_string())?,
        &target.path,
        true,
    )
    .map_err(|_| "Worktree is selected or open in an editor".to_owned())?;
    // A window launched from here on cannot take the shared lease that showing
    // the worktree needs, as a running window that selects it now cannot.
    drop(launch);
    // Git's own metadata locks prevent checkout, commit, and packed-ref
    // maintenance from changing the confirmed head during removal.
    let mut head = crate::repository::GitFileLock::acquire(&target.admin_dir.join("HEAD"))?;
    let branch = target
        .branch
        .as_ref()
        .map(|branch| {
            // A packed branch below a directory of refs may have no loose
            // directory to lock in; Git creates leading directories alike.
            let loose = target.common_dir.join("refs/heads").join(branch);
            if Path::new(branch)
                .components()
                .any(|part| !matches!(part, std::path::Component::Normal(_)))
            {
                return Err(format!("Branch {branch} is not a valid branch name"));
            }
            if let Some(parent) = loose.parent() {
                std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
            }
            crate::repository::GitFileLock::acquire(&loose)
        })
        .transpose()?;
    head.check()?;
    if let Some(branch) = &branch {
        branch.check()?;
    }
    let metadata = target.path.metadata().map_err(|error| error.to_string())?;
    if (metadata.dev(), metadata.ino()) != target.directory_identity {
        return Err("Worktree directory was replaced".into());
    }
    let cancellation = crate::process::CancellationToken::new();
    let service = crate::repository::RepositoryService::system("git");
    if service
        .common_dir(&target.repository, &cancellation)
        .map_err(|error| error.to_string())?
        != target.common_dir
        || service
            .git_dir(&target.path, &cancellation)
            .map_err(|error| error.to_string())?
            != target.admin_dir
    {
        return Err("Worktree administrative identity changed".into());
    }
    verify()?;
    // The repository-wide locks confirm that no ref packing or config write
    // is in flight as the removal commits, then go: the HEAD and branch locks
    // alone fix the confirmed head while Git deletes the tree, which can take
    // minutes that other worktrees of the repository need not wait out.
    for name in ["packed-refs", "config"] {
        crate::repository::GitFileLock::acquire(&target.common_dir.join(name))
            .and_then(|lock| lock.check())
            .map_err(|error| {
                format!(
                    "Another Git operation in this repository is running ({error}); retry shortly"
                )
            })?;
    }
    let mut command = crate::process::CommandSpec::new("git")
        .cwd(&target.repository)
        .args(["worktree", "remove"]);
    if force {
        command = command.arg("--force");
    }
    let command = command
        .arg("--")
        .arg(target.path.as_os_str())
        .timeout(timeout)
        .output_limits(64 * 1024, 64 * 1024);
    let result = crate::process::SystemProcessExecutor
        .execute(&command, &cancellation)
        .and_then(|output| output.require_success(&command.program));
    if let Err(error) = result {
        // A timeout does not certify removal. Preserve the UI row unless
        // both Git's registration and the exact directory are gone.
        let records = service
            .list_worktrees(&target.repository, &cancellation)
            .map_err(|_| error.reported_detail())?;
        if target.path.exists() || records.iter().any(|record| record.path == target.path) {
            return Err(error.reported_detail());
        }
    }
    head.retired_with_directory();
    Ok(crate::repository::RemovedWorktree {
        common_dir: target.common_dir,
        admin_dir: target.admin_dir,
        path: target.path,
        branch: target.branch.map(|branch| format!("refs/heads/{branch}")),
    })
}

pub struct Protection {
    directory: PathBuf,
    _locks: Vec<crate::instance::OwnedLock>,
    _directories: Vec<std::fs::File>,
}

impl std::fmt::Debug for Protection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("Protection").field(&self.directory).finish()
    }
}

impl Protection {
    /// Actual retained native descriptors, including every discovered Git
    /// ancestor's two locks and held directory. Does no filesystem work.
    pub(crate) fn retained_descriptor_count(&self) -> usize {
        self._locks.len() + self._directories.len()
    }

    /// Worker-side proof that this already-acquired lease protects the exact
    /// model-open root; lexical path equality is insufficient after replacement.
    pub(crate) fn protects_root(&self, root: &crate::fs_safety::Root) -> io::Result<bool> {
        use std::os::unix::fs::MetadataExt;
        let expected = root.metadata()?;
        for directory in &self._directories {
            let actual = directory.metadata()?;
            if (expected.dev(), expected.ino()) == (actual.dev(), actual.ino()) {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Independent open descriptions retain shared protection in exec children.
    /// OwnedLock explicitly unlocks on drop, so duplicating its descriptor would
    /// let the parent's retirement unlock the child's shared open description.
    pub(crate) fn retained_child_files(&self) -> io::Result<Vec<std::sync::Arc<std::fs::File>>> {
        use std::os::fd::AsRawFd;
        use std::os::unix::fs::MetadataExt;
        let mut files = Vec::new();
        for lock in &self._locks {
            let file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(format!("/proc/self/fd/{}", lock.as_raw_fd()))?;
            let old = lock.metadata()?;
            let new = file.metadata()?;
            if (old.dev(), old.ino()) != (new.dev(), new.ino()) {
                return Err(io::Error::other("directory protection inode changed"));
            }
            // SAFETY: file owns this independent open description. It only
            // closes on drop; an inherited child keeps its SH lock alive.
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_SH | libc::LOCK_NB) } != 0 {
                return Err(io::Error::last_os_error());
            }
            files.push(std::sync::Arc::new(file));
        }
        for directory in &self._directories {
            files.push(std::sync::Arc::new(directory.try_clone()?));
        }
        Ok(files)
    }

    pub(crate) fn directory(&self) -> &Path {
        &self.directory
    }
}

fn lock_path(runtime: &Path, directory: &Path) -> io::Result<PathBuf> {
    use std::os::unix::ffi::OsStrExt;
    let locks = runtime.join("cagents-worktree-protection");
    crate::secure_file::prepare_private_directory(&locks)?;
    Ok(locks.join(format!(
        "{:x}.lock",
        Sha256::digest(directory.as_os_str().as_bytes())
    )))
}

fn acquire(runtime: &Path, directory: &Path, exclusive: bool) -> io::Result<Protection> {
    use std::os::unix::fs::OpenOptionsExt;
    acquire_with_directory_pin(runtime, directory, exclusive, |path| {
        std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(path)
    })
}
fn acquire_with_directory_pin(
    runtime: &Path,
    directory: &Path,
    exclusive: bool,
    pin: impl Fn(&Path) -> io::Result<std::fs::File>,
) -> io::Result<Protection> {
    use std::os::unix::fs::MetadataExt;
    crate::secure_file::reject_symlink_components(directory)?;
    let directory = directory.canonicalize()?;
    // A nested folder or submodule editor also protects containing Git worktrees.
    // Ordinary folder ancestors do not consume a descriptor each.
    let mut roots = vec![directory.clone()];
    if !exclusive {
        for ancestor in directory.ancestors().skip(1) {
            match ancestor.join(".git").symlink_metadata() {
                Ok(_) => roots.push(ancestor.to_owned()),
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }
    }
    let mut locks = Vec::with_capacity(roots.len() * 2);
    let mut directories = Vec::with_capacity(roots.len());
    for root in roots {
        let held_directory = pin(&root)?;
        let identity = held_directory.metadata()?;
        let path = lock_path(runtime, &root)?;
        let inode_path = path.with_file_name(format!(
            "directory-{:x}-{:x}.lock",
            identity.dev(),
            identity.ino()
        ));
        // The path lease covers replacement at the same pathname. The inode
        // lease follows an externally moved worktree, while the held directory
        // descriptor prevents inode reuse until the protection retires.
        let mut lock_paths = [path, inode_path];
        lock_paths.sort();
        for path in lock_paths {
            let lock = crate::instance::owned_lock(
                &path,
                (if exclusive {
                    libc::LOCK_EX
                } else {
                    libc::LOCK_SH
                }) | libc::LOCK_NB,
            )?;
            let held = lock.metadata()?;
            let named = path.symlink_metadata()?;
            if !named.is_file() || (held.dev(), held.ino()) != (named.dev(), named.ino()) {
                return Err(io::Error::other("worktree protection lock was replaced"));
            }
            locks.push(lock);
        }
        let named_directory = root.symlink_metadata()?;
        if !named_directory.is_dir()
            || (named_directory.dev(), named_directory.ino()) != (identity.dev(), identity.ino())
        {
            return Err(io::Error::other(
                "protected directory changed while acquiring its leases",
            ));
        }
        directories.push(held_directory);
    }
    Ok(Protection {
        directory,
        _locks: locks,
        _directories: directories,
    })
}

static STANDALONE_EDITOR: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Declare the independent editor before constructing its UI. Readiness is
/// published separately, after its exact startup descriptors are protected.
pub fn prepare_standalone_editor_protection() {
    STANDALONE_EDITOR.store(true, std::sync::atomic::Ordering::Release);
}

static EDITOR_STARTUP_PROTECTIONS: std::sync::OnceLock<Vec<Protection>> =
    std::sync::OnceLock::new();

/// Independent ceditor windows share local directory leases without joining
/// workspace activation. Startup guards live until this editor process exits.
pub fn complete_standalone_editor_protection(paths: &[PathBuf]) -> io::Result<()> {
    if !STANDALONE_EDITOR.load(std::sync::atomic::Ordering::Acquire) {
        return Ok(());
    }
    let _launch = crate::instance::recovery_launch_fence()?;
    let runtime = crate::instance::runtime_directory()?;
    let guards = initial_editor_protections(&runtime, paths)?;
    EDITOR_STARTUP_PROTECTIONS
        .set(guards)
        .map_err(|_| io::Error::other("editor startup protection already initialized"))?;
    crate::instance::standalone_editor_protection_ready()
}

fn initial_editor_protections(runtime: &Path, paths: &[PathBuf]) -> io::Result<Vec<Protection>> {
    let mut directories = std::collections::BTreeSet::new();
    for path in paths {
        let absolute = if path.is_absolute() {
            path.clone()
        } else {
            std::env::current_dir()?.join(path)
        };
        let directory = if absolute.is_dir() {
            absolute.as_path()
        } else {
            absolute
                .parent()
                .ok_or_else(|| io::Error::other("editor path has no parent"))?
        };
        crate::secure_file::reject_symlink_components(directory)?;
        directories.insert(directory.canonicalize()?);
        if let Ok(target) = absolute.canonicalize()
            && target != absolute
            && !target.is_dir()
            && let Some(parent) = target.parent()
        {
            directories.insert(parent.to_owned());
        }
    }
    directories
        .into_iter()
        .map(|directory| acquire(runtime, &directory, false))
        .collect()
}

/// Worker-only physical lease for an explicit local filesystem operation,
/// including intake without an initialized window. This only protects the
/// original root against cooperating automatic cleanup; it grants no global
/// root-adoption, placement or publication authority. It never returns an
/// unprotected compatibility fallback.
pub fn protect_root_operation(root: &crate::fs_safety::Root) -> io::Result<Protection> {
    protect_root_operation_in_runtime(&crate::instance::runtime_directory()?, root)
}
fn protect_root_operation_in_runtime(
    runtime: &Path,
    root: &crate::fs_safety::Root,
) -> io::Result<Protection> {
    // An operation needs a physical directory pin, not enumeration rights.
    // Keep the ordinary path/host protection API's read-capable pins intact.
    let protection = acquire_with_directory_pin(runtime, root.path(), false, |path| {
        crate::fs_safety::Root::open(path)?.retained_directory()
    })?;
    if !protection.protects_root(root)? {
        return Err(io::Error::other(
            "The operation source root changed while acquiring cleanup protection; no file was changed.",
        ));
    }
    Ok(protection)
}

/// A displaying process protects the next directory before it exposes it.
/// Remote paths cannot be interpreted by this computer; remote automatic
/// cleanup remains unavailable until that window has equivalent host leases.
pub fn protect(directory: &Path) -> io::Result<Option<Protection>> {
    if (!crate::instance::window_is_initialized()
        && !STANDALONE_EDITOR.load(std::sync::atomic::Ordering::Acquire))
        || crate::device::selected().is_some()
    {
        return Ok(None);
    }
    acquire(&crate::instance::runtime_directory()?, directory, false).map(Some)
}

/// Local mutations need directory protection even outside an initialized UI.
/// Callers retain this guard until every inherited operation lease has retired.
pub(crate) fn protect_operation(directory: &Path) -> io::Result<Protection> {
    acquire(&crate::instance::runtime_directory()?, directory, false)
}

#[cfg(test)]
pub(crate) fn fixture_protection(
    runtime: &Path,
    directory: &Path,
    exclusive: bool,
) -> io::Result<Protection> {
    acquire(runtime, directory, exclusive)
}

/// Selected scopes and restored editor descriptors remain protected even
/// when their actual widgets are parked or have not mounted yet.
#[derive(Default)]
pub(crate) struct WindowProtections {
    directories: BTreeMap<PathBuf, Protection>,
    resolved: BTreeMap<PathBuf, PathBuf>,
    requested: std::collections::BTreeSet<PathBuf>,
    ready: Option<crate::instance::OwnedLock>,
    retry_after: Option<std::time::Instant>,
    last_failure: Option<String>,
    missing: std::collections::BTreeSet<PathBuf>,
    #[cfg(test)]
    fixture_runtime: Option<PathBuf>,
}

impl WindowProtections {
    fn suspend_readiness(&mut self) {
        self.ready = None;
        self.requested.clear();
        self.retry_after = None;
        self.last_failure = None;
    }

    #[cfg(test)]
    pub(crate) fn use_fixture_runtime(&mut self, runtime: PathBuf) {
        assert!(self.directories.is_empty());
        self.fixture_runtime = Some(runtime);
    }

    pub(crate) fn protect_next(&mut self, directory: &Path) -> io::Result<()> {
        #[cfg(test)]
        let protection = match &self.fixture_runtime {
            Some(runtime) => Some(acquire(runtime, directory, false)?),
            None => protect(directory)?,
        };
        #[cfg(not(test))]
        let protection = protect(directory)?;
        if let Some(protection) = protection {
            self.resolved
                .insert(directory.to_owned(), protection.directory.clone());
            self.directories
                .insert(protection.directory.clone(), protection);
        }
        Ok(())
    }
    pub(crate) fn sync(
        &mut self,
        directories: impl IntoIterator<Item = PathBuf>,
    ) -> io::Result<()> {
        if !crate::instance::window_is_initialized() {
            return Ok(());
        }
        if crate::device::selected().is_some() {
            // Retain earlier local guards across a device switch, but stop
            // certifying readiness until all local scopes are resynchronized.
            self.suspend_readiness();
            return Ok(());
        }
        self.sync_with(
            directories,
            std::time::Instant::now(),
            |path| path.canonicalize(),
            |path| protect(path)?.ok_or_else(|| io::Error::other("window protection unavailable")),
            crate::instance::window_protection_ready,
        )
    }

    /// Missing paths and failed leases are revisited on a slow timer. The
    /// ordinary unchanged-message path never touches the filesystem.
    fn sync_with(
        &mut self,
        directories: impl IntoIterator<Item = PathBuf>,
        now: std::time::Instant,
        mut canonicalize: impl FnMut(&Path) -> io::Result<PathBuf>,
        mut protect: impl FnMut(&Path) -> io::Result<Protection>,
        ready: impl FnOnce() -> io::Result<crate::instance::OwnedLock>,
    ) -> io::Result<()> {
        const RETRY_DELAY: std::time::Duration = std::time::Duration::from_secs(30);
        let requested = directories
            .into_iter()
            .collect::<std::collections::BTreeSet<_>>();
        let changed = requested != self.requested;
        if !changed
            && self.retry_after.is_none_or(|deadline| now < deadline)
            && (self.ready.is_some() || self.retry_after.is_some())
        {
            return Ok(());
        }
        if changed {
            self.last_failure = None;
        }
        self.requested = requested;
        let mut needed = std::collections::BTreeSet::new();
        let mut missing = std::collections::BTreeSet::new();
        let mut errors = Vec::new();
        // Continue after individual failures so every resolvable scope keeps
        // its lease. Preserve earlier guards until the replacement set is safe.
        for path in &self.requested {
            let directory = match canonicalize(path) {
                Ok(directory) => directory,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    missing.insert(path.clone());
                    // The path may have moved while an editor still uses its
                    // open descriptor. Keep its old inode lease until that
                    // requested scope leaves, without resolving the old path.
                    if let Some(previous) = self.resolved.get(path) {
                        needed.insert(previous.clone());
                    }
                    continue;
                }
                Err(error) => {
                    errors.push(format!("{}: {error}", path.display()));
                    continue;
                }
            };
            if !self.directories.contains_key(&directory) {
                match protect(&directory) {
                    Ok(guard) => {
                        self.directories.insert(directory.clone(), guard);
                    }
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {
                        missing.insert(path.clone());
                        if let Some(previous) = self.resolved.get(path) {
                            needed.insert(previous.clone());
                        }
                        continue;
                    }
                    Err(error) => {
                        errors.push(format!("{}: {error}", path.display()));
                        continue;
                    }
                }
            }
            self.resolved.insert(path.clone(), directory.clone());
            needed.insert(directory);
        }
        self.resolved
            .retain(|path, _| self.requested.contains(path));
        if missing != self.missing && !missing.is_empty() {
            tracing::debug!(
                ?missing,
                "window directories are missing; retaining any existing worktree leases"
            );
        }
        self.missing = missing;
        if errors.is_empty() {
            if self.ready.is_none() {
                match ready() {
                    Ok(guard) => self.ready = Some(guard),
                    Err(error) => errors.push(error.to_string()),
                }
            }
            if errors.is_empty() {
                self.directories
                    .retain(|directory, _| needed.contains(directory));
                self.retry_after = (!self.missing.is_empty()).then_some(now + RETRY_DELAY);
                self.last_failure = None;
                return Ok(());
            }
        }
        self.ready = None;
        self.retry_after = Some(now + RETRY_DELAY);
        let failure = errors.join("; ");
        if self.last_failure.as_ref() == Some(&failure) {
            return Ok(());
        }
        self.last_failure = Some(failure.clone());
        Err(io::Error::other(failure))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_directory() -> tempfile::TempDir {
        // Positive fixtures need physical paths even when TMPDIR names a symlink.
        // Keep the TempDir guard; the lease API still rejects symlink components.
        let parent = std::env::temp_dir().canonicalize().unwrap();
        tempfile::tempdir_in(parent).unwrap()
    }

    fn fixture_git(root: &Path) -> std::process::Command {
        let mut command = std::process::Command::new("git");
        command
            .args(["-c", "core.hooksPath=/dev/null", "-C"])
            .arg(root)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1");
        for name in [
            "GIT_DIR",
            "GIT_WORK_TREE",
            "GIT_COMMON_DIR",
            "GIT_INDEX_FILE",
            "GIT_OBJECT_DIRECTORY",
            "GIT_ALTERNATE_OBJECT_DIRECTORIES",
            "GIT_CONFIG_PARAMETERS",
            "GIT_CONFIG_COUNT",
        ] {
            command.env_remove(name);
        }
        command
    }

    fn request() -> RemovalRequest {
        let source =
            crate::github_identity::GitHubRepoKey::new("github.com", None, "source", "repository")
                .unwrap();
        RemovalRequest {
            repository: "/fixture/repository".into(),
            path: "/fixture/worktree".into(),
            branch: "feature".into(),
            head_oid: "a".repeat(40),
            source_repository: source.clone(),
            pull_request: crate::github_identity::PullRequestKey {
                repository: source,
                number: 9,
            },
        }
    }

    #[cfg(any(feature = "host", feature = "ui"))]
    #[test]
    fn merged_eligibility_requires_exact_commit_branch_and_source_identity() {
        let request = request();
        let mut pr: crate::github::PullRequest = serde_json::from_value(serde_json::json!({
            "number": 9, "title": "Fixture", "state": "MERGED", "headRefName": "feature",
            "headRefOid": "a".repeat(40), "headRepository": {"name": "repository"},
            "headRepositoryOwner": {"login": "source"}, "baseRefName": "main",
            "updatedAt": "2026-10-09T00:00:00Z", "url": "https://github.com/source/repository/pull/9"
        })).unwrap();
        assert!(exact_merged_head(&request, &pr));
        let original = pr.clone();
        for variation in 0..5 {
            pr = original.clone();
            match variation {
                0 => pr.state.0 = "CLOSED".into(),
                1 => pr.head_ref_oid = "b".repeat(40),
                2 => pr.head_ref_name = "reused-branch".into(),
                3 => pr.head_repository_owner.as_mut().unwrap().login = "fork".into(),
                _ => pr.head_repository = None,
            }
            assert!(!exact_merged_head(&request, &pr));
        }
    }

    #[cfg(feature = "host")]
    #[test]
    fn fresh_git_eligibility_preserves_dirty_untracked_locked_changed_and_primary_worktrees() {
        let directory = tempfile::tempdir_in("/var/tmp").unwrap();
        let repository = directory.path().join("repository");
        let worktree = directory.path().join("worktree");
        std::fs::create_dir(&repository).unwrap();
        let git = |root: &Path, arguments: &[&str]| {
            let output = fixture_git(root).args(arguments).output().unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            String::from_utf8(output.stdout).unwrap().trim().to_owned()
        };
        git(&repository, &["init", "-b", "main"]);
        std::fs::write(repository.join("tracked"), "original").unwrap();
        git(&repository, &["add", "tracked"]);
        git(
            &repository,
            &[
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "commit",
                "-m",
                "initial",
            ],
        );
        git(
            &repository,
            &[
                "remote",
                "add",
                "origin",
                "https://github.com/source/repository.git",
            ],
        );
        git(
            &repository,
            &[
                "worktree",
                "add",
                "-b",
                "feature",
                worktree.to_str().unwrap(),
            ],
        );
        git(&repository, &["config", "branch.feature.remote", "origin"]);
        let mut request = request();
        request.repository = repository.clone();
        request.path = worktree.clone();
        request.head_oid = git(&worktree, &["rev-parse", "HEAD"]);
        checked_record(&request).unwrap();
        std::fs::write(repository.join(".git/info/exclude"), ".env\ntarget/\n").unwrap();
        let service = crate::repository::RepositoryService::system("git");
        let cancellation = crate::process::CancellationToken::new();
        let before = service.list_worktrees(&repository, &cancellation).unwrap();
        let admin = service.git_dir(&worktree, &cancellation).unwrap();
        let head_before = std::fs::read(admin.join("HEAD")).unwrap();
        for late_user_file in [true, false] {
            let prepared = PreparedRemoval {
                request: request.clone(),
                target: FencedTarget::capture(&repository, &worktree, Some(request.branch.clone()))
                    .unwrap(),
            };
            let file = if late_user_file {
                worktree.join(".env")
            } else {
                std::fs::create_dir_all(worktree.join("target/nested")).unwrap();
                worktree.join("target/nested/generated")
            };
            std::fs::write(&file, "late external data").unwrap();
            let refusal = prepared
                .remove(|_| panic!("automatic retention must not request destructive admission"))
                .unwrap_err();
            assert!(refusal.contains("automatic cleanup cannot exclude new files"));
            assert!(refusal.contains("Delete worktree and confirm"));
            assert_eq!(std::fs::read(&file).unwrap(), b"late external data");
            assert_eq!(std::fs::read(admin.join("HEAD")).unwrap(), head_before);
            assert_eq!(
                service.list_worktrees(&repository, &cancellation).unwrap(),
                before
            );
            assert_eq!(
                git(&repository, &["rev-parse", "refs/heads/feature"]),
                request.head_oid
            );
            if late_user_file {
                assert!(
                    matches!(PreparedRemoval::prepare(request.clone()), Err(reason) if reason.contains("ignored files .env")),
                    "fresh ignored-data refusal must happen before the GitHub read"
                );
            } else {
                checked_record(&request).unwrap();
            }
            std::fs::remove_file(&file).unwrap();
        }
        std::fs::write(worktree.join("tracked"), "changed").unwrap();
        assert!(checked_record(&request).is_err());
        git(&worktree, &["restore", "tracked"]);
        std::fs::write(worktree.join("untracked"), "preserve").unwrap();
        assert!(checked_record(&request).is_err());
        std::fs::remove_file(worktree.join("untracked")).unwrap();
        git(
            &repository,
            &["worktree", "lock", worktree.to_str().unwrap()],
        );
        assert!(checked_record(&request).is_err());
        git(
            &repository,
            &["worktree", "unlock", worktree.to_str().unwrap()],
        );
        git(
            &worktree,
            &[
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "commit",
                "--allow-empty",
                "-m",
                "new unmerged head",
            ],
        );
        assert!(checked_record(&request).is_err());
        request.path = repository.clone();
        request.branch = "main".into();
        assert!(checked_record(&request).is_err());
        request.path = worktree.clone();
        request.branch = "feature".into();
        request.head_oid = git(&worktree, &["rev-parse", "HEAD"]);
        checked_record(&request).unwrap();

        let admin = PathBuf::from(git(&worktree, &["rev-parse", "--absolute-git-dir"]));
        let common = repository.join(".git");
        let mut head = crate::repository::GitFileLock::acquire(&admin.join("HEAD")).unwrap();
        let branch =
            crate::repository::GitFileLock::acquire(&common.join("refs/heads/feature")).unwrap();
        let packed = crate::repository::GitFileLock::acquire(&common.join("packed-refs")).unwrap();
        let config = crate::repository::GitFileLock::acquire(&common.join("config")).unwrap();
        let checkout = fixture_git(&worktree)
            .args(["checkout", "--detach"])
            .output()
            .unwrap();
        assert!(!checkout.status.success(), "HEAD lock must fence checkout");
        let update = fixture_git(&repository)
            .args(["update-ref", "refs/heads/feature", &request.head_oid])
            .output()
            .unwrap();
        assert!(
            !update.status.success(),
            "branch lock must fence ref writes"
        );
        std::fs::write(worktree.join("late-untracked"), "preserve").unwrap();
        let removal = fixture_git(&repository)
            .args(["worktree", "remove", "--"])
            .arg(&worktree)
            .output()
            .unwrap();
        assert!(!removal.status.success());
        assert_eq!(
            std::fs::read_to_string(worktree.join("late-untracked")).unwrap(),
            "preserve"
        );
        std::fs::remove_file(worktree.join("late-untracked")).unwrap();
        git(
            &repository,
            &["worktree", "remove", "--", worktree.to_str().unwrap()],
        );
        head.retired_with_directory();
        assert!(!worktree.exists());
        assert!(!admin.exists());
        drop((head, branch, packed, config));
        assert_eq!(
            git(&repository, &["rev-parse", "refs/heads/feature"]),
            request.head_oid
        );
        for path in [
            common.join("refs/heads/feature.lock"),
            common.join("packed-refs.lock"),
            common.join("config.lock"),
        ] {
            assert!(!path.exists());
        }
    }

    #[test]
    fn independent_editor_startup_descriptors_protect_files_and_restored_folders_before_readiness()
    {
        let runtime = fixture_directory();
        let worktree = fixture_directory();
        std::fs::write(worktree.path().join(".git"), "gitdir: fixture").unwrap();
        let nested = worktree.path().join("nested");
        std::fs::create_dir(&nested).unwrap();
        let file = nested.join("notes.txt");
        std::fs::write(&file, "preserve").unwrap();
        let guards = initial_editor_protections(runtime.path(), &[file, nested]).unwrap();
        assert_eq!(
            acquire(runtime.path(), worktree.path(), true)
                .unwrap_err()
                .kind(),
            io::ErrorKind::WouldBlock
        );
        drop(guards);
        acquire(runtime.path(), worktree.path(), true).unwrap();
    }

    #[test]
    fn root_operation_protection_uses_path_pins_for_write_search_only_parents() {
        use std::os::{fd::AsRawFd, unix::fs::PermissionsExt};
        let runtime = fixture_directory();
        let tree = fixture_directory();
        let directory = tree.path().join("parent");
        std::fs::create_dir(&directory).unwrap();
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o300)).unwrap();
        let root = crate::fs_safety::Root::open(&directory).unwrap();
        let protection = protect_root_operation_in_runtime(runtime.path(), &root).unwrap();
        assert!(protection.protects_root(&root).unwrap());
        for pin in &protection._directories {
            let flags = unsafe { libc::fcntl(pin.as_raw_fd(), libc::F_GETFL) };
            assert!(flags >= 0);
            assert_ne!(flags & libc::O_PATH, 0);
        }
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).unwrap();
    }

    #[test]
    fn root_operation_protection_holds_the_original_directory() {
        let runtime = fixture_directory();
        let tree = fixture_directory();
        let directory = tree.path().join("checkout");
        std::fs::create_dir(&directory).unwrap();
        let root = crate::fs_safety::Root::open(&directory).unwrap();
        let protection = protect_root_operation_in_runtime(runtime.path(), &root).unwrap();
        assert!(protection.protects_root(&root).unwrap());
        assert!(acquire(runtime.path(), &directory, true).is_err());
        drop(protection);
        assert!(acquire(runtime.path(), &directory, true).is_ok());
    }

    #[test]
    fn root_operation_protection_rejects_replacement_at_the_original_path() {
        let runtime = fixture_directory();
        let tree = fixture_directory();
        let directory = tree.path().join("checkout");
        let moved = tree.path().join("moved");
        std::fs::create_dir(&directory).unwrap();
        let root = crate::fs_safety::Root::open(&directory).unwrap();
        std::fs::rename(&directory, &moved).unwrap();
        std::fs::create_dir(&directory).unwrap();
        assert!(protect_root_operation_in_runtime(runtime.path(), &root).is_err());
        // Rejection must release the replacement's temporary acquisition.
        assert!(acquire(runtime.path(), &directory, true).is_ok());
        assert!(acquire(runtime.path(), &moved, true).is_ok());
    }

    #[test]
    fn directory_identity_protection_survives_git_worktree_moves_and_path_replacement() {
        let fixture = tempfile::tempdir_in("/var/tmp").unwrap();
        let runtime = fixture.path().join("runtime");
        let repository = fixture.path().join("repository");
        let original = fixture.path().join("original");
        let moved = fixture.path().join("moved");
        std::fs::create_dir(&runtime).unwrap();
        std::fs::create_dir(&repository).unwrap();
        let git = |arguments: &[&std::ffi::OsStr]| {
            let output = fixture_git(&repository).args(arguments).output().unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        };
        git(&["init".as_ref(), "-b".as_ref(), "main".as_ref()]);
        git(&[
            "-c".as_ref(),
            "user.name=Fixture".as_ref(),
            "-c".as_ref(),
            "user.email=fixture@example.invalid".as_ref(),
            "commit".as_ref(),
            "--allow-empty".as_ref(),
            "-m".as_ref(),
            "Fixture".as_ref(),
        ]);
        git(&[
            "worktree".as_ref(),
            "add".as_ref(),
            "-b".as_ref(),
            "feature".as_ref(),
            original.as_os_str(),
        ]);
        let editor = acquire(&runtime, &original, false).unwrap();
        git(&[
            "worktree".as_ref(),
            "move".as_ref(),
            original.as_os_str(),
            moved.as_os_str(),
        ]);
        // An editor retaining the old descriptor still owns the moved inode.
        assert!(acquire(&runtime, &moved, true).is_err());
        std::fs::create_dir(&original).unwrap();
        // A replacement at the original pathname is covered by its path lease.
        assert!(acquire(&runtime, &original, true).is_err());
        drop(editor);
        let removal = acquire(&runtime, &moved, true).unwrap();
        assert!(acquire(&runtime, &moved, false).is_err());
        drop(removal);
        assert!(acquire(&runtime, &original, true).is_ok());
    }

    #[test]
    fn sync_retains_moved_directory_inode_lease_until_scope_leaves() {
        let runtime = fixture_directory();
        let fixture = fixture_directory();
        let original = fixture.path().join("original");
        let moved = fixture.path().join("moved");
        let present = fixture.path().join("present");
        std::fs::create_dir(&original).unwrap();
        std::fs::create_dir(&present).unwrap();
        let mut window = WindowProtections::default();
        let now = std::time::Instant::now();
        let sync = |window: &mut WindowProtections, paths: Vec<PathBuf>, now| {
            window.sync_with(
                paths,
                now,
                |path| path.canonicalize(),
                |path| acquire(runtime.path(), path, false),
                || {
                    crate::instance::owned_lock(
                        &runtime.path().join("ready"),
                        libc::LOCK_EX | libc::LOCK_NB,
                    )
                },
            )
        };
        sync(&mut window, vec![original.clone()], now).unwrap();
        std::fs::rename(&original, &moved).unwrap();
        // A changed set forces a refresh while the old requested path is
        // missing. Readiness survives, and the moved inode remains protected.
        sync(&mut window, vec![original.clone(), present.clone()], now).unwrap();
        assert!(window.ready.is_some());
        assert_eq!(
            acquire(runtime.path(), &moved, true).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        // Timer-driven retries retain the same lease as well.
        sync(
            &mut window,
            vec![original, present.clone()],
            now + std::time::Duration::from_secs(31),
        )
        .unwrap();
        assert_eq!(
            acquire(runtime.path(), &moved, true).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        sync(
            &mut window,
            vec![present],
            now + std::time::Duration::from_secs(32),
        )
        .unwrap();
        acquire(runtime.path(), &moved, true).unwrap();
    }

    #[test]
    #[cfg(any(feature = "ui", feature = "host"))]
    fn inherited_shared_protection_outlives_parent_guard_and_directory_replacement() {
        use std::io::{BufRead, Write};
        let runtime = fixture_directory();
        let tree = fixture_directory();
        let directory = tree.path().join("checkout");
        std::fs::create_dir(&directory).unwrap();
        let parent = acquire(runtime.path(), &directory, false).unwrap();
        let mut spec = crate::process::CommandSpec::new("/bin/sh")
            .args(["-c", "printf 'ready\\n'; read -r retired"]);
        for file in parent.retained_child_files().unwrap() {
            spec = spec.inherit_file(file).unwrap();
        }
        let mut command = spec.to_command();
        drop(spec);
        let mut child = command
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        drop(command);
        let mut ready = String::new();
        std::io::BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut ready)
            .unwrap();
        assert_eq!(ready, "ready\n");
        drop(parent); // OwnedLock's explicit unlock must not retire child locks.
        assert!(acquire(runtime.path(), &directory, true).is_err());
        let moved = tree.path().join("moved");
        std::fs::rename(&directory, &moved).unwrap();
        std::fs::create_dir(&directory).unwrap();
        assert!(acquire(runtime.path(), &directory, true).is_err());
        assert!(acquire(runtime.path(), &moved, true).is_err());
        child.stdin.take().unwrap().write_all(b"retire\n").unwrap();
        assert!(child.wait().unwrap().success());
        assert!(acquire(runtime.path(), &directory, true).is_ok());
        assert!(acquire(runtime.path(), &moved, true).is_ok());
    }

    #[test]
    fn child_shared_protection_never_downgrades_an_exclusive_guard() {
        let runtime = fixture_directory();
        let directory = fixture_directory();
        let exclusive = acquire(runtime.path(), directory.path(), true).unwrap();
        assert_eq!(
            exclusive.retained_child_files().unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        assert!(acquire(runtime.path(), directory.path(), false).is_err());
        drop(exclusive);
        assert!(acquire(runtime.path(), directory.path(), false).is_ok());
    }

    #[test]
    fn directory_protection_blocks_removal_until_every_window_releases_it() {
        let runtime = fixture_directory();
        let directory = fixture_directory();
        let first = acquire(runtime.path(), directory.path(), false).unwrap();
        let second = acquire(runtime.path(), directory.path(), false).unwrap();
        assert_eq!(
            acquire(runtime.path(), directory.path(), true)
                .unwrap_err()
                .kind(),
            io::ErrorKind::WouldBlock
        );
        drop(first);
        assert_eq!(
            acquire(runtime.path(), directory.path(), true)
                .unwrap_err()
                .kind(),
            io::ErrorKind::WouldBlock
        );
        drop(second);
        let removal = acquire(runtime.path(), directory.path(), true).unwrap();
        assert_eq!(
            acquire(runtime.path(), directory.path(), false)
                .unwrap_err()
                .kind(),
            io::ErrorKind::WouldBlock
        );
        drop(removal);
        acquire(runtime.path(), directory.path(), false).unwrap();
    }

    #[test]
    fn missing_scope_keeps_other_scopes_ready_without_message_driven_io() {
        let runtime = fixture_directory();
        let present = fixture_directory();
        let missing = present.path().join("missing");
        let paths = vec![present.path().to_owned(), missing.clone()];
        let mut window = WindowProtections::default();
        let now = std::time::Instant::now();
        window
            .sync_with(
                paths.clone(),
                now,
                |path| path.canonicalize(),
                |path| acquire(runtime.path(), path, false),
                || {
                    crate::instance::owned_lock(
                        &runtime.path().join("ready"),
                        libc::LOCK_EX | libc::LOCK_NB,
                    )
                },
            )
            .unwrap();
        assert!(window.ready.is_some());
        assert!(window.missing.contains(&missing));
        assert_eq!(window.directories.len(), 1);
        assert_eq!(
            acquire(runtime.path(), present.path(), true)
                .unwrap_err()
                .kind(),
            io::ErrorKind::WouldBlock
        );
        for _ in 0..100 {
            window
                .sync_with(
                    paths.clone(),
                    now,
                    |_| panic!("unchanged scopes performed filesystem I/O"),
                    |_| panic!("unchanged scopes acquired a lease"),
                    || panic!("unchanged scopes republished readiness"),
                )
                .unwrap();
        }
    }

    #[test]
    fn failed_scope_is_cached_and_logs_only_once_until_its_failure_changes() {
        let paths = vec![PathBuf::from("/scope")];
        let mut window = WindowProtections::default();
        let now = std::time::Instant::now();
        assert!(
            window
                .sync_with(
                    paths.clone(),
                    now,
                    |path| Ok(path.to_owned()),
                    |_| Err(io::Error::other("lease busy")),
                    || panic!("failed scopes cannot be ready"),
                )
                .is_err()
        );
        window
            .sync_with(
                paths.clone(),
                now,
                |_| panic!("cached failure performed I/O"),
                |_| panic!("cached failure acquired a lease"),
                || panic!("failed scopes cannot be ready"),
            )
            .unwrap();
        // The same failure after the retry deadline is not emitted again.
        window
            .sync_with(
                paths,
                now + std::time::Duration::from_secs(31),
                |path| Ok(path.to_owned()),
                |_| Err(io::Error::other("lease busy")),
                || panic!("failed scopes cannot be ready"),
            )
            .unwrap();
    }

    #[test]
    fn changed_scope_set_retries_before_backoff_expires() {
        let mut window = WindowProtections::default();
        let now = std::time::Instant::now();
        let mut attempts = 0;
        for paths in [vec![PathBuf::from("/one")], vec![PathBuf::from("/two")]] {
            assert!(
                window
                    .sync_with(
                        paths,
                        now,
                        |path| Ok(path.to_owned()),
                        |_| {
                            attempts += 1;
                            Err(io::Error::other("lease busy"))
                        },
                        || panic!("failed scopes cannot be ready"),
                    )
                    .is_err()
            );
        }
        assert_eq!(attempts, 2);
    }

    #[test]
    fn failed_scope_retries_after_backoff_and_restores_readiness() {
        let runtime = fixture_directory();
        let directory = fixture_directory();
        let paths = vec![directory.path().to_owned()];
        let mut window = WindowProtections::default();
        let now = std::time::Instant::now();
        assert!(
            window
                .sync_with(
                    paths.clone(),
                    now,
                    |path| path.canonicalize(),
                    |_| Err(io::Error::other("lease busy")),
                    || panic!("failed scopes cannot be ready"),
                )
                .is_err()
        );
        window
            .sync_with(
                paths,
                now + std::time::Duration::from_secs(31),
                |path| path.canonicalize(),
                |path| acquire(runtime.path(), path, false),
                || {
                    crate::instance::owned_lock(
                        &runtime.path().join("ready"),
                        libc::LOCK_EX | libc::LOCK_NB,
                    )
                },
            )
            .unwrap();
        assert!(window.ready.is_some());
        assert!(window.last_failure.is_none());
        assert!(window.retry_after.is_none());
    }

    #[test]
    fn device_switch_revokes_readiness_without_exposing_retained_local_editors() {
        let runtime = fixture_directory();
        let directory = fixture_directory();
        let guard = acquire(runtime.path(), directory.path(), false).unwrap();
        let path = guard.directory.clone();
        let marker = runtime.path().join("ready");
        let mut window = WindowProtections {
            directories: [(path.clone(), guard)].into(),
            requested: [path].into(),
            ready: Some(
                crate::instance::owned_lock(&marker, libc::LOCK_EX | libc::LOCK_NB).unwrap(),
            ),
            ..Default::default()
        };
        window.suspend_readiness();
        assert!(window.ready.is_none());
        assert!(window.requested.is_empty());
        assert_eq!(
            acquire(runtime.path(), directory.path(), true)
                .unwrap_err()
                .kind(),
            io::ErrorKind::WouldBlock
        );
        let ready = crate::instance::owned_lock(&marker, libc::LOCK_EX | libc::LOCK_NB).unwrap();
        drop((ready, window));
        acquire(runtime.path(), directory.path(), true).unwrap();
    }

    #[test]
    fn nested_editors_protect_containing_worktrees() {
        let runtime = fixture_directory();
        let directory = fixture_directory();
        std::fs::write(directory.path().join(".git"), "gitdir: ignored").unwrap();
        let nested = directory.path().join("nested/folder");
        std::fs::create_dir_all(&nested).unwrap();
        let editor = acquire(runtime.path(), &nested, false).unwrap();
        assert_eq!(
            acquire(runtime.path(), directory.path(), true)
                .unwrap_err()
                .kind(),
            io::ErrorKind::WouldBlock
        );
        drop(editor);
        acquire(runtime.path(), directory.path(), true).unwrap();
    }

    #[test]
    fn canonical_aliases_share_one_stable_protection_file() {
        let runtime = fixture_directory();
        let directory = fixture_directory();
        let first = acquire(runtime.path(), directory.path(), false).unwrap();
        assert_eq!(
            acquire(runtime.path(), &directory.path().join("."), true)
                .unwrap_err()
                .kind(),
            io::ErrorKind::WouldBlock
        );
        let before = lock_path(runtime.path(), &directory.path().canonicalize().unwrap()).unwrap();
        let inode = std::os::unix::fs::MetadataExt::ino(&before.metadata().unwrap());
        drop(first);
        drop(acquire(runtime.path(), directory.path(), true).unwrap());
        assert_eq!(
            inode,
            std::os::unix::fs::MetadataExt::ino(&before.metadata().unwrap())
        );
    }
}
