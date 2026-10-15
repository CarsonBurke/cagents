//! A delayed UI intent owns the physical worktree it originally displayed.

use super::*;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};

#[derive(Clone)]
pub struct WorktreeAuthority {
    root: Arc<fs_safety::Root>,
    identity: (u64, u64),
    git_dir: Arc<fs_safety::Root>,
    common_dir: Arc<fs_safety::Root>,
}

impl fmt::Debug for WorktreeAuthority {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("WorktreeAuthority").field("path", &self.path()).field("identity", &self.identity).finish()
    }
}

impl WorktreeAuthority {
    /// Strong Git mutation authority currently uses Linux's parent-owned
    /// descriptor namespace. Basic Git reads and ordinary file editing do not
    /// require this capability and must not substitute an unpinned mutation.
    pub const fn supported() -> bool { cfg!(target_os = "linux") }

    pub const fn unsupported_reason() -> Option<&'static str> {
        if Self::supported() { None } else { Some("Git mutations are currently supported on Linux only. You can still edit files and inspect Git changes.") }
    }

    pub fn capture(path: &Path) -> Result<Self, RepositoryError> {
        if let Some(reason) = Self::unsupported_reason() { return Err(ValidationError::Io(reason.into()).into()); }
        if !path.is_absolute() { return Err(ValidationError::InvalidWorktree(path.display().to_string()).into()); }
        let root = fs_safety::Root::open(path).map_err(|error| ValidationError::Io(error.to_string()))?;
        let metadata = root.metadata().map_err(|error| ValidationError::Io(error.to_string()))?;
        let git_dir = open_metadata_root(&resolve_private_git_dir(&descriptor_path(&root))?)?;
        let common_dir = open_metadata_root(&resolve_common_git_dir(&descriptor_path(&git_dir))?)?;
        let authority = Self { root: Arc::new(root), identity: (metadata.dev(), metadata.ino()), git_dir: Arc::new(git_dir), common_dir: Arc::new(common_dir) };
        authority.validate()?;
        Ok(authority)
    }

    pub fn path(&self) -> &Path { self.root.path() }

    pub fn validate(&self) -> Result<(), RepositoryError> {
        if let Some(reason) = Self::unsupported_reason() { return Err(ValidationError::Io(reason.into()).into()); }
        let named = fs_safety::Root::open(self.path()).map_err(|error| ValidationError::Io(format!("The original worktree is no longer available: {error}")))?;
        let metadata = named.metadata().map_err(|error| ValidationError::Io(error.to_string()))?;
        if (metadata.dev(), metadata.ino()) != self.identity {
            return Err(ValidationError::Io("The original worktree directory was replaced; no action was applied to the replacement".into()).into());
        }
        let named_private = open_metadata_root(self.git_dir.path())?;
        let named_common = open_metadata_root(self.common_dir.path())?;
        if !same_directory(&named_private, &self.git_dir)? || !same_directory(&named_common, &self.common_dir)? {
            return Err(ValidationError::Io("The original Git metadata directory name was replaced; reopen the worktree before retrying".into()).into());
        }
        let private = open_metadata_root(&resolve_private_git_dir(&self.anchored_path())?)?;
        let common = open_metadata_root(&resolve_common_git_dir(&descriptor_path(&self.git_dir))?)?;
        if !same_directory(&private, &self.git_dir)? || !same_directory(&common, &self.common_dir)? {
            return Err(ValidationError::Io("The original Git metadata was replaced or redirected; no action was applied to the replacement".into()).into());
        }
        Ok(())
    }

    /// Journal code can retain the exact source roots without rediscovering
    /// metadata through a changed path. No executor is exposed here.
    pub(super) fn retained_roots(&self) -> Result<(fs_safety::Root, fs_safety::Root, fs_safety::Root), RepositoryError> {
        self.validate()?;
        let clone_root = |root: &fs_safety::Root| -> Result<fs_safety::Root, RepositoryError> { root.try_clone().map_err(|error| ValidationError::Io(error.to_string()).into()) };
        Ok((clone_root(&self.root)?, clone_root(&self.git_dir)?, clone_root(&self.common_dir)?))
    }

    /// A creation journal first verifies its owned registration, backlinks and
    /// publication; this constructor additionally verifies the held roots still
    /// match the worktree's named metadata before publishing destination scope.
    pub(super) fn from_retained_roots(root: fs_safety::Root, git_dir: fs_safety::Root, common_dir: fs_safety::Root) -> Result<Self, RepositoryError> {
        let metadata = root.metadata().map_err(|error| ValidationError::Io(error.to_string()))?;
        let authority = Self { root: Arc::new(root), identity: (metadata.dev(), metadata.ino()), git_dir: Arc::new(git_dir), common_dir: Arc::new(common_dir) };
        authority.validate()?;
        Ok(authority)
    }

    pub(super) fn stable_private_metadata_path(&self) -> &Path { self.git_dir.path() }
    pub(super) fn stable_common_metadata_path(&self) -> &Path { self.common_dir.path() }

    pub(super) fn private_metadata_path(&self) -> PathBuf { descriptor_path(&self.git_dir) }
    pub(super) fn common_metadata_path(&self) -> PathBuf { descriptor_path(&self.common_dir) }

    fn anchored_path(&self) -> PathBuf {
        // The executor owns the descriptor throughout execute, including any
        // blocking launch delay. A path replacement cannot redirect Git -C.
        descriptor_path(&self.root)
    }
}

fn descriptor_path(root: &fs_safety::Root) -> PathBuf {
    PathBuf::from(format!("/proc/{}/fd/{}/.", std::process::id(), root.descriptor()))
}

fn metadata_error(error: io::Error) -> RepositoryError { ValidationError::Io(error.to_string()).into() }

fn open_metadata_root(path: &Path) -> Result<fs_safety::Root, RepositoryError> {
    fs_safety::Root::open(path).map_err(metadata_error)
}

fn same_directory(left: &fs_safety::Root, right: &fs_safety::Root) -> Result<bool, RepositoryError> {
    let left = left.metadata().map_err(metadata_error)?;
    let right = right.metadata().map_err(metadata_error)?;
    Ok((left.dev(), left.ino()) == (right.dev(), right.ino()))
}

/// Bounded regular-file reads cannot hang on a replaced FIFO or follow a new
/// symlink. Git's raw pathname bytes, including non-UTF8 paths, are preserved.
fn read_locator(path: &Path) -> Result<Vec<u8>, RepositoryError> {
    let file = fs::OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK).open(path).map_err(metadata_error)?;
    if !file.metadata().map_err(metadata_error)?.is_file() { return Err(ValidationError::Io("Git metadata locator is not a regular file".into()).into()); }
    let mut bytes = Vec::new();
    file.take(8193).read_to_end(&mut bytes).map_err(metadata_error)?;
    if bytes.len() > 8192 { return Err(ValidationError::Io("Git metadata locator is too large".into()).into()); }
    Ok(bytes)
}

fn resolve_private_git_dir(root: &Path) -> Result<PathBuf, RepositoryError> {
    let marker = root.join(".git");
    let path = if fs::metadata(&marker).map_err(metadata_error)?.is_dir() { marker }
    else {
        let bytes = read_locator(&marker)?;
        let relative = bytes.strip_prefix(b"gitdir: ").ok_or_else(|| ValidationError::Io("Invalid Git worktree locator".into()))?;
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

impl<E> Clone for WorktreeExecutor<E> {
    fn clone(&self) -> Self { Self { inner: self.inner.clone(), authority: self.authority.clone() } }
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
            let argument = scoped.arguments[index].to_string_lossy().into_owned();
            if argument == "--" || !argument.starts_with('-') { break; }
            match argument.as_ref() {
                "-C" => {
                    let Some(path) = scoped.arguments.get_mut(index + 1) else { return Err(refuse("Missing worktree argument".into())); };
                    if Path::new(path.as_os_str()) != self.authority.path() { return Err(refuse("The command no longer names the captured worktree".into())); }
                    *path = self.authority.anchored_path().into_os_string();
                    anchored = true;
                    index += 2;
                }
                // Values may themselves be '-C'. They are not global options.
                "-c" | "--config-env" => {
                    if index + 1 >= scoped.arguments.len() { return Err(refuse("Missing Git configuration argument".into())); }
                    index += 2;
                }
                "--git-dir" | "--work-tree" => return Err(refuse("A captured worktree cannot be overridden by another Git scope".into())),
                value if value.starts_with("--git-dir=") || value.starts_with("--work-tree=") => return Err(refuse("A captured worktree cannot be overridden by another Git scope".into())),
                _ => index += 1,
            }
        }
        if let Some(path) = scoped.current_dir.as_mut() {
            if path.as_path() != self.authority.path() { return Err(refuse("The command no longer names the captured worktree".into())); }
            *path = self.authority.anchored_path();
            anchored = true;
        }
        if !anchored { return Err(refuse("The command has no captured worktree scope".into())); }
        // Explicit scopes prevent Git from rediscovering .git/commondir after
        // validation, including a delayed launch inside the executor.
        scoped.arguments.splice(0..0, [
            OsString::from(format!("--git-dir={}", self.authority.private_metadata_path().display())),
            OsString::from(format!("--work-tree={}", self.authority.anchored_path().display())),
        ]);
        scoped.environment.insert(OsString::from("GIT_COMMON_DIR"), self.authority.common_metadata_path().into_os_string());
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
                if let Ok(metadata_relative) = relative.strip_prefix(".git")
                    && !metadata_relative.as_os_str().is_empty()
                { return Ok(authority.private_metadata_path().join(metadata_relative)); }
                return Ok(authority.anchored_path().join(relative));
            }
        }
        Ok(path.to_path_buf())
    }

    pub(super) fn git_dir_for_io(&self, worktree: &Path, cancellation: &CancellationToken) -> Result<PathBuf, RepositoryError> {
        if let Some(authority) = &self.authority {
            if authority.path() != worktree { return Err(ValidationError::InvalidWorktree(worktree.display().to_string()).into()); }
            authority.validate()?;
            return Ok(authority.private_metadata_path());
        }
        self.git_dir(worktree, cancellation)
    }

    pub(super) fn common_dir_for_io(&self, worktree: &Path, cancellation: &CancellationToken) -> Result<PathBuf, RepositoryError> {
        if let Some(authority) = &self.authority {
            if authority.path() != worktree { return Err(ValidationError::InvalidWorktree(worktree.display().to_string()).into()); }
            authority.validate()?;
            return Ok(authority.common_metadata_path());
        }
        self.common_dir(worktree, cancellation)
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

#[cfg(test)]
mod authority_tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    struct RecordingExecutor(Mutex<Vec<CommandSpec>>);
    impl ProcessExecutor for RecordingExecutor {
        fn execute(&self, command: &CommandSpec, _: &CancellationToken) -> Result<ProcessOutput, ProcessFailure> {
            self.0.lock().unwrap().push(command.clone());
            Ok(ProcessOutput { exit_code: Some(0), signal: None, success: true, stdout: vec![], stdout_truncated: false, stderr: vec![] })
        }
    }

    #[test]
    fn captured_authority_rewrites_only_global_worktree_scope_and_preserves_literal_c_data() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::create_dir(directory.path().join(".git")).unwrap();
        let authority = WorktreeAuthority::capture(directory.path()).unwrap();
        let inner = Arc::new(RecordingExecutor::default());
        let executor = WorktreeExecutor { inner: inner.clone(), authority: authority.clone() };
        for tail in [vec!["diff", "--", "-C"], vec!["log", "--grep", "-C", "--", "file"], vec!["-c", "example.value=-C", "log", "--grep=-C"]] {
            let command = CommandSpec::new("git").args([std::ffi::OsStr::new("-C"), directory.path().as_os_str()]).args(tail.iter().copied());
            executor.execute(&command, &CancellationToken::default()).unwrap();
            let commands = inner.0.lock().unwrap();
            let scoped = commands.last().unwrap();
            assert_eq!(scoped.arguments[2], "-C");
            assert_eq!(Path::new(&scoped.arguments[3]), authority.anchored_path());
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
        let executor = WorktreeExecutor { inner: inner.clone(), authority };
        std::fs::rename(&root, parent.path().join("original")).unwrap();
        std::fs::create_dir(&root).unwrap();
        let command = CommandSpec::new("git").args([std::ffi::OsStr::new("-C"), root.as_os_str(), std::ffi::OsStr::new("add"), std::ffi::OsStr::new("--all")]);
        assert!(executor.execute(&command, &CancellationToken::default()).is_err());
        assert!(inner.0.lock().unwrap().is_empty());
    }
    fn isolated_spec(mut command: CommandSpec) -> CommandSpec {
        for (name, _) in std::env::vars_os().filter(|(name, _)| name.as_encoded_bytes().starts_with(b"GIT_")) { command = command.env_remove(name); }
        command.env("GIT_CONFIG_GLOBAL", "/dev/null").env("GIT_CONFIG_SYSTEM", "/dev/null").env("GIT_CONFIG_NOSYSTEM", "1")
    }

    fn actual_git(root: &Path, arguments: &[&str]) -> ProcessOutput {
        let command = isolated_spec(CommandSpec::new("git").args(["-c", "core.fsmonitor=false", "-c", "core.hooksPath=/dev/null", "-c", "init.templateDir="]).arg("-C").arg(root).args(arguments));
        let output = SystemProcessExecutor.execute(&command, &CancellationToken::default()).unwrap();
        assert!(output.success, "{}", String::from_utf8_lossy(&output.stderr));
        output
    }

    #[test]
    fn captured_authority_actual_git_reads_and_stages_literal_c_filename() {
        struct Isolated;
        impl ProcessExecutor for Isolated {
            fn execute(&self, command: &CommandSpec, cancel: &CancellationToken) -> Result<ProcessOutput, ProcessFailure> {
                SystemProcessExecutor.execute(&isolated_spec(command.clone()), cancel)
            }
        }
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        actual_git(root, &["init", "-b", "main"]);
        actual_git(root, &["config", "core.fsmonitor", "false"]);
        actual_git(root, &["config", "core.hooksPath", "/dev/null"]);
        std::fs::write(root.join("-C"), "original\n").unwrap();
        let service = RepositoryService::new("git", Arc::new(Isolated)).with_worktree_authority(WorktreeAuthority::capture(root).unwrap()).unwrap();
        let cancel = CancellationToken::default();
        assert_eq!(service.status(root, &cancel).unwrap().entries.len(), 1);
        assert!(matches!(service.stage(&StageRequest { worktree: root.to_owned(), selection: vec![PathSelection::new(RepoPath::parse("-C").unwrap())] }, &cancel).unwrap(), MutationOutcome::Applied { .. }));
        let status = service.status(root, &cancel).unwrap();
        assert_eq!(status.entries[0].path, RepoPath::parse("-C").unwrap());
        assert_eq!(status.entries[0].area, ChangeArea::Staged);
    }

    #[test]
    fn captured_authority_pins_actual_git_effects_across_inner_launch_delay() {
        struct ReplaceBeforeLaunch { root: PathBuf, parked: PathBuf }
        impl ProcessExecutor for ReplaceBeforeLaunch {
            fn execute(&self, command: &CommandSpec, cancel: &CancellationToken) -> Result<ProcessOutput, ProcessFailure> {
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
        let executor = WorktreeExecutor { inner: Arc::new(ReplaceBeforeLaunch { root: root.clone(), parked: parked.clone() }), authority: WorktreeAuthority::capture(&root).unwrap() };
        let command = CommandSpec::new("git").arg("-C").arg(&root).args(["--literal-pathspecs", "add", "--", "-C"]);
        let result = executor.execute(&command, &CancellationToken::default()).unwrap();
        assert!(result.success, "{}", String::from_utf8_lossy(&result.stderr));
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
        std::fs::write(root.join(".git/commondir"), foreign.join(".git").as_os_str().as_encoded_bytes()).unwrap();
        assert!(authority.validate().is_err());
    }

    #[test]
    fn captured_authority_pins_private_metadata_after_validation_before_actual_git_launch() {
        struct ReplaceMetadata { root: PathBuf, foreign: PathBuf, parked: PathBuf }
        impl ProcessExecutor for ReplaceMetadata {
            fn execute(&self, command: &CommandSpec, cancel: &CancellationToken) -> Result<ProcessOutput, ProcessFailure> {
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
        let executor = WorktreeExecutor { inner: Arc::new(ReplaceMetadata { root: root.clone(), foreign: foreign.clone(), parked: parked.clone() }), authority: WorktreeAuthority::capture(&root).unwrap() };
        let command = CommandSpec::new("git").arg("-C").arg(&root).args(["add", "--", "new.txt"]);
        let result = executor.execute(&command, &CancellationToken::default()).unwrap();
        assert!(result.success, "{}", String::from_utf8_lossy(&result.stderr));
        assert!(parked.join("index").is_file(), "original retained metadata receives the stage");
        assert_eq!(std::fs::read(root.join(".git/index")).unwrap(), foreign_index, "foreign metadata is untouched despite occupying .git at launch");
    }

    #[test]
    fn captured_authority_pins_common_object_store_after_validation_before_actual_git_launch() {
        struct RedirectCommon { root: PathBuf, foreign: PathBuf }
        impl ProcessExecutor for RedirectCommon {
            fn execute(&self, command: &CommandSpec, cancel: &CancellationToken) -> Result<ProcessOutput, ProcessFailure> {
                std::fs::write(self.root.join(".git/commondir"), self.foreign.join(".git").as_os_str().as_encoded_bytes()).unwrap();
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
        let executor = WorktreeExecutor { inner: Arc::new(RedirectCommon { root: root.clone(), foreign: foreign.clone() }), authority: WorktreeAuthority::capture(&root).unwrap() };
        let command = CommandSpec::new("git").arg("-C").arg(&root).args(["add", "--", "new.txt"]);
        let result = executor.execute(&command, &CancellationToken::default()).unwrap();
        assert!(result.success, "{}", String::from_utf8_lossy(&result.stderr));
        assert!(root.join(".git").join(&object).is_file(), "original common object store receives the staged object");
        assert!(!foreign.join(".git").join(&object).exists(), "foreign common object store remains untouched");
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
        actual_git(&root, &["init", "-b", "main", "--separate-git-dir", metadata.to_str().unwrap()]);
        actual_git(&foreign, &["init", "-b", "main"]);
        let authority = WorktreeAuthority::capture(&root).unwrap();
        assert_eq!(authority.stable_private_metadata_path(), metadata);
        std::fs::rename(&metadata, &moved).unwrap();
        std::fs::write(root.join(".git"), format!("gitdir: {}\n", moved.display())).unwrap();
        std::fs::rename(foreign.join(".git"), &metadata).unwrap();
        assert!(authority.validate().is_err(), "durable metadata names must never name foreign directories even if the locator still resolves to the retained inode");
    }

}
