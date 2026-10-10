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
    common_dir: PathBuf,
    admin_dir: PathBuf,
    directory_identity: (u64, u64),
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
        .status(&request.path, &cancellation)
        .map_err(|error| error.to_string())?;
    if status.untracked_omitted || status.dirty.is_dirty() || !status.entries.is_empty() {
        return Err("Worktree has changes or untracked files".into());
    }
    Ok(record.clone())
}

#[cfg(feature = "host")]
impl PreparedRemoval {
    /// GitHub is read afresh before taking exclusive lifecycle fences. Sidebar
    /// branch badges and cached archived PRs never authorize removal.
    pub(crate) fn prepare(request: RemovalRequest) -> Result<Self, String> {
        use std::os::unix::fs::MetadataExt;
        checked_record(&request)?;
        let cancellation = crate::process::CancellationToken::new();
        let service = crate::repository::RepositoryService::system("git");
        // Files-backend locks do not fence a reftable repository. Preserve
        // unsupported ref storage rather than claiming the same guarantee.
        use crate::process::ProcessExecutor;
        let command = crate::process::CommandSpec::new("git")
            .cwd(&request.repository)
            .args(["config", "--get", "extensions.refStorage"]);
        let output = crate::process::SystemProcessExecutor
            .execute(&command, &cancellation)
            .map_err(|error| error.to_string())?;
        if (!output.stdout.is_empty() && output.stdout.as_slice() != b"files\n")
            || (output.exit_code != Some(0) && output.exit_code != Some(1))
        {
            return Err("Repository reference storage is unsupported for automatic cleanup".into());
        }
        let common_dir = service
            .common_dir(&request.repository, &cancellation)
            .map_err(|error| error.to_string())?;
        let admin_dir = service
            .git_dir(&request.path, &cancellation)
            .map_err(|error| error.to_string())?;
        let metadata = request.path.metadata().map_err(|error| error.to_string())?;
        let pr = crate::github::GitHubService::system("gh")
            .view(&request.pull_request, &cancellation)
            .map_err(|error| error.to_string())?;
        if request.branch == pr.base_ref_name || !exact_merged_head(&request, &pr) {
            return Err("The exact current worktree commit is not confirmed merged".into());
        }
        Ok(Self {
            request,
            common_dir,
            admin_dir,
            directory_identity: (metadata.dev(), metadata.ino()),
        })
    }

    /// Caller retains the daemon's exclusive admission gate through this call.
    pub(crate) fn remove(
        self,
        check_quiescence: impl FnOnce(&[crate::instance::VerifiedWindow]) -> Result<(), String>,
    ) -> Result<crate::repository::RemovedWorktree, String> {
        use crate::process::ProcessExecutor;
        use std::os::unix::fs::MetadataExt;
        let _launch =
            crate::instance::worktree_mutation_launch_fence().map_err(|error| error.to_string())?;
        let windows =
            crate::instance::verified_running_windows().map_err(|error| error.to_string())?;
        if windows.is_empty()
            || windows
                .iter()
                .any(|window| !window.protects_worktrees().unwrap_or(false))
        {
            return Err("Window directory protection is not ready".into());
        }
        // Busy applications skip before the independent-editor process census.
        // Admission remains fenced while directory leases are then acquired.
        check_quiescence(&windows)?;
        let _editors =
            crate::instance::verified_running_editors().map_err(|error| error.to_string())?;
        let _directory = acquire(
            &crate::instance::runtime_directory().map_err(|error| error.to_string())?,
            &self.request.path,
            true,
        )
        .map_err(|_| "Worktree is selected or open in an editor".to_owned())?;
        // Git's own metadata locks prevent checkout, commit, and packed-ref
        // maintenance from changing the confirmed head during removal.
        let mut head = crate::repository::GitFileLock::acquire(&self.admin_dir.join("HEAD"))?;
        let branch = crate::repository::GitFileLock::acquire(
            &self
                .common_dir
                .join("refs/heads")
                .join(&self.request.branch),
        )?;
        let packed = crate::repository::GitFileLock::acquire(&self.common_dir.join("packed-refs"))?;
        let config = crate::repository::GitFileLock::acquire(&self.common_dir.join("config"))?;
        head.check()?;
        branch.check()?;
        packed.check()?;
        config.check()?;
        checked_record(&self.request)?;
        let metadata = self
            .request
            .path
            .metadata()
            .map_err(|error| error.to_string())?;
        if (metadata.dev(), metadata.ino()) != self.directory_identity {
            return Err("Worktree directory was replaced".into());
        }
        let cancellation = crate::process::CancellationToken::new();
        let service = crate::repository::RepositoryService::system("git");
        if service
            .common_dir(&self.request.repository, &cancellation)
            .map_err(|error| error.to_string())?
            != self.common_dir
            || service
                .git_dir(&self.request.path, &cancellation)
                .map_err(|error| error.to_string())?
                != self.admin_dir
        {
            return Err("Worktree administrative identity changed".into());
        }
        let command = crate::process::CommandSpec::new("git")
            .cwd(&self.request.repository)
            .args(["worktree", "remove", "--"])
            .arg(self.request.path.as_os_str())
            .timeout(std::time::Duration::from_secs(30))
            .output_limits(64 * 1024, 64 * 1024);
        let result = crate::process::SystemProcessExecutor
            .execute(&command, &cancellation)
            .and_then(|output| output.require_success(&command.program));
        if let Err(error) = result {
            // A timeout does not certify removal. Preserve the UI row unless
            // both Git's registration and the exact directory are gone.
            let records = service
                .list_worktrees(&self.request.repository, &cancellation)
                .map_err(|_| error.reported_detail())?;
            if self.request.path.exists()
                || records
                    .iter()
                    .any(|record| record.path == self.request.path)
            {
                return Err(error.reported_detail());
            }
        }
        head.retired_with_directory();
        Ok(crate::repository::RemovedWorktree {
            common_dir: self.common_dir,
            admin_dir: self.admin_dir,
            path: self.request.path,
            branch: Some(format!("refs/heads/{}", self.request.branch)),
        })
    }
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
    use std::os::unix::{fs::MetadataExt, fs::OpenOptionsExt};
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
        let held_directory = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&root)?;
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

/// Selected scopes and restored editor descriptors remain protected even
/// when their actual widgets are parked or have not mounted yet.
#[derive(Default)]
pub(crate) struct WindowProtections {
    directories: BTreeMap<PathBuf, Protection>,
    requested: std::collections::BTreeSet<PathBuf>,
    ready: Option<crate::instance::OwnedLock>,
}

impl WindowProtections {
    fn suspend_readiness(&mut self) {
        self.ready = None;
        self.requested.clear();
    }

    pub(crate) fn protect_next(&mut self, directory: &Path) -> io::Result<()> {
        if let Some(protection) = protect(directory)? {
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
        let requested = directories
            .into_iter()
            .collect::<std::collections::BTreeSet<_>>();
        if self.ready.is_some() && requested == self.requested {
            return Ok(());
        }
        // Install every replacement before retiring any earlier protection.
        // A failed acquisition removes readiness, so cleanup fails closed.
        let result = (|| {
            let mut needed = std::collections::BTreeSet::new();
            for directory in &requested {
                let directory = directory.canonicalize()?;
                if !self.directories.contains_key(&directory) {
                    let guard = protect(&directory)?
                        .ok_or_else(|| io::Error::other("window protection unavailable"))?;
                    self.directories.insert(directory.clone(), guard);
                }
                needed.insert(directory);
            }
            self.directories
                .retain(|directory, _| needed.contains(directory));
            if self.ready.is_none() {
                self.ready = Some(crate::instance::window_protection_ready()?);
            }
            self.requested = requested;
            Ok(())
        })();
        if result.is_err() {
            self.ready = None;
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let runtime = tempfile::tempdir().unwrap();
        let worktree = tempfile::tempdir().unwrap();
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
    fn directory_protection_blocks_removal_until_every_window_releases_it() {
        let runtime = tempfile::tempdir().unwrap();
        let directory = tempfile::tempdir().unwrap();
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
    fn device_switch_revokes_readiness_without_exposing_retained_local_editors() {
        let runtime = tempfile::tempdir().unwrap();
        let directory = tempfile::tempdir().unwrap();
        let guard = acquire(runtime.path(), directory.path(), false).unwrap();
        let path = guard.directory.clone();
        let marker = runtime.path().join("ready");
        let mut window = WindowProtections {
            directories: [(path.clone(), guard)].into(),
            requested: [path].into(),
            ready: Some(
                crate::instance::owned_lock(&marker, libc::LOCK_EX | libc::LOCK_NB).unwrap(),
            ),
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
        let runtime = tempfile::tempdir().unwrap();
        let directory = tempfile::tempdir().unwrap();
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
        let runtime = tempfile::tempdir().unwrap();
        let directory = tempfile::tempdir().unwrap();
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
