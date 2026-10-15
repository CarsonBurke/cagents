//! A delayed UI intent owns the physical worktree it originally displayed.

use super::*;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};

#[derive(Clone)]
pub struct WorktreeAuthority {
    root: Arc<fs_safety::Root>,
    identity: (u64, u64),
    git_dir: Arc<fs_safety::Root>,
    common_dir: Arc<fs_safety::Root>,
    child_root: Arc<fs::File>,
    child_git: Arc<fs::File>,
    child_common: Arc<fs::File>,
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

    pub fn path(&self) -> &Path {
        self.root.path()
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
        let named_private = open_metadata_root(self.git_dir.path())?;
        let named_common = open_metadata_root(self.common_dir.path())?;
        if !same_directory(&named_private, &self.git_dir)?
            || !same_directory(&named_common, &self.common_dir)?
        {
            return Err(ValidationError::Io("The original Git metadata directory name was replaced; reopen the worktree before retrying".into()).into());
        }
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
    pub(super) fn retained_roots(
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
    pub(super) fn from_retained_roots(
        root: fs_safety::Root,
        git_dir: fs_safety::Root,
        common_dir: fs_safety::Root,
    ) -> Result<Self, RepositoryError> {
        let metadata = root
            .metadata()
            .map_err(|error| ValidationError::Io(error.to_string()))?;
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
            git_dir: Arc::new(git_dir),
            common_dir: Arc::new(common_dir),
            child_root,
            child_git,
            child_common,
        };
        authority.validate()?;
        Ok(authority)
    }

    pub(super) fn stable_private_metadata_path(&self) -> &Path {
        self.git_dir.path()
    }
    pub(super) fn stable_common_metadata_path(&self) -> &Path {
        self.common_dir.path()
    }

    pub(super) fn private_metadata_path(&self) -> PathBuf {
        descriptor_path(&self.git_dir)
    }
    pub(super) fn common_metadata_path(&self) -> PathBuf {
        descriptor_path(&self.common_dir)
    }

    fn anchored_path(&self) -> PathBuf {
        // The executor owns the descriptor throughout execute, including any
        // blocking parent-side filesystem work. Child Git uses its own inherited
        // directory descriptors, so parent FD reuse cannot retarget a survivor.
        descriptor_path(&self.root)
    }
}

fn descriptor_path(root: &fs_safety::Root) -> PathBuf {
    PathBuf::from(format!(
        "/proc/{}/fd/{}/.",
        std::process::id(),
        root.descriptor()
    ))
}

/// The same owned file must be passed to CommandSpec::inherit_file. Journal
/// callers keep durable canonical names separately from this active operand.
pub(super) fn inherited_directory_scope(
    root: &fs_safety::Root,
) -> Result<(PathBuf, Arc<fs::File>), RepositoryError> {
    if let Some(reason) = WorktreeAuthority::unsupported_reason() {
        return Err(ValidationError::Io(reason.into()).into());
    }
    let file = Arc::new(root.retained_directory().map_err(metadata_error)?);
    Ok((child_directory_path(&file)?, file))
}

fn child_directory_path(file: &fs::File) -> Result<PathBuf, RepositoryError> {
    if let Some(reason) = WorktreeAuthority::unsupported_reason() {
        return Err(ValidationError::Io(reason.into()).into());
    }
    Ok(PathBuf::from(format!(
        "/proc/self/fd/{}/.",
        file.as_raw_fd()
    )))
}

fn metadata_error(error: io::Error) -> RepositoryError {
    ValidationError::Io(error.to_string()).into()
}

fn open_metadata_root(path: &Path) -> Result<fs_safety::Root, RepositoryError> {
    fs_safety::Root::open(path).map_err(metadata_error)
}

fn same_directory(
    left: &fs_safety::Root,
    right: &fs_safety::Root,
) -> Result<bool, RepositoryError> {
    let left = left.metadata().map_err(metadata_error)?;
    let right = right.metadata().map_err(metadata_error)?;
    Ok((left.dev(), left.ino()) == (right.dev(), right.ino()))
}

/// Bounded regular-file reads cannot hang on a replaced FIFO or follow a new
/// symlink. Git's raw pathname bytes, including non-UTF8 paths, are preserved.
fn read_locator(path: &Path) -> Result<Vec<u8>, RepositoryError> {
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

fn resolve_private_git_dir(root: &Path) -> Result<PathBuf, RepositoryError> {
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

fn resolve_common_git_dir(private: &Path) -> Result<PathBuf, RepositoryError> {
    let marker = private.join("commondir");
    let path = match fs::symlink_metadata(&marker) {
        Ok(_) => private.join(bytes_to_path(trim_one_line_ending(&read_locator(&marker)?))),
        Err(error) if error.kind() == io::ErrorKind::NotFound => private.to_owned(),
        Err(error) => return Err(metadata_error(error)),
    };
    fs::canonicalize(path).map_err(metadata_error)
}

pub struct WorktreeExecutor<E> {
    inner: Arc<E>,
    authority: WorktreeAuthority,
}

/// A journal-owned destination handoff retains the same original executor.
/// No unwrapping API is exposed to callers, and this cannot authorize an
/// arbitrary second root.
pub(super) struct JournalExecutor {
    original: Arc<dyn ProcessExecutor>,
    created: Arc<super::creation_authority::VerifiedCreatedRoot>,
}

pub(super) struct JournalSourceExecutor {
    original: Arc<dyn ProcessExecutor>,
    source: Arc<super::creation_authority::CreationSession>,
}

impl ProcessExecutor for JournalSourceExecutor {
    fn execute(
        &self,
        command: &CommandSpec,
        cancellation: &CancellationToken,
    ) -> Result<ProcessOutput, ProcessFailure> {
        self.source.validate().map_err(|error| ProcessFailure {
            kind: ProcessFailureKind::Io,
            program: command.program.clone(),
            exit_code: None,
            signal: None,
            stdout: Vec::new(),
            stderr: Vec::new(),
            message: error.to_string(),
            started: false,
        })?;
        self.original.execute(command, cancellation)
    }
}

impl ProcessExecutor for JournalExecutor {
    fn execute(
        &self,
        command: &CommandSpec,
        cancellation: &CancellationToken,
    ) -> Result<ProcessOutput, ProcessFailure> {
        self.created.validate().map_err(|error| ProcessFailure {
            kind: ProcessFailureKind::Io,
            program: command.program.clone(),
            exit_code: None,
            signal: None,
            stdout: Vec::new(),
            stderr: Vec::new(),
            message: error.to_string(),
            started: false,
        })?;
        self.original.execute(command, cancellation)
    }
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

enum RecoveryObjectCommand<'a> {
    Stream {
        oid: &'a Oid,
        file: &'a Arc<fs::File>,
        max_bytes: u64,
    },
    Hash(&'a Arc<fs::File>),
}

impl<E: ProcessExecutor> RepositoryService<E> {
    /// Retain the original object-store authority for one recovery operation.
    /// Recovery requires the original retained metadata authority. A later
    /// pathname capture cannot reconstruct the repository the user approved.
    pub(super) fn recovery_source_authority(
        &self,
        worktree: &Path,
        expected_root: &fs_safety::Root,
    ) -> Result<WorktreeAuthority, RepositoryError> {
        let authority = match &self.authority {
            Some(authority) if authority.path() == worktree => authority.clone(),
            Some(_) => {
                return Err(
                    ValidationError::InvalidWorktree(worktree.display().to_string()).into(),
                );
            }
            None => return Err(ValidationError::Io(
                "Undo requires the original retained Git metadata authority; no path was undone"
                    .into(),
            )
            .into()),
        };
        authority.validate()?;
        let expected = expected_root.metadata().map_err(metadata_error)?;
        if authority.identity != (expected.dev(), expected.ino()) {
            return Err(ValidationError::Io(
                "The recovery source worktree was replaced; no replacement was changed".into(),
            )
            .into());
        }
        Ok(authority)
    }

    /// Fixed recovery-only commands. No general executor or foreign-root
    /// service is exposed: the private destination is a published held inode,
    /// and every Git metadata/worktree scope stays on the original authority.
    pub(super) fn stream_recovery_object(
        &self,
        source: &WorktreeAuthority,
        holding: &fs_safety::DirectoryPublication,
        oid: &Oid,
        file: &Arc<fs::File>,
        expected_bytes: u64,
        cancellation: &CancellationToken,
    ) -> Result<ProcessOutput, RepositoryError> {
        let max_bytes = expected_bytes
            .checked_add(1)
            .ok_or_else(|| ValidationError::Io("Recovery blob size budget overflows".into()))?;
        self.run_recovery_object(
            source,
            holding,
            RecoveryObjectCommand::Stream {
                oid,
                file,
                max_bytes,
            },
            cancellation,
        )
    }

    pub(super) fn hash_recovery_object(
        &self,
        source: &WorktreeAuthority,
        holding: &fs_safety::DirectoryPublication,
        file: &Arc<fs::File>,
        cancellation: &CancellationToken,
    ) -> Result<Oid, RepositoryError> {
        let output = self.run_recovery_object(
            source,
            holding,
            RecoveryObjectCommand::Hash(file),
            cancellation,
        )?;
        let oid = std::str::from_utf8(&output.stdout).map_err(|error| {
            RepositoryError::InvalidOutput {
                field: "recovery object hash",
                reason: error.to_string(),
            }
        })?;
        Oid::parse_object(oid.trim()).map_err(|error| RepositoryError::InvalidOutput {
            field: "recovery object hash",
            reason: error.to_string(),
        })
    }

    fn run_recovery_object(
        &self,
        source: &WorktreeAuthority,
        holding: &fs_safety::DirectoryPublication,
        operation: RecoveryObjectCommand<'_>,
        cancellation: &CancellationToken,
    ) -> Result<ProcessOutput, RepositoryError> {
        source.validate()?;
        let metadata = holding.directory().metadata().map_err(metadata_error)?;
        if holding.identity() != (metadata.dev(), metadata.ino())
            || metadata.mode() & 0o7777 != 0o700
            || metadata.uid() != unsafe { libc::geteuid() }
            || metadata.uid() != holding.initial_metadata().uid()
            || metadata.gid() != holding.initial_metadata().gid()
        {
            return Err(ValidationError::Io(
                "Recovery materialization requires the exact published private holding directory"
                    .into(),
            )
            .into());
        }
        if let Some(error) = holding.publication_error() {
            return Err(ValidationError::Io(format!(
                "Recovery holding publication requires resolution: {error}"
            ))
            .into());
        }
        let (holding_path, holding_file) = inherited_directory_scope(holding.directory())?;
        let git_path = child_directory_path(&source.child_git)?;
        let common_path = child_directory_path(&source.child_common)?;
        let mut command = CommandSpec::new(&self.git);
        // Remove ALL inherited Git injection, including object/config routing.
        // Only explicit original metadata scopes are installed below.
        for (name, _) in std::env::vars_os() {
            if name.as_encoded_bytes().starts_with(b"GIT_") {
                command = command.env_remove(name);
            }
        }
        command = command
            .args([
                OsString::from("--no-replace-objects"),
                OsString::from(format!("--git-dir={}", git_path.display())),
                // RUN_SETUP may chdir to --work-tree when cwd is beneath it.
                // Object-only operations intentionally use the held private
                // directory for BOTH cwd and work-tree; metadata stays original.
                OsString::from(format!("--work-tree={}", holding_path.display())),
                OsString::from("-C"),
                holding_path.into_os_string(),
            ])
            .args([
                "-c",
                "core.fsmonitor=false",
                "-c",
                "core.hooksPath=/dev/null",
            ])
            .env("GIT_COMMON_DIR", common_path.into_os_string())
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_PAGER", "cat")
            .env("GIT_NO_LAZY_FETCH", "1")
            .env("GIT_ALLOW_PROTOCOL", "")
            .output_limits(8192, 256 * 1024)
            .inherit_file(holding_file)
            .map_err(metadata_error)?
            .inherit_file(source.child_root.clone())
            .map_err(metadata_error)?
            .inherit_file(source.child_git.clone())
            .map_err(metadata_error)?
            .inherit_file(source.child_common.clone())
            .map_err(metadata_error)?;
        command = match operation {
            RecoveryObjectCommand::Stream {
                oid,
                file,
                max_bytes,
            } => command
                .args(["cat-file", "blob", oid.as_str()])
                .stdout_file(file.clone(), max_bytes)
                .map_err(metadata_error)?,
            RecoveryObjectCommand::Hash(file) => command
                .args(["hash-object", "--no-filters", "--"])
                .arg(format!("/proc/self/fd/{}", file.as_raw_fd()))
                .inherit_file(file.clone())
                .map_err(metadata_error)?,
        };
        // The original executor is private. Its only recovery use accepts the
        // typed original authority plus this fixed operation and owned holding.
        let output = self
            .creation_executor
            .execute(&command, cancellation)
            .inspect_err(|failure| log_git_failure(&command, failure))?;
        if output.success {
            Ok(output)
        } else {
            Err(git_failure(&command, output).into())
        }
    }

    pub(super) fn journal_source_service(
        &self,
        source: Arc<super::creation_authority::CreationSession>,
    ) -> Result<RepositoryService<WorktreeExecutor<JournalSourceExecutor>>, RepositoryError> {
        let authority = source.source_authority()?;
        Ok(RepositoryService {
            git: self.git.clone(),
            executor: Arc::new(WorktreeExecutor {
                inner: Arc::new(JournalSourceExecutor {
                    original: self.creation_executor.clone(),
                    source,
                }),
                authority: authority.clone(),
            }),
            authority: Some(authority),
            recovery_context: self.recovery_context.clone(),
            creation_executor: self.creation_executor.clone(),
        })
    }

    pub(super) fn created_worktree_service(
        &self,
        created: Arc<super::creation_authority::VerifiedCreatedRoot>,
    ) -> Result<RepositoryService<WorktreeExecutor<JournalExecutor>>, RepositoryError> {
        let authority = created.authority()?;
        Ok(RepositoryService {
            git: self.git.clone(),
            executor: Arc::new(WorktreeExecutor {
                inner: Arc::new(JournalExecutor {
                    original: self.creation_executor.clone(),
                    created,
                }),
                authority: authority.clone(),
            }),
            authority: Some(authority),
            // Source cleanup/placement consent never transfers to the newly
            // created destination. Its mutations need a fresh domain context.
            recovery_context: None,
            creation_executor: self.creation_executor.clone(),
        })
    }

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
            recovery_context: self.recovery_context.clone(),
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

#[cfg(all(test, target_os = "linux"))]
mod authority_tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    struct RecordingExecutor(Mutex<Vec<CommandSpec>>);
    impl ProcessExecutor for RecordingExecutor {
        fn execute(
            &self,
            command: &CommandSpec,
            _: &CancellationToken,
        ) -> Result<ProcessOutput, ProcessFailure> {
            self.0.lock().unwrap().push(command.clone());
            Ok(ProcessOutput {
                exit_code: Some(0),
                signal: None,
                success: true,
                stdout: vec![],
                stdout_truncated: false,
                stderr: vec![],
            })
        }
    }

    #[test]
    fn captured_authority_rewrites_only_global_worktree_scope_and_preserves_literal_c_data() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::create_dir(directory.path().join(".git")).unwrap();
        let authority = WorktreeAuthority::capture(directory.path()).unwrap();
        let inner = Arc::new(RecordingExecutor::default());
        let executor = WorktreeExecutor {
            inner: inner.clone(),
            authority: authority.clone(),
        };
        for tail in [
            vec!["diff", "--", "-C"],
            vec!["log", "--grep", "-C", "--", "file"],
            vec!["-c", "example.value=-C", "log", "--grep=-C"],
        ] {
            let command = CommandSpec::new("git")
                .args([std::ffi::OsStr::new("-C"), directory.path().as_os_str()])
                .args(tail.iter().copied());
            executor
                .execute(&command, &CancellationToken::default())
                .unwrap();
            let commands = inner.0.lock().unwrap();
            let scoped = commands.last().unwrap();
            assert_eq!(scoped.arguments[2], "-C");
            let child_root = Path::new(&scoped.arguments[3]);
            assert!(child_root.starts_with("/proc/self/fd"));
            let scoped_identity = fs::metadata(child_root).unwrap();
            let expected_identity = authority.root.metadata().unwrap();
            assert_eq!(
                (scoped_identity.dev(), scoped_identity.ino()),
                (expected_identity.dev(), expected_identity.ino())
            );
            assert_eq!(&scoped.arguments[4..], &command.arguments[2..]);
        }
    }

    #[test]
    fn captured_authority_refuses_replacement_before_invoking_git() {
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("worktree");
        std::fs::create_dir(&root).unwrap();
        std::fs::create_dir(root.join(".git")).unwrap();
        let authority = WorktreeAuthority::capture(&root).unwrap();
        let inner = Arc::new(RecordingExecutor::default());
        let executor = WorktreeExecutor {
            inner: inner.clone(),
            authority,
        };
        std::fs::rename(&root, parent.path().join("original")).unwrap();
        std::fs::create_dir(&root).unwrap();
        let command = CommandSpec::new("git").args([
            std::ffi::OsStr::new("-C"),
            root.as_os_str(),
            std::ffi::OsStr::new("add"),
            std::ffi::OsStr::new("--all"),
        ]);
        assert!(
            executor
                .execute(&command, &CancellationToken::default())
                .is_err()
        );
        assert!(inner.0.lock().unwrap().is_empty());
    }
    fn isolated_spec(mut command: CommandSpec) -> CommandSpec {
        for (name, _) in
            std::env::vars_os().filter(|(name, _)| name.as_encoded_bytes().starts_with(b"GIT_"))
        {
            command = command.env_remove(name);
        }
        command
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
    }

    fn actual_git(root: &Path, arguments: &[&str]) -> ProcessOutput {
        let command = isolated_spec(
            CommandSpec::new("git")
                .args([
                    "-c",
                    "core.fsmonitor=false",
                    "-c",
                    "core.hooksPath=/dev/null",
                    "-c",
                    "init.templateDir=",
                ])
                .arg("-C")
                .arg(root)
                .args(arguments),
        );
        let output = SystemProcessExecutor
            .execute(&command, &CancellationToken::default())
            .unwrap();
        assert!(
            output.success,
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }

    fn recovery_holding(parent: &Path) -> fs_safety::DirectoryPublication {
        let parent = fs_safety::Root::open(parent).unwrap();
        match fs_safety::create_directory_beneath(&parent, Path::new("holding"), 0o700).unwrap() {
            fs_safety::DirectoryCreation::Published(receipt) => receipt,
            other => panic!("expected private holding publication: {other:?}"),
        }
    }

    fn recovery_sink(holding: &fs_safety::DirectoryPublication) -> Arc<fs::File> {
        Arc::new(
            fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .open(holding.directory().path().join("restored-blob"))
                .unwrap(),
        )
    }

    #[test]
    fn recovery_object_helper_materializes_and_hashes_only_inside_owned_holding() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source");
        fs::create_dir(&source).unwrap();
        actual_git(&source, &["init", "-b", "main"]);
        fs::write(source.join("file"), b"recovery proof").unwrap();
        let output = actual_git(
            &source,
            &["hash-object", "-w", "--no-filters", "--", "file"],
        );
        let oid = Oid::parse_object(String::from_utf8(output.stdout).unwrap().trim()).unwrap();
        let service = RepositoryService::system("git");
        let authority = WorktreeAuthority::capture(&source).unwrap();
        let holding = recovery_holding(directory.path());
        let cancel = CancellationToken::new();
        let file = recovery_sink(&holding);
        let output = service
            .stream_recovery_object(&authority, &holding, &oid, &file, 14, &cancel)
            .unwrap();
        assert!(output.stdout.is_empty());
        assert_eq!(
            service
                .hash_recovery_object(&authority, &holding, &file, &cancel)
                .unwrap(),
            oid
        );
        assert_eq!(
            fs::read(holding.directory().path().join("restored-blob")).unwrap(),
            b"recovery proof"
        );
        assert!(!source.join("restored-blob").exists());
    }

    #[test]
    fn recovery_object_helper_inside_worktree_or_gitdir_never_materializes_in_shared_root() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source");
        fs::create_dir(&source).unwrap();
        actual_git(&source, &["init", "-b", "main"]);
        fs::write(source.join("file"), b"private bytes").unwrap();
        let output = actual_git(&source, &["hash-object", "-w", "--", "file"]);
        let oid = Oid::parse_object(String::from_utf8(output.stdout).unwrap().trim()).unwrap();
        let service = RepositoryService::system("git");
        let authority = WorktreeAuthority::capture(&source).unwrap();
        for parent in [source.clone(), source.join(".git")] {
            let holding = recovery_holding(&parent);
            let file = recovery_sink(&holding);
            service
                .stream_recovery_object(
                    &authority,
                    &holding,
                    &oid,
                    &file,
                    13,
                    &CancellationToken::new(),
                )
                .unwrap();
            assert_eq!(
                fs::read(holding.directory().path().join("restored-blob")).unwrap(),
                b"private bytes"
            );
            assert!(!source.join("restored-blob").exists());
            assert!(!source.join(".git/restored-blob").exists());
        }
    }

    #[test]
    fn recovery_object_helper_refuses_changed_source_metadata_before_original_executor() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source");
        fs::create_dir(&source).unwrap();
        actual_git(&source, &["init", "-b", "main"]);
        let authority = WorktreeAuthority::capture(&source).unwrap();
        let holding = recovery_holding(directory.path());
        let inner = Arc::new(RecordingExecutor::default());
        let service = RepositoryService::new("git", inner.clone());
        fs::rename(source.join(".git"), directory.path().join("saved-metadata")).unwrap();
        fs::create_dir(source.join(".git")).unwrap();
        let oid = Oid::parse_object("a".repeat(40)).unwrap();
        let file = recovery_sink(&holding);
        assert!(
            service
                .stream_recovery_object(
                    &authority,
                    &holding,
                    &oid,
                    &file,
                    1,
                    &CancellationToken::new()
                )
                .is_err()
        );
        assert_eq!(file.metadata().unwrap().len(), 0);
        assert!(inner.0.lock().unwrap().is_empty());
        assert_eq!(fs::read_dir(holding.directory().path()).unwrap().count(), 1);
    }

    #[test]
    fn recovery_object_helper_pins_metadata_and_holding_across_delayed_inner_launch() {
        struct Delayed {
            source: PathBuf,
            parent: PathBuf,
            parked: PathBuf,
        }
        impl ProcessExecutor for Delayed {
            fn execute(
                &self,
                command: &CommandSpec,
                cancel: &CancellationToken,
            ) -> Result<ProcessOutput, ProcessFailure> {
                assert!(
                    command
                        .arguments
                        .iter()
                        .any(|argument| argument == "cat-file")
                );
                fs::rename(self.source.join(".git"), self.parent.join("saved-git")).unwrap();
                fs::create_dir(self.source.join(".git")).unwrap();
                fs::rename(self.parent.join("holding"), &self.parked).unwrap();
                fs::create_dir(self.parent.join("holding")).unwrap();
                fs::write(self.parent.join("holding").join("foreign"), b"preserve").unwrap();
                SystemProcessExecutor.execute(command, cancel)
            }
        }
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source");
        fs::create_dir(&source).unwrap();
        actual_git(&source, &["init", "-b", "main"]);
        fs::write(source.join("file"), b"original object").unwrap();
        let output = actual_git(&source, &["hash-object", "-w", "--", "file"]);
        let oid = Oid::parse_object(String::from_utf8(output.stdout).unwrap().trim()).unwrap();
        let authority = WorktreeAuthority::capture(&source).unwrap();
        let holding = recovery_holding(directory.path());
        let parked = directory.path().join("retained-holding");
        let service = RepositoryService::new(
            "git",
            Arc::new(Delayed {
                source: source.clone(),
                parent: directory.path().to_owned(),
                parked: parked.clone(),
            }),
        );
        let file = recovery_sink(&holding);
        service
            .stream_recovery_object(
                &authority,
                &holding,
                &oid,
                &file,
                15,
                &CancellationToken::new(),
            )
            .unwrap();
        assert_eq!(
            fs::read(parked.join("restored-blob")).unwrap(),
            b"original object"
        );
        assert!(!directory.path().join("holding/restored-blob").exists());
        assert_eq!(
            fs::read(directory.path().join("holding/foreign")).unwrap(),
            b"preserve"
        );
        assert_eq!(fs::read_dir(source.join(".git")).unwrap().count(), 0);
    }

    #[test]
    fn recovery_object_stream_is_disk_bounded_and_cancellation_preserves_owned_partial() {
        use std::os::unix::fs::FileExt;
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source");
        fs::create_dir(&source).unwrap();
        actual_git(&source, &["init", "-b", "main"]);
        let length = 5 * 1024 * 1024;
        fs::write(source.join("file"), vec![b'x'; length]).unwrap();
        let output = actual_git(&source, &["hash-object", "-w", "--", "file"]);
        let oid = Oid::parse_object(String::from_utf8(output.stdout).unwrap().trim()).unwrap();
        let service = RepositoryService::system("git");
        let authority = WorktreeAuthority::capture(&source).unwrap();
        for scenario in ["complete", "budget", "cancelled"] {
            let parent = directory.path().join(scenario);
            fs::create_dir(&parent).unwrap();
            let holding = recovery_holding(&parent);
            let file = recovery_sink(&holding);
            let cancellation = CancellationToken::new();
            if scenario == "cancelled" {
                cancellation.cancel();
            }
            let expected = if scenario == "budget" {
                31
            } else {
                length as u64
            };
            let result = service.stream_recovery_object(
                &authority,
                &holding,
                &oid,
                &file,
                expected,
                &cancellation,
            );
            match scenario {
                "complete" => {
                    let output = result.unwrap();
                    assert!(output.stdout.is_empty());
                    assert_eq!(file.metadata().unwrap().len(), length as u64);
                    assert_eq!(
                        service
                            .hash_recovery_object(&authority, &holding, &file, &cancellation)
                            .unwrap(),
                        oid
                    );
                    let mut tail = [0; 1];
                    file.read_at(&mut tail, length as u64 - 1).unwrap();
                    assert_eq!(tail, [b'x']);
                }
                "budget" => {
                    assert!(result.is_err());
                    assert!(file.metadata().unwrap().len() > 0);
                    assert!(file.metadata().unwrap().len() <= 32);
                }
                _ => {
                    assert!(result.is_err());
                    assert_eq!(file.metadata().unwrap().len(), 0);
                }
            }
            assert!(
                holding.directory().path().join("restored-blob").exists(),
                "the caller retains the actual sink on every outcome"
            );
        }
    }

    #[test]
    fn captured_authority_actual_git_reads_and_stages_literal_c_filename() {
        struct Isolated;
        impl ProcessExecutor for Isolated {
            fn execute(
                &self,
                command: &CommandSpec,
                cancel: &CancellationToken,
            ) -> Result<ProcessOutput, ProcessFailure> {
                SystemProcessExecutor.execute(&isolated_spec(command.clone()), cancel)
            }
        }
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        actual_git(root, &["init", "-b", "main"]);
        actual_git(root, &["config", "core.fsmonitor", "false"]);
        actual_git(root, &["config", "core.hooksPath", "/dev/null"]);
        std::fs::write(root.join("-C"), "original\n").unwrap();
        let service = RepositoryService::new("git", Arc::new(Isolated))
            .with_worktree_authority(WorktreeAuthority::capture(root).unwrap())
            .unwrap();
        let cancel = CancellationToken::default();
        assert_eq!(service.status(root, &cancel).unwrap().entries.len(), 1);
        assert!(matches!(
            service
                .stage(
                    &StageRequest {
                        worktree: root.to_owned(),
                        selection: vec![PathSelection::new(RepoPath::parse("-C").unwrap())]
                    },
                    &cancel
                )
                .unwrap(),
            MutationOutcome::Applied { .. }
        ));
        let status = service.status(root, &cancel).unwrap();
        assert_eq!(status.entries[0].path, RepoPath::parse("-C").unwrap());
        assert_eq!(status.entries[0].area, ChangeArea::Staged);
    }

    #[test]
    fn captured_authority_pins_actual_git_effects_across_inner_launch_delay() {
        struct ReplaceBeforeLaunch {
            root: PathBuf,
            parked: PathBuf,
        }
        impl ProcessExecutor for ReplaceBeforeLaunch {
            fn execute(
                &self,
                command: &CommandSpec,
                cancel: &CancellationToken,
            ) -> Result<ProcessOutput, ProcessFailure> {
                std::fs::rename(&self.root, &self.parked).unwrap();
                std::fs::create_dir(&self.root).unwrap();
                actual_git(&self.root, &["init", "-b", "main"]);
                std::fs::write(self.root.join("-C"), "replacement\n").unwrap();
                SystemProcessExecutor.execute(&isolated_spec(command.clone()), cancel)
            }
        }
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("worktree");
        let parked = directory.path().join("original");
        std::fs::create_dir(&root).unwrap();
        actual_git(&root, &["init", "-b", "main"]);
        std::fs::write(root.join("-C"), "original\n").unwrap();
        let executor = WorktreeExecutor {
            inner: Arc::new(ReplaceBeforeLaunch {
                root: root.clone(),
                parked: parked.clone(),
            }),
            authority: WorktreeAuthority::capture(&root).unwrap(),
        };
        let command = CommandSpec::new("git").arg("-C").arg(&root).args([
            "--literal-pathspecs",
            "add",
            "--",
            "-C",
        ]);
        let result = executor
            .execute(&command, &CancellationToken::default())
            .unwrap();
        assert!(
            result.success,
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert_eq!(actual_git(&parked, &["ls-files", "-z"]).stdout, b"-C\0");
        assert!(actual_git(&root, &["ls-files", "-z"]).stdout.is_empty());
        assert_eq!(std::fs::read(root.join("-C")).unwrap(), b"replacement\n");
        assert_eq!(std::fs::read(parked.join("-C")).unwrap(), b"original\n");
    }

    #[test]
    fn captured_authority_refuses_private_or_common_metadata_replacement_before_git() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("worktree");
        let foreign = directory.path().join("foreign");
        std::fs::create_dir(&root).unwrap();
        std::fs::create_dir(&foreign).unwrap();
        actual_git(&root, &["init", "-b", "main"]);
        actual_git(&foreign, &["init", "-b", "main"]);
        let authority = WorktreeAuthority::capture(&root).unwrap();
        let root_identity = std::fs::metadata(&root).unwrap().ino();
        std::fs::rename(root.join(".git"), directory.path().join("original-git")).unwrap();
        std::fs::rename(foreign.join(".git"), root.join(".git")).unwrap();
        assert_eq!(std::fs::metadata(&root).unwrap().ino(), root_identity);
        assert!(authority.validate().is_err());
        std::fs::rename(root.join(".git"), foreign.join(".git")).unwrap();
        std::fs::rename(directory.path().join("original-git"), root.join(".git")).unwrap();
        assert!(authority.validate().is_ok());
        std::fs::write(
            root.join(".git/commondir"),
            foreign.join(".git").as_os_str().as_encoded_bytes(),
        )
        .unwrap();
        assert!(authority.validate().is_err());
    }

    #[test]
    fn captured_authority_pins_private_metadata_after_validation_before_actual_git_launch() {
        struct ReplaceMetadata {
            root: PathBuf,
            foreign: PathBuf,
            parked: PathBuf,
        }
        impl ProcessExecutor for ReplaceMetadata {
            fn execute(
                &self,
                command: &CommandSpec,
                cancel: &CancellationToken,
            ) -> Result<ProcessOutput, ProcessFailure> {
                std::fs::rename(self.root.join(".git"), &self.parked).unwrap();
                std::fs::rename(self.foreign.join(".git"), self.root.join(".git")).unwrap();
                SystemProcessExecutor.execute(&isolated_spec(command.clone()), cancel)
            }
        }
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("worktree");
        let foreign = directory.path().join("foreign");
        let parked = directory.path().join("original-git");
        std::fs::create_dir(&root).unwrap();
        std::fs::create_dir(&foreign).unwrap();
        actual_git(&root, &["init", "-b", "main"]);
        actual_git(&foreign, &["init", "-b", "main"]);
        std::fs::write(root.join("new.txt"), "original work\n").unwrap();
        std::fs::write(foreign.join("new.txt"), "foreign staged work\n").unwrap();
        actual_git(&foreign, &["add", "new.txt"]);
        let foreign_index = std::fs::read(foreign.join(".git/index")).unwrap();
        let executor = WorktreeExecutor {
            inner: Arc::new(ReplaceMetadata {
                root: root.clone(),
                foreign: foreign.clone(),
                parked: parked.clone(),
            }),
            authority: WorktreeAuthority::capture(&root).unwrap(),
        };
        let command = CommandSpec::new("git")
            .arg("-C")
            .arg(&root)
            .args(["add", "--", "new.txt"]);
        let result = executor
            .execute(&command, &CancellationToken::default())
            .unwrap();
        assert!(
            result.success,
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert!(
            parked.join("index").is_file(),
            "original retained metadata receives the stage"
        );
        assert_eq!(
            std::fs::read(root.join(".git/index")).unwrap(),
            foreign_index,
            "foreign metadata is untouched despite occupying .git at launch"
        );
    }

    #[test]
    fn captured_authority_pins_common_object_store_after_validation_before_actual_git_launch() {
        struct RedirectCommon {
            root: PathBuf,
            foreign: PathBuf,
        }
        impl ProcessExecutor for RedirectCommon {
            fn execute(
                &self,
                command: &CommandSpec,
                cancel: &CancellationToken,
            ) -> Result<ProcessOutput, ProcessFailure> {
                std::fs::write(
                    self.root.join(".git/commondir"),
                    self.foreign.join(".git").as_os_str().as_encoded_bytes(),
                )
                .unwrap();
                SystemProcessExecutor.execute(&isolated_spec(command.clone()), cancel)
            }
        }
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("worktree");
        let foreign = directory.path().join("foreign");
        std::fs::create_dir(&root).unwrap();
        std::fs::create_dir(&foreign).unwrap();
        actual_git(&root, &["init", "-b", "main"]);
        actual_git(&foreign, &["init", "-b", "main"]);
        std::fs::write(root.join("new.txt"), "new original object\n").unwrap();
        let oid = String::from_utf8(actual_git(&root, &["hash-object", "new.txt"]).stdout).unwrap();
        let oid = oid.trim();
        let object = format!("objects/{}/{}", &oid[..2], &oid[2..]);
        let executor = WorktreeExecutor {
            inner: Arc::new(RedirectCommon {
                root: root.clone(),
                foreign: foreign.clone(),
            }),
            authority: WorktreeAuthority::capture(&root).unwrap(),
        };
        let command = CommandSpec::new("git")
            .arg("-C")
            .arg(&root)
            .args(["add", "--", "new.txt"]);
        let result = executor
            .execute(&command, &CancellationToken::default())
            .unwrap();
        assert!(
            result.success,
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert!(
            root.join(".git").join(&object).is_file(),
            "original common object store receives the staged object"
        );
        assert!(
            !foreign.join(".git").join(&object).exists(),
            "foreign common object store remains untouched"
        );
    }

    #[test]
    fn captured_authority_refuses_redirected_separate_gitdir_with_replaced_original_name() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("worktree");
        let metadata = directory.path().join("metadata");
        let moved = directory.path().join("moved-metadata");
        let foreign = directory.path().join("foreign");
        std::fs::create_dir(&root).unwrap();
        std::fs::create_dir(&foreign).unwrap();
        actual_git(
            &root,
            &[
                "init",
                "-b",
                "main",
                "--separate-git-dir",
                metadata.to_str().unwrap(),
            ],
        );
        actual_git(&foreign, &["init", "-b", "main"]);
        let authority = WorktreeAuthority::capture(&root).unwrap();
        assert_eq!(authority.stable_private_metadata_path(), metadata);
        std::fs::rename(&metadata, &moved).unwrap();
        std::fs::write(root.join(".git"), format!("gitdir: {}\n", moved.display())).unwrap();
        std::fs::rename(foreign.join(".git"), &metadata).unwrap();
        assert!(
            authority.validate().is_err(),
            "durable metadata names must never name foreign directories even if the locator still resolves to the retained inode"
        );
    }

    #[test]
    fn captured_authority_prepared_child_command_survives_parent_authority_drop_and_fd_reuse() {
        #[derive(Default)]
        struct CaptureCommand(Mutex<Option<CommandSpec>>);
        impl ProcessExecutor for CaptureCommand {
            fn execute(
                &self,
                command: &CommandSpec,
                _: &CancellationToken,
            ) -> Result<ProcessOutput, ProcessFailure> {
                *self.0.lock().unwrap() = Some(command.clone());
                Ok(ProcessOutput {
                    exit_code: Some(0),
                    signal: None,
                    success: true,
                    stdout: vec![],
                    stdout_truncated: false,
                    stderr: vec![],
                })
            }
        }
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("worktree");
        let parked = directory.path().join("original");
        let foreign = directory.path().join("foreign");
        std::fs::create_dir(&root).unwrap();
        std::fs::create_dir(&foreign).unwrap();
        actual_git(&root, &["init", "-b", "main"]);
        actual_git(&foreign, &["init", "-b", "main"]);
        std::fs::write(root.join("new.txt"), "original\n").unwrap();
        std::fs::write(foreign.join("new.txt"), "foreign\n").unwrap();
        actual_git(&foreign, &["add", "new.txt"]);
        let foreign_index = std::fs::read(foreign.join(".git/index")).unwrap();
        let inner = Arc::new(CaptureCommand::default());
        let executor = WorktreeExecutor {
            inner: inner.clone(),
            authority: WorktreeAuthority::capture(&root).unwrap(),
        };
        executor
            .execute(
                &CommandSpec::new("git")
                    .arg("-C")
                    .arg(&root)
                    .args(["add", "--", "new.txt"]),
                &CancellationToken::default(),
            )
            .unwrap();
        let command = inner.0.lock().unwrap().take().unwrap();
        drop(executor);
        drop(inner);
        std::fs::rename(&root, &parked).unwrap();
        std::fs::rename(&foreign, &root).unwrap();
        // Reoccupy released parent descriptor numbers; only the invocation's
        // owned descriptor copies must remain authoritative in its child.
        let reused = (0..32)
            .map(|_| fs::File::open("/dev/null").unwrap())
            .collect::<Vec<_>>();
        let result = SystemProcessExecutor
            .execute(&isolated_spec(command), &CancellationToken::default())
            .unwrap();
        assert!(
            result.success,
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert_eq!(
            actual_git(&parked, &["ls-files", "-z"]).stdout,
            b"new.txt\0"
        );
        assert_eq!(
            std::fs::read(root.join(".git/index")).unwrap(),
            foreign_index
        );
        drop(reused);
    }
}

#[cfg(all(test, not(target_os = "linux")))]
mod portable_authority_tests {
    use super::*;
    #[test]
    fn unsupported_mutation_authority_fails_before_opening_a_proc_path() {
        assert!(!WorktreeAuthority::supported());
        let reason = WorktreeAuthority::unsupported_reason().unwrap();
        assert!(
            WorktreeAuthority::capture(Path::new("/nonexistent-worktree"))
                .unwrap_err()
                .to_string()
                .contains(reason)
        );
    }
}
