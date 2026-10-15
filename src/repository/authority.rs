//! A delayed UI intent owns the physical worktree it originally displayed.

use super::*;
use std::os::unix::fs::MetadataExt;

#[derive(Clone)]
pub struct WorktreeAuthority {
    root: Arc<fs_safety::Root>,
    identity: (u64, u64),
}

impl fmt::Debug for WorktreeAuthority {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("WorktreeAuthority").field("path", &self.path()).field("identity", &self.identity).finish()
    }
}

impl WorktreeAuthority {
    pub fn capture(path: &Path) -> Result<Self, RepositoryError> {
        if !path.is_absolute() { return Err(ValidationError::InvalidWorktree(path.display().to_string()).into()); }
        let root = fs_safety::Root::open(path).map_err(|error| ValidationError::Io(error.to_string()))?;
        let metadata = root.metadata().map_err(|error| ValidationError::Io(error.to_string()))?;
        let authority = Self { root: Arc::new(root), identity: (metadata.dev(), metadata.ino()) };
        authority.validate()?;
        Ok(authority)
    }

    pub fn path(&self) -> &Path { self.root.path() }

    pub fn validate(&self) -> Result<(), RepositoryError> {
        let named = fs_safety::Root::open(self.path()).map_err(|error| ValidationError::Io(format!("The original worktree is no longer available: {error}")))?;
        let metadata = named.metadata().map_err(|error| ValidationError::Io(error.to_string()))?;
        if (metadata.dev(), metadata.ino()) != self.identity {
            return Err(ValidationError::Io("The original worktree directory was replaced; no action was applied to the replacement".into()).into());
        }
        Ok(())
    }

    fn anchored_path(&self) -> PathBuf {
        // The executor owns the descriptor throughout execute, including any
        // blocking launch delay. A path replacement cannot redirect Git -C.
        PathBuf::from(format!("/proc/{}/fd/{}/.", std::process::id(), self.root.descriptor()))
    }
}

pub struct WorktreeExecutor<E> {
    inner: Arc<E>,
    authority: WorktreeAuthority,
}

impl<E: ProcessExecutor> ProcessExecutor for WorktreeExecutor<E> {
    fn execute(&self, command: &CommandSpec, cancellation: &CancellationToken) -> Result<ProcessOutput, ProcessFailure> {
        let refuse = |message: String| ProcessFailure {
            kind: ProcessFailureKind::Io, program: command.program.clone(), exit_code: None, signal: None,
            stdout: Vec::new(), stderr: Vec::new(), message, started: false,
        };
        self.authority.validate().map_err(|error| refuse(error.to_string()))?;
        let mut scoped = command.clone();
        let mut anchored = false;
        let mut index = 0;
        while index < scoped.arguments.len() {
            if scoped.arguments[index] == "-C" {
                let Some(path) = scoped.arguments.get_mut(index + 1) else { return Err(refuse("Missing worktree argument".into())); };
                if Path::new(path.as_os_str()) != self.authority.path() { return Err(refuse("The command no longer names the captured worktree".into())); }
                *path = self.authority.anchored_path().into_os_string();
                anchored = true;
                index += 1;
            }
            index += 1;
        }
        if let Some(path) = scoped.current_dir.as_mut() {
            if path.as_path() != self.authority.path() { return Err(refuse("The command no longer names the captured worktree".into())); }
            *path = self.authority.anchored_path();
            anchored = true;
        }
        if !anchored { return Err(refuse("The command has no captured worktree scope".into())); }
        self.inner.execute(&scoped, cancellation)
    }
}

impl<E: ProcessExecutor> RepositoryService<E> {
    /// Bind subsequent operations to the retained directory. This does not
    /// authorize other paths, bypass filesystem permissions, or exclude Git
    /// processes outside this application from changing the original worktree.
    pub fn with_worktree_authority(&self, authority: WorktreeAuthority) -> Result<RepositoryService<WorktreeExecutor<E>>, RepositoryError> {
        authority.validate()?;
        Ok(RepositoryService {
            git: self.git.clone(),
            executor: Arc::new(WorktreeExecutor { inner: self.executor.clone(), authority: authority.clone() }),
            authority: Some(authority),
        })
    }

    pub(super) fn captured_worktree_path(&self, path: &Path) -> Result<PathBuf, RepositoryError> {
        if let Some(authority) = &self.authority {
            authority.validate()?;
            if let Ok(relative) = path.strip_prefix(authority.path()) {
                return Ok(authority.anchored_path().join(relative));
            }
        }
        Ok(path.to_path_buf())
    }

    pub(super) fn captured_worktree_root(&self, path: &Path) -> Result<fs_safety::Root, RepositoryError> {
        if let Some(authority) = &self.authority {
            if authority.path() != path { return Err(ValidationError::InvalidWorktree(path.display().to_string()).into()); }
            authority.validate()?;
            return authority.root.try_clone().map_err(|error| ValidationError::Io(error.to_string()).into());
        }
        fs_safety::Root::open(path).map_err(|error| ValidationError::Io(error.to_string()).into())
    }
}
