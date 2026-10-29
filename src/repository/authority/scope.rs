//! A delayed UI intent owns the physical worktree it originally displayed.

use super::*;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};

#[derive(Clone)]
pub struct WorktreeAuthority {
    pub(super) root: Arc<fs_safety::Root>,
    pub(super) identity: (u64, u64),
    git_identity: (u64, u64),
    common_identity: (u64, u64),
    pub(super) git_dir: Arc<fs_safety::Root>,
    pub(super) common_dir: Arc<fs_safety::Root>,
    pub(super) child_root: Arc<fs::File>,
    pub(super) child_git: Arc<fs::File>,
    pub(super) child_common: Arc<fs::File>,
}

impl fmt::Debug for WorktreeAuthority {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WorktreeAuthority")
            .field("path", &self.path())
            .field("identity", &self.identity)
            .finish()
    }
}

impl WorktreeAuthority {
    /// Strong Git mutation authority currently uses Linux descriptor
    /// namespaces with owned directory handles inherited by Git. Basic Git reads and ordinary file editing do not
    /// require this capability and must not substitute an unpinned mutation.
    pub const fn supported() -> bool {
        cfg!(target_os = "linux")
    }

    pub const fn unsupported_reason() -> Option<&'static str> {
        if Self::supported() {
            None
        } else {
            Some(
                "Git mutations are currently supported on Linux only. You can still edit files and inspect Git changes.",
            )
        }
    }

    pub fn path(&self) -> &Path {
        self.root.path()
    }

    /// Compare the originally held scopes without touching the filesystem.
    /// Workers must still call `validate` before performing IO.
    pub fn same_scope(&self, other: &Self) -> bool {
        self.identity == other.identity
            && self.git_identity == other.git_identity
            && self.common_identity == other.common_identity
            && self.path() == other.path()
            && self.git_dir.path() == other.git_dir.path()
            && self.common_dir.path() == other.common_dir.path()
    }

    /// Pure clone of the original held worktree for negative root enrollment.
    /// This performs no IO and grants no new source/adoption/effect authority.
    pub fn retained_worktree_root(&self) -> Arc<fs_safety::Root> {
        Arc::clone(&self.root)
    }

    pub fn validate(&self) -> Result<(), RepositoryError> {
        if let Some(reason) = Self::unsupported_reason() {
            return Err(ValidationError::Io(reason.into()).into());
        }
        let named = fs_safety::Root::open(self.path()).map_err(|error| {
            ValidationError::Io(format!(
                "The original worktree is no longer available: {error}"
            ))
        })?;
        let metadata = named
            .metadata()
            .map_err(|error| ValidationError::Io(error.to_string()))?;
        if (metadata.dev(), metadata.ino()) != self.identity {
            return Err(ValidationError::Io("The original worktree directory was replaced; no action was applied to the replacement".into()).into());
        }
        // Each completed identity observation can release its temporary handle.
        // Original authority remains held by self throughout all checks.
        drop(named);
        let named_private = open_metadata_root(self.git_dir.path())?;
        let named_common = open_metadata_root(self.common_dir.path())?;
        if !same_directory(&named_private, &self.git_dir)?
            || !same_directory(&named_common, &self.common_dir)?
        {
            return Err(ValidationError::Io("The original Git metadata directory name was replaced; reopen the worktree before retrying".into()).into());
        }
        drop(named_private);
        drop(named_common);
        let private = open_metadata_root(&resolve_private_git_dir(&self.anchored_path())?)?;
        let common = open_metadata_root(&resolve_common_git_dir(&descriptor_path(&self.git_dir))?)?;
        if !same_directory(&private, &self.git_dir)? || !same_directory(&common, &self.common_dir)?
        {
            return Err(ValidationError::Io("The original Git metadata was replaced or redirected; no action was applied to the replacement".into()).into());
        }
        Ok(())
    }

    /// Journal code can retain the exact source roots without rediscovering
    /// metadata through a changed path. No executor is exposed here.
    pub(in crate::repository) fn retained_roots(
        &self,
    ) -> Result<(fs_safety::Root, fs_safety::Root, fs_safety::Root), RepositoryError> {
        self.validate()?;
        let clone_root = |root: &fs_safety::Root| -> Result<fs_safety::Root, RepositoryError> {
            root.try_clone()
                .map_err(|error| ValidationError::Io(error.to_string()).into())
        };
        Ok((
            clone_root(&self.root)?,
            clone_root(&self.git_dir)?,
            clone_root(&self.common_dir)?,
        ))
    }

    /// A creation journal first verifies its owned registration, backlinks and
    /// publication; this constructor additionally verifies the held roots still
    /// match the worktree's named metadata before publishing destination scope.
    // Also used by the authenticated created-start receiver AFTER its durable
    // required-row/operation/audience proof and six descriptor-role checks.
    // This does not accept a raw descriptor or serialized path report.
    pub(crate) fn from_retained_roots(
        root: fs_safety::Root,
        git_dir: fs_safety::Root,
        common_dir: fs_safety::Root,
    ) -> Result<Self, RepositoryError> {
        let metadata = root
            .metadata()
            .map_err(|error| ValidationError::Io(error.to_string()))?;
        let git_metadata = git_dir.metadata().map_err(metadata_error)?;
        let common_metadata = common_dir.metadata().map_err(metadata_error)?;
        let (_, child_root) = inherited_directory_scope(&root)?;
        let (_, child_git) = inherited_directory_scope(&git_dir)?;
        let child_common = if same_directory(&git_dir, &common_dir)? {
            child_git.clone()
        } else {
            inherited_directory_scope(&common_dir)?.1
        };
        let authority = Self {
            root: Arc::new(root),
            identity: (metadata.dev(), metadata.ino()),
            git_identity: (git_metadata.dev(), git_metadata.ino()),
            common_identity: (common_metadata.dev(), common_metadata.ino()),
            git_dir: Arc::new(git_dir),
            common_dir: Arc::new(common_dir),
            child_root,
            child_git,
            child_common,
        };
        authority.validate()?;
        Ok(authority)
    }

    pub(in crate::repository) fn stable_private_metadata_path(&self) -> &Path {
        self.git_dir.path()
    }
    pub(in crate::repository) fn stable_common_metadata_path(&self) -> &Path {
        self.common_dir.path()
    }

    pub(in crate::repository) fn private_metadata_path(&self) -> PathBuf {
        descriptor_path(&self.git_dir)
    }
    pub(in crate::repository) fn common_metadata_path(&self) -> PathBuf {
        descriptor_path(&self.common_dir)
    }

    /// Worker read locator. The caller must retain this original authority
    /// throughout every use; this never resolves the public checkout name.
    pub(crate) fn anchored_read_path(&self) -> PathBuf {
        descriptor_path(&self.root)
    }

    pub(super) fn anchored_path(&self) -> PathBuf {
        // The executor owns the descriptor throughout execute, including any
        // blocking parent-side filesystem work. Child Git uses its own inherited
        // directory descriptors, so parent FD reuse cannot retarget a survivor.
        descriptor_path(&self.root)
    }
}

pub(super) fn descriptor_path(root: &fs_safety::Root) -> PathBuf {
    PathBuf::from(format!(
        "/proc/{}/fd/{}/.",
        std::process::id(),
        root.descriptor()
    ))
}

/// The same owned file must be passed to CommandSpec::inherit_file. Journal
/// callers keep durable canonical names separately from this active operand.
pub(in crate::repository) fn inherited_directory_scope(
    root: &fs_safety::Root,
) -> Result<(PathBuf, Arc<fs::File>), RepositoryError> {
    if let Some(reason) = WorktreeAuthority::unsupported_reason() {
        return Err(ValidationError::Io(reason.into()).into());
    }
    let file = Arc::new(root.retained_directory().map_err(metadata_error)?);
    Ok((child_directory_path(&file)?, file))
}

pub(super) fn child_directory_path(file: &fs::File) -> Result<PathBuf, RepositoryError> {
    if let Some(reason) = WorktreeAuthority::unsupported_reason() {
        return Err(ValidationError::Io(reason.into()).into());
    }
    Ok(PathBuf::from(format!(
        "/proc/self/fd/{}/.",
        file.as_raw_fd()
    )))
}

pub(super) fn metadata_error(error: io::Error) -> RepositoryError {
    ValidationError::Io(error.to_string()).into()
}

pub(super) fn open_metadata_root(path: &Path) -> Result<fs_safety::Root, RepositoryError> {
    fs_safety::Root::open(path).map_err(metadata_error)
}

pub(super) fn same_directory(
    left: &fs_safety::Root,
    right: &fs_safety::Root,
) -> Result<bool, RepositoryError> {
    let left = left.metadata().map_err(metadata_error)?;
    let right = right.metadata().map_err(metadata_error)?;
    Ok((left.dev(), left.ino()) == (right.dev(), right.ino()))
}

/// Bounded regular-file reads cannot hang on a replaced FIFO or follow a new
/// symlink. Git's raw pathname bytes, including non-UTF8 paths, are preserved.
pub(super) fn read_locator(path: &Path) -> Result<Vec<u8>, RepositoryError> {
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .map_err(metadata_error)?;
    if !file.metadata().map_err(metadata_error)?.is_file() {
        return Err(
            ValidationError::Io("Git metadata locator is not a regular file".into()).into(),
        );
    }
    let mut bytes = Vec::new();
    file.take(8193)
        .read_to_end(&mut bytes)
        .map_err(metadata_error)?;
    if bytes.len() > 8192 {
        return Err(ValidationError::Io("Git metadata locator is too large".into()).into());
    }
    Ok(bytes)
}

pub(super) fn resolve_private_git_dir(root: &Path) -> Result<PathBuf, RepositoryError> {
    let marker = root.join(".git");
    let path = if fs::metadata(&marker).map_err(metadata_error)?.is_dir() {
        marker
    } else {
        let bytes = read_locator(&marker)?;
        let relative = bytes
            .strip_prefix(b"gitdir: ")
            .ok_or_else(|| ValidationError::Io("Invalid Git worktree locator".into()))?;
        root.join(bytes_to_path(trim_one_line_ending(relative)))
    };
    fs::canonicalize(path).map_err(metadata_error)
}

pub(super) fn resolve_common_git_dir(private: &Path) -> Result<PathBuf, RepositoryError> {
    let marker = private.join("commondir");
    let path = match fs::symlink_metadata(&marker) {
        Ok(_) => private.join(bytes_to_path(trim_one_line_ending(&read_locator(&marker)?))),
        Err(error) if error.kind() == io::ErrorKind::NotFound => private.to_owned(),
        Err(error) => return Err(metadata_error(error)),
    };
    fs::canonicalize(path).map_err(metadata_error)
}
