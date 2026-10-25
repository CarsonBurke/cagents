//! Explicit user checkout observations. No actor launch scope or execution permit.
use crate::{
    fs_safety::{self, Root},
    repository::{
        CancellationToken, CommandSpec, ProcessExecutor, ProcessFailure, ProcessOutput,
        RepositoryService, SystemProcessExecutor, ValidatedBranchName,
    },
};
use std::{
    ffi::{OsStr, OsString},
    fs::{self, File},
    io::Read,
    os::{
        fd::AsRawFd,
        unix::{
            ffi::OsStrExt,
            fs::{MetadataExt, OpenOptionsExt},
        },
    },
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

const METADATA_BYTES: u64 = 8192;
const READ_TIME: Duration = Duration::from_secs(5);
const CHANGED: &str = "The selected checkout changed; select it again";
const UNAVAILABLE: &str = "The selected checkout could not be read safely";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CheckoutFacts {
    pub repository: PathBuf,
    pub path: PathBuf,
    pub branch: Option<String>,
    pub head: Option<String>,
    /// Metadata-only capture never runs worktree conversion. None is not clean.
    pub dirty: Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct DirectoryIdentity {
    device: u64,
    inode: u64,
}
impl DirectoryIdentity {
    fn of(metadata: &fs::Metadata) -> Self {
        Self {
            device: metadata.dev(),
            inode: metadata.ino(),
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct FileStamp {
    identity: DirectoryIdentity,
    len: u64,
    modified: (i64, i64),
    changed: (i64, i64),
}
impl FileStamp {
    fn of(metadata: &fs::Metadata) -> Self {
        Self {
            identity: DirectoryIdentity::of(metadata),
            len: metadata.len(),
            modified: (metadata.mtime(), metadata.mtime_nsec()),
            changed: (metadata.ctime(), metadata.ctime_nsec()),
        }
    }
}

/// A branch interval, not a stable path-derived repository/author identity.
/// Keeping the original HEAD file open prevents its inode being reused while cached.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct CheckoutIdentity {
    worktree: DirectoryIdentity,
    admin: DirectoryIdentity,
    common: DirectoryIdentity,
    head_binding: FileStamp,
    head_bytes: Vec<u8>,
    repository: PathBuf,
    path: PathBuf,
    branch: Option<String>,
}

struct NamedPin {
    root: Root,
    original: PathBuf,
    canonical: PathBuf,
    identity: DirectoryIdentity,
}
impl NamedPin {
    fn open(path: &Path) -> Result<Self, String> {
        if !path.is_absolute() {
            return Err(UNAVAILABLE.into());
        }
        let opened = Root::open(path).map_err(|_| UNAVAILABLE)?;
        let identity = DirectoryIdentity::of(&opened.metadata().map_err(|_| UNAVAILABLE)?);
        let canonical = fs::canonicalize(path).map_err(|_| UNAVAILABLE)?;
        let root = Root::open(&canonical).map_err(|_| UNAVAILABLE)?;
        if DirectoryIdentity::of(&root.metadata().map_err(|_| UNAVAILABLE)?) != identity {
            return Err(CHANGED.into());
        }
        let pin = Self {
            root,
            original: path.into(),
            canonical,
            identity,
        };
        pin.check()?;
        Ok(pin)
    }
    fn check(&self) -> Result<(), String> {
        for name in [&self.original, &self.canonical] {
            let named = Root::open(name).map_err(|_| CHANGED)?;
            if DirectoryIdentity::of(&named.metadata().map_err(|_| CHANGED)?) != self.identity
                || fs::canonicalize(name).map_err(|_| CHANGED)? != self.canonical
            {
                return Err(CHANGED.into());
            }
        }
        if DirectoryIdentity::of(&self.root.metadata().map_err(|_| CHANGED)?) != self.identity {
            return Err(CHANGED.into());
        }
        Ok(())
    }
}

struct MetadataWitness {
    name: &'static str,
    bytes: Vec<u8>,
    stamp: FileStamp,
    file: File,
}
impl MetadataWitness {
    fn read(root: &Root, name: &'static str) -> Result<Self, String> {
        // NONBLOCK precedes fstat: a replaced metadata leaf must not hang on a FIFO.
        let mut file = fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(root.process_path().join(name))
            .map_err(|_| UNAVAILABLE)?;
        let before = file.metadata().map_err(|_| UNAVAILABLE)?;
        if !before.is_file() || before.len() > METADATA_BYTES {
            return Err(UNAVAILABLE.into());
        }
        let mut bytes = Vec::new();
        (&mut file)
            .take(METADATA_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| UNAVAILABLE)?;
        let after = file.metadata().map_err(|_| UNAVAILABLE)?;
        let named = fs_safety::metadata_beneath(root, Path::new(name)).map_err(|_| CHANGED)?;
        let stamp = FileStamp::of(&before);
        if bytes.len() > METADATA_BYTES as usize
            || bytes.contains(&0)
            || stamp != FileStamp::of(&after)
            || stamp != FileStamp::of(&named)
        {
            return Err(CHANGED.into());
        }
        Ok(Self {
            name,
            bytes,
            stamp,
            file,
        })
    }
    fn check(&self, root: &Root) -> Result<(), String> {
        let now = Self::read(root, self.name)?;
        if now.stamp != self.stamp
            || now.bytes != self.bytes
            || FileStamp::of(&self.file.metadata().map_err(|_| CHANGED)?) != self.stamp
        {
            return Err(CHANGED.into());
        }
        Ok(())
    }
    fn path(&self, base: &Path, prefix: &[u8]) -> Result<PathBuf, String> {
        let line = self.bytes.strip_suffix(b"\n").unwrap_or(&self.bytes);
        let path = line.strip_prefix(prefix).ok_or(UNAVAILABLE)?;
        if path.is_empty() || path.contains(&b'\n') || path.contains(&b'\r') {
            return Err(UNAVAILABLE.into());
        }
        let path = Path::new(OsStr::from_bytes(path));
        Ok(if path.is_absolute() {
            path.into()
        } else {
            base.join(path)
        })
    }
}

/// RepositoryService supplies its normal shell-free, fsmonitor-disabled reads.
/// This purpose-specific executor further bounds them and forbids hooks/lazy fetch.
struct ReadExecutor;
impl ProcessExecutor for ReadExecutor {
    fn execute(
        &self,
        command: &CommandSpec,
        cancel: &CancellationToken,
    ) -> Result<ProcessOutput, ProcessFailure> {
        let mut command = command
            .clone()
            .timeout(READ_TIME)
            .output_limits(8192, 8192)
            .env("GIT_NO_LAZY_FETCH", "1")
            .env("GIT_NO_REPLACE_OBJECTS", "1");
        command.arguments.splice(
            0..0,
            [
                OsString::from("-c"),
                OsString::from("core.hooksPath=/dev/null"),
            ],
        );
        SystemProcessExecutor.execute(&command, cancel)
    }
}
fn service() -> RepositoryService<ReadExecutor> {
    RepositoryService::new("git", Arc::new(ReadExecutor))
}
fn not_cancelled(cancel: &CancellationToken) -> Result<(), String> {
    if cancel.is_cancelled() {
        Err("Checkout capture was cancelled".into())
    } else {
        Ok(())
    }
}
fn optional_metadata(root: &Root, name: &'static str) -> Result<Option<MetadataWitness>, String> {
    match fs_safety::metadata_beneath(root, Path::new(name)) {
        Err(error) if error.is_not_found() => Ok(None),
        Ok(_) => MetadataWitness::read(root, name).map(Some),
        Err(_) => Err(UNAVAILABLE.into()),
    }
}

pub(crate) struct CapturedCheckout {
    project: NamedPin,
    tree: NamedPin,
    admin: NamedPin,
    common: NamedPin,
    locator: Option<MetadataWitness>,
    commondir: Option<MetadataWitness>,
    backlink: Option<MetadataWitness>,
    head: MetadataWitness,
    facts: CheckoutFacts,
    identity: CheckoutIdentity,
    // Process-local observation order, not wire identity or persisted provenance.
    observed_at: Instant,
}
impl CapturedCheckout {
    pub(crate) fn capture_user(
        repository: &Path,
        selected_worktree: &Path,
        cancel: &CancellationToken,
    ) -> Result<Self, String> {
        not_cancelled(cancel)?;
        let project = NamedPin::open(repository)?;
        let tree = NamedPin::open(selected_worktree)?;
        let marker =
            fs_safety::metadata_beneath(&tree.root, Path::new(".git")).map_err(|_| UNAVAILABLE)?;
        let locator = if marker.is_file() {
            Some(MetadataWitness::read(&tree.root, ".git")?)
        } else if marker.is_dir() {
            None
        } else {
            return Err(UNAVAILABLE.into());
        };
        let admin_path = match &locator {
            Some(locator) => locator.path(&tree.canonical, b"gitdir: ")?,
            None => tree.canonical.join(".git"),
        };
        let admin = NamedPin::open(&admin_path)?;
        let commondir = optional_metadata(&admin.root, "commondir")?;
        let common_path = match &commondir {
            Some(witness) => witness.path(&admin.canonical, b"")?,
            None => admin.canonical.clone(),
        };
        let common = NamedPin::open(&common_path)?;
        // Validate HEAD's regular/no-follow bound before Git can open it.
        let head = MetadataWitness::read(&admin.root, "HEAD")?;
        let project_common = service()
            .common_dir(&project.root.process_path(), cancel)
            .map_err(|_| UNAVAILABLE)?;
        let expected = NamedPin::open(&project_common)?;
        if expected.identity != common.identity {
            return Err("The selected worktree belongs to another repository".into());
        }
        let backlink = if admin.identity != common.identity {
            let backlink = MetadataWitness::read(&admin.root, "gitdir")?;
            if locator.is_none()
                || fs::canonicalize(backlink.path(&admin.canonical, b"")?)
                    .map_err(|_| UNAVAILABLE)?
                    != tree.canonical.join(".git")
            {
                return Err(CHANGED.into());
            }
            Some(backlink)
        } else {
            None
        };
        // A directory inside a worktree or a configured alternate worktree is
        // not the explicit root selected by the user. Do not make it one by
        // supplying --work-tree to the later pinned metadata commands.
        let discovered = service()
            .discover(&tree.root.process_path(), cancel)
            .map_err(|_| UNAVAILABLE)?;
        let discovered_root = discovered.worktree_root.ok_or(UNAVAILABLE)?;
        if discovered.is_bare
            || fs::canonicalize(discovered_root).map_err(|_| UNAVAILABLE)? != tree.canonical
            || NamedPin::open(&discovered.git_dir)?.identity != admin.identity
            || NamedPin::open(&discovered.common_dir)?.identity != common.identity
        {
            return Err("Select an actual worktree root in this repository".into());
        }
        let identity = CheckoutIdentity {
            worktree: tree.identity,
            admin: admin.identity,
            common: common.identity,
            head_binding: head.stamp,
            head_bytes: head.bytes.clone(),
            repository: common.canonical.clone(),
            path: tree.canonical.clone(),
            branch: None,
        };
        let facts = CheckoutFacts {
            repository: common.canonical.clone(),
            path: tree.canonical.clone(),
            branch: None,
            head: None,
            dirty: None,
        };
        let observed_at = Instant::now();
        let mut captured = Self {
            project,
            tree,
            admin,
            common,
            locator,
            commondir,
            backlink,
            head,
            facts,
            identity,
            observed_at,
        };
        captured.check_bindings(cancel)?;
        let facts = captured.read_metadata(cancel)?;
        // Record immediately: a slow binding check must not make old facts
        // appear newer than another checkout observation that completed later.
        let observed_at = Instant::now();
        captured.check_bindings(cancel)?;
        captured.observed_at = observed_at;
        captured.identity.branch = facts.branch.clone();
        captured.facts = facts;
        Ok(captured)
    }
    pub(crate) fn facts(&self) -> CheckoutFacts {
        self.facts.clone()
    }
    pub(crate) fn identity(&self) -> CheckoutIdentity {
        self.identity.clone()
    }
    pub(crate) fn observed_at(&self) -> Instant {
        self.observed_at
    }
    pub(crate) fn revalidate(&self, cancel: &CancellationToken) -> Result<CheckoutFacts, String> {
        self.check_bindings(cancel)?;
        let facts = self.read_metadata(cancel)?;
        self.check_bindings(cancel)?;
        if facts.branch != self.identity.branch {
            return Err(CHANGED.into());
        }
        Ok(facts)
    }
    fn check_bindings(&self, cancel: &CancellationToken) -> Result<(), String> {
        not_cancelled(cancel)?;
        for pin in [&self.project, &self.tree, &self.admin, &self.common] {
            pin.check()?;
        }
        match &self.locator {
            Some(witness) => witness.check(&self.tree.root)?,
            None => {
                let marker = fs_safety::open_directory_beneath(&self.tree.root, Path::new(".git"))
                    .map_err(|_| CHANGED)?;
                if DirectoryIdentity::of(&marker.metadata().map_err(|_| CHANGED)?)
                    != self.admin.identity
                {
                    return Err(CHANGED.into());
                }
            }
        }
        for (root, name, witness) in [
            (&self.admin.root, "commondir", &self.commondir),
            (&self.admin.root, "gitdir", &self.backlink),
        ] {
            match witness {
                Some(witness) => witness.check(root)?,
                None if name == "gitdir" && self.admin.identity == self.common.identity => {}
                None => {
                    if optional_metadata(root, name)?.is_some() {
                        return Err(CHANGED.into());
                    }
                }
            }
        }
        self.head.check(&self.admin.root)?;
        let current_common = service()
            .common_dir(&self.project.root.process_path(), cancel)
            .map_err(|_| UNAVAILABLE)?;
        if NamedPin::open(&current_common)?.identity != self.common.identity {
            return Err(CHANGED.into());
        }
        not_cancelled(cancel)
    }
    /// Fixed ref/object builtins only. No status/diff/attributes/conversion is
    /// invoked, so repository clean/process drivers cannot run through this read.
    fn pinned_read(
        &self,
        arguments: &[&str],
        cancel: &CancellationToken,
    ) -> Result<ProcessOutput, String> {
        let files = [&self.tree, &self.admin, &self.common]
            .map(|pin| pin.root.retained_directory().map(Arc::new))
            .into_iter()
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| UNAVAILABLE)?;
        let path =
            |index: usize| PathBuf::from(format!("/proc/self/fd/{}", files[index].as_raw_fd()));
        let mut command = service()
            .git_base(&path(0), false)
            .args(["--no-optional-locks"])
            .args(arguments.iter().copied())
            .env("GIT_OPTIONAL_LOCKS", "0");
        command.arguments.splice(
            0..0,
            [
                OsString::from(format!("--git-dir={}", path(1).display())),
                OsString::from(format!("--work-tree={}", path(0).display())),
            ],
        );
        command = command.env("GIT_COMMON_DIR", path(2));
        for file in files {
            command = command.inherit_file(file).map_err(|_| UNAVAILABLE)?;
        }
        ReadExecutor
            .execute(&command, cancel)
            .map_err(|_| UNAVAILABLE.into())
    }

    fn selected_branch(
        &self,
        cancel: &CancellationToken,
    ) -> Result<Option<ValidatedBranchName>, String> {
        // Report the direct HEAD-selected branch, not a guessed author or an
        // indirect ref-chain owner. The retained original HEAD binding fences ABA.
        let output =
            self.pinned_read(&["symbolic-ref", "--quiet", "--no-recurse", "HEAD"], cancel)?;
        if output.success {
            let line = metadata_line(&output.stdout)?;
            let branch = line.strip_prefix("refs/heads/").ok_or(UNAVAILABLE)?;
            if branch.len() > 1024 {
                return Err(UNAVAILABLE.into());
            }
            ValidatedBranchName::parse(branch)
                .map(Some)
                .map_err(|_| UNAVAILABLE.into())
        } else if output.exit_code == Some(1)
            && output.signal.is_none()
            && output.stdout.is_empty()
            && output.stderr.is_empty()
        {
            Ok(None) // Git's explicit quiet non-symbolic-HEAD outcome.
        } else {
            Err(UNAVAILABLE.into())
        }
    }

    fn read_metadata(&self, cancel: &CancellationToken) -> Result<CheckoutFacts, String> {
        let branch = self.selected_branch(cancel)?;
        let commit = self.pinned_read(
            &["rev-parse", "--verify", "--quiet", "HEAD^{commit}"],
            cancel,
        )?;
        let head = if commit.success {
            let oid = metadata_line(&commit.stdout)?;
            if ![40, 64].contains(&oid.len()) || !oid.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                return Err(UNAVAILABLE.into());
            }
            Some(oid.to_ascii_lowercase())
        } else {
            // Quiet verification's ordinary unresolved-revision outcome alone
            // can be checked for unborn. Signals, diagnostics, and other errors
            // must not be converted into a missing commit observation.
            if commit.exit_code != Some(1)
                || commit.signal.is_some()
                || !commit.stdout.is_empty()
                || !commit.stderr.is_empty()
            {
                return Err(UNAVAILABLE.into());
            }
            let branch = branch.as_ref().ok_or(UNAVAILABLE)?;
            // --exists distinguishes absent(2) from lookup/error(1). Older Git
            // without it refuses; a missing object or broken ref is not unborn.
            let exists = self.pinned_read(&["show-ref", "--exists", &branch.full_ref()], cancel)?;
            if exists.exit_code != Some(2) || exists.signal.is_some() || !exists.stdout.is_empty() {
                return Err(UNAVAILABLE.into());
            }
            None
        };
        let final_branch = self.selected_branch(cancel)?;
        if final_branch != branch {
            return Err(CHANGED.into());
        }
        if [&self.common.canonical, &self.tree.canonical]
            .iter()
            .any(|path| {
                path.as_os_str().as_encoded_bytes().len()
                    > crate::channel_work::MAX_CONTEXT_PATH_BYTES
            })
        {
            return Err(UNAVAILABLE.into());
        }
        Ok(CheckoutFacts {
            repository: self.common.canonical.clone(),
            path: self.tree.canonical.clone(),
            branch: branch.map(|branch| branch.as_str().to_owned()),
            head,
            dirty: None,
        })
    }
}

fn metadata_line(bytes: &[u8]) -> Result<&str, String> {
    let line = bytes.strip_suffix(b"\n").unwrap_or(bytes);
    let line = std::str::from_utf8(line).map_err(|_| UNAVAILABLE)?;
    if line.is_empty() || line.len() > METADATA_BYTES as usize || line.chars().any(char::is_control)
    {
        return Err(UNAVAILABLE.into());
    }
    Ok(line)
}

#[cfg(test)]
mod tests;
