//! Original physical Git roots shared by creator, readers, and receiver.
use super::*;
mod recovery;
mod scope;
pub use scope::WorktreeAuthority;
pub(super) use scope::inherited_directory_scope;
use scope::{
    child_directory_path, descriptor_path, open_metadata_root, resolve_common_git_dir,
    resolve_private_git_dir,
};

// Fresh capture belongs to the actual original-source producer, not the
// minimal imported host receiver core, which accepts only its original roles.
impl WorktreeAuthority {
    pub(super) fn discard_index_lock(&self) -> Result<GitFileLock, RepositoryError> {
        self.validate()?;
        GitFileLock::acquire_beneath(&self.git_dir, OsStr::new("index"))
            .map_err(|reason| ValidationError::Io(reason).into())
    }

    pub fn capture(path: &Path) -> Result<Self, RepositoryError> {
        if let Some(reason) = Self::unsupported_reason() {
            return Err(ValidationError::Io(reason.into()).into());
        }
        if !path.is_absolute() {
            return Err(ValidationError::InvalidWorktree(path.display().to_string()).into());
        }
        let root =
            fs_safety::Root::open(path).map_err(|error| ValidationError::Io(error.to_string()))?;
        let git_dir = open_metadata_root(&resolve_private_git_dir(&descriptor_path(&root))?)?;
        let common_dir = open_metadata_root(&resolve_common_git_dir(&descriptor_path(&git_dir))?)?;
        Self::from_retained_roots(root, git_dir, common_dir)
    }
}

pub struct WorktreeExecutor<E> {
    inner: Arc<E>,
    authority: WorktreeAuthority,
}

impl<E> Clone for WorktreeExecutor<E> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            authority: self.authority.clone(),
        }
    }
}

impl<E: ProcessExecutor> ProcessExecutor for WorktreeExecutor<E> {
    fn execute(
        &self,
        command: &CommandSpec,
        cancellation: &CancellationToken,
    ) -> Result<ProcessOutput, ProcessFailure> {
        let refuse = |message: String| ProcessFailure {
            kind: ProcessFailureKind::Io,
            program: command.program.clone(),
            exit_code: None,
            signal: None,
            stdout: Vec::new(),
            stderr: Vec::new(),
            message,
            started: false,
        };
        self.authority
            .validate()
            .map_err(|error| refuse(error.to_string()))?;
        let inherited_root = self.authority.child_root.clone();
        let inherited_git = self.authority.child_git.clone();
        let inherited_common = self.authority.child_common.clone();
        let child_root =
            child_directory_path(&inherited_root).map_err(|error| refuse(error.to_string()))?;
        let child_git =
            child_directory_path(&inherited_git).map_err(|error| refuse(error.to_string()))?;
        let child_common =
            child_directory_path(&inherited_common).map_err(|error| refuse(error.to_string()))?;
        let mut scoped = command.clone();
        let mut anchored = false;
        let mut index = 0;
        while index < scoped.arguments.len() {
            let argument = scoped.arguments[index].to_string_lossy().into_owned();
            if argument == "--" || !argument.starts_with('-') {
                break;
            }
            match argument.as_ref() {
                "-C" => {
                    let Some(path) = scoped.arguments.get_mut(index + 1) else {
                        return Err(refuse("Missing worktree argument".into()));
                    };
                    if Path::new(path.as_os_str()) != self.authority.path() {
                        return Err(refuse(
                            "The command no longer names the captured worktree".into(),
                        ));
                    }
                    *path = child_root.clone().into_os_string();
                    anchored = true;
                    index += 2;
                }
                // Values may themselves be '-C'. They are not global options.
                "-c" | "--config-env" => {
                    if index + 1 >= scoped.arguments.len() {
                        return Err(refuse("Missing Git configuration argument".into()));
                    }
                    index += 2;
                }
                "--git-dir" | "--work-tree" => {
                    return Err(refuse(
                        "A captured worktree cannot be overridden by another Git scope".into(),
                    ));
                }
                value if value.starts_with("--git-dir=") || value.starts_with("--work-tree=") => {
                    return Err(refuse(
                        "A captured worktree cannot be overridden by another Git scope".into(),
                    ));
                }
                _ => index += 1,
            }
        }
        if let Some(path) = scoped.current_dir.as_mut() {
            if path.as_path() != self.authority.path() {
                return Err(refuse(
                    "The command no longer names the captured worktree".into(),
                ));
            }
            *path = child_root.clone();
            anchored = true;
        }
        if !anchored {
            return Err(refuse("The command has no captured worktree scope".into()));
        }
        // Explicit scopes prevent Git from rediscovering .git/commondir after
        // validation, including a delayed launch inside the executor. Child
        // scopes use owned inherited FDs, not a reusable parent FD number.
        // The authority also retains these exact duplicates, so Git --git-path
        // output using /proc/self/fd remains valid for subsequent parent IO.
        scoped.arguments.splice(
            0..0,
            [
                OsString::from(format!("--git-dir={}", child_git.display())),
                OsString::from(format!("--work-tree={}", child_root.display())),
            ],
        );
        scoped.environment.insert(
            OsString::from("GIT_COMMON_DIR"),
            child_common.into_os_string(),
        );
        scoped = scoped
            .inherit_file(inherited_root)
            .and_then(|spec| spec.inherit_file(inherited_git))
            .and_then(|spec| spec.inherit_file(inherited_common))
            .map_err(|error| refuse(error.to_string()))?;
        self.inner.execute(&scoped, cancellation)
    }
}

impl<E: ProcessExecutor> RepositoryService<E> {
    /// Bind subsequent operations to the retained directory. This does not
    /// authorize other paths, bypass filesystem permissions, or exclude Git
    /// processes outside this application from changing the original worktree.
    pub fn with_worktree_authority(
        &self,
        authority: WorktreeAuthority,
    ) -> Result<RepositoryService<WorktreeExecutor<E>>, RepositoryError> {
        authority.validate()?;
        Ok(RepositoryService {
            git: self.git.clone(),
            executor: Arc::new(WorktreeExecutor {
                inner: self.executor.clone(),
                authority: authority.clone(),
            }),
            authority: Some(authority),
            creation_executor: self.creation_executor.clone(),
        })
    }

    pub(super) fn captured_worktree_path(&self, path: &Path) -> Result<PathBuf, RepositoryError> {
        if let Some(authority) = &self.authority {
            authority.validate()?;
            if let Ok(relative) = path.strip_prefix(authority.path()) {
                if let Ok(metadata_relative) = relative.strip_prefix(".git")
                    && !metadata_relative.as_os_str().is_empty()
                {
                    return Ok(authority.private_metadata_path().join(metadata_relative));
                }
                return Ok(authority.anchored_path().join(relative));
            }
        }
        Ok(path.to_path_buf())
    }

    pub(super) fn git_dir_for_io(
        &self,
        worktree: &Path,
        cancellation: &CancellationToken,
    ) -> Result<PathBuf, RepositoryError> {
        if let Some(authority) = &self.authority {
            if authority.path() != worktree {
                return Err(
                    ValidationError::InvalidWorktree(worktree.display().to_string()).into(),
                );
            }
            authority.validate()?;
            return Ok(authority.private_metadata_path());
        }
        self.git_dir(worktree, cancellation)
    }

    pub(super) fn common_dir_for_io(
        &self,
        worktree: &Path,
        cancellation: &CancellationToken,
    ) -> Result<PathBuf, RepositoryError> {
        if let Some(authority) = &self.authority {
            if authority.path() != worktree {
                return Err(
                    ValidationError::InvalidWorktree(worktree.display().to_string()).into(),
                );
            }
            authority.validate()?;
            return Ok(authority.common_metadata_path());
        }
        self.common_dir(worktree, cancellation)
    }

    pub(super) fn captured_worktree_root(
        &self,
        path: &Path,
    ) -> Result<fs_safety::Root, RepositoryError> {
        if let Some(authority) = &self.authority {
            if authority.path() != path {
                return Err(ValidationError::InvalidWorktree(path.display().to_string()).into());
            }
            authority.validate()?;
            return authority
                .root
                .try_clone()
                .map_err(|error| ValidationError::Io(error.to_string()).into());
        }
        fs_safety::Root::open(path).map_err(|error| ValidationError::Io(error.to_string()).into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // These are filesystem scope contracts, not Git command/creation evidence.
    fn checkout(parent: &Path, name: &str) -> PathBuf {
        let root = parent.join(name);
        fs::create_dir_all(root.join(".git")).unwrap();
        fs::canonicalize(root).unwrap()
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn cloned_original_scope_refuses_replaced_checkout_and_private_metadata() {
        for replace_private in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let root = checkout(directory.path(), "original");
            let original = WorktreeAuthority::capture(&root).unwrap();
            let retained = original.clone();
            let other = WorktreeAuthority::capture(&checkout(directory.path(), "other")).unwrap();
            assert!(original.same_scope(&retained));
            assert!(!original.same_scope(&other));
            assert!(Arc::ptr_eq(
                &original.retained_worktree_root(),
                &retained.retained_worktree_root()
            ));
            let replaced = if replace_private {
                root.join(".git")
            } else {
                root.clone()
            };
            fs::rename(&replaced, replaced.with_extension("saved")).unwrap();
            fs::create_dir_all(&replaced).unwrap();
            // A pure comparison preserves the original identity; only worker
            // validation may determine whether the name still names it.
            assert!(original.same_scope(&retained));
            assert!(original.validate().is_err());
            assert!(retained.validate().is_err());
        }
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn original_scope_refuses_a_sibling_private_locator_in_the_same_common_directory() {
        let directory = tempfile::tempdir().unwrap();
        let root = checkout(directory.path(), "primary");
        let linked = directory.path().join("linked");
        fs::create_dir(&linked).unwrap();
        let common = root.join(".git");
        let private = common.join("worktrees/original");
        let sibling = common.join("worktrees/sibling");
        for admin in [&private, &sibling] {
            fs::create_dir_all(admin).unwrap();
            fs::write(admin.join("commondir"), "../..\n").unwrap();
        }
        fs::write(
            linked.join(".git"),
            format!("gitdir: {}\n", private.display()),
        )
        .unwrap();
        let original = WorktreeAuthority::capture(&linked).unwrap();
        fs::write(
            linked.join(".git"),
            format!("gitdir: {}\n", sibling.display()),
        )
        .unwrap();
        let redirected = WorktreeAuthority::capture(&linked).unwrap();
        assert!(!original.same_scope(&redirected));
        assert!(original.validate().is_err());
        assert!(redirected.validate().is_ok());
    }
}
