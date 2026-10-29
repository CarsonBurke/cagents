//! Fixed object-store reads/materialization using the original captured roots.
//! Git never receives a destination pathname for streamed blob contents.
use super::scope::{child_directory_path, metadata_error};
use super::*;
use std::os::unix::fs::MetadataExt;

enum RecoveryObjectCommand<'a> {
    Stream {
        oid: &'a Oid,
        file: &'a Arc<fs::File>,
        max_bytes: u64,
    },
    Hash(&'a Arc<fs::File>),
}

impl WorktreeAuthority {
    // Three retained Root descriptors plus the child scope descriptors; primary
    // private/common share the same child Arc and are counted once. Pure/O(1).
    pub(in crate::repository) fn recovery_descriptor_count(&self) -> usize {
        3 + if Arc::ptr_eq(&self.child_git, &self.child_common) {
            2
        } else {
            3
        }
    }
}

impl<E: ProcessExecutor> RepositoryService<E> {
    /// Retain the original object-store authority for one recovery operation.
    /// Recovery requires the original retained metadata authority. A later
    /// pathname capture cannot reconstruct the repository the user approved.
    pub(in crate::repository) fn recovery_source_authority(
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
    pub(in crate::repository) fn stream_recovery_object(
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

    pub(in crate::repository) fn hash_recovery_object(
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
}
