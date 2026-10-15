//! Recoverable discard receipts. Working contents live in Git's object store;
//! untracked entries remain visible in the desktop's freedesktop Trash.

use super::*;
use std::{
    io::Write,
    os::unix::{
        ffi::OsStrExt,
        fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    },
};

#[path = "repository/recovery_publication.rs"]
mod publication;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SavedEntry {
    Absent,
    Trashed,
    Unavailable { reason: String },
    Blob { oid: Oid, mode: u32, bytes: u64 },
    Symlink { target: PathBuf },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrashReceipt {
    pub warning: Option<String>,
    pub path: RepoPath,
    pub file: PathBuf,
    pub info: PathBuf,
    pub version: Option<(u64, u64, u64, i64, i64, i64, i64)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscardRecoveryEntry {
    pub path: RepoPath,
    pub before: SavedEntry,
    pub after: SavedEntry,
    pub after_version: Option<(u64, u64, u64, i64, i64, i64, i64)>,
    pub trash: Option<TrashReceipt>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscardRecovery {
    /// Ephemeral exact recovery authority; never serialized or reconstructed
    /// from a diagnostic pathname. Clones share the same bounded payload.
    pub live_recovery: LiveDiscardRecovery,
    pub worktree: PathBuf,
    pub worktree_identity: (u64, u64),
    pub directories: BTreeMap<RepoPath, u32>,
    pub entries: Vec<DiscardRecoveryEntry>,
    pub expires_unix_ms: u64,
}

/// Keeps unresolved worker-owned receipts with the actual discard receipt.
/// Mutex guards never cover filesystem IO: clone then retry on the worker.
#[derive(Debug, Clone, Default)]
pub struct LiveDiscardRecovery(Arc<std::sync::Mutex<LiveDiscardState>>);
#[derive(Debug, Default)]
struct LiveDiscardState {
    authority: Option<crate::recovery_authority::RecoveryAuthority>,
    uncertain: bool,
    completed: std::collections::BTreeSet<RepoPath>,
    resolution_required: bool,
    resumed_until_unix_ms: u64,
}
impl LiveDiscardRecovery {
    pub fn authority(&self) -> Option<crate::recovery_authority::RecoveryAuthority> {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .authority
            .clone()
    }
    pub fn is_pending(&self) -> bool {
        self.authority()
            .is_some_and(|authority| !authority.is_resolved())
    }
    pub fn needs_resolution(&self) -> bool {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .resolution_required
    }
    fn is_completed(&self, path: &RepoPath) -> bool {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .completed
            .contains(path)
    }
    fn completed_count(&self) -> usize {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .completed
            .len()
    }
    fn mark_completed(&self, path: &RepoPath) {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .completed
            .insert(path.clone());
    }
    fn resume_deadline(&self) -> u64 {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .resumed_until_unix_ms
    }
    fn acknowledge_resolution(&self) {
        let mut state = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.uncertain = false;
        state.resolution_required = false;
        state.resumed_until_unix_ms = unix_ms().saturating_add(60_000);
    }
    fn set_uncertain(&self, uncertain: bool) {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .uncertain = uncertain;
    }
    fn is_uncertain(&self) -> bool {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .uncertain
    }
    fn retain(&self, authority: crate::recovery_authority::RecoveryAuthority) {
        let mut current = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        current.resolution_required = true;
        current.authority = Some(match current.authority.take() {
            Some(previous) => previous.combine(authority),
            None => authority,
        });
    }
}
impl PartialEq for LiveDiscardRecovery {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0) || self.authority() == other.authority()
    }
}
impl Eq for LiveDiscardRecovery {}

impl DiscardRecovery {
    pub fn can_undo(&self) -> bool {
        self.live_recovery.needs_resolution()
            || (self.remaining_paths() > 0
                && unix_ms()
                    <= self
                        .expires_unix_ms
                        .max(self.live_recovery.resume_deadline()))
    }
    /// Pending cleanup has a separate UI control and no expiry timer. A
    /// successful typed resolution resumes the remaining Undo window.
    pub fn undo_deadline_unix_ms(&self) -> Option<u64> {
        (!self.live_recovery.needs_resolution()).then(|| {
            self.expires_unix_ms
                .max(self.live_recovery.resume_deadline())
        })
    }
    pub fn remaining_paths(&self) -> usize {
        self.entries
            .len()
            .saturating_sub(self.live_recovery.completed_count())
    }
}

/// Cleanup completion is distinct from a file Undo. The same receipt retains
/// every unattempted path; callers keep it when remaining_paths is nonzero.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiscardRecoveryResolution {
    pub remaining_paths: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoverableDiscard {
    pub outcome: MutationOutcome,
    pub recovery: DiscardRecovery,
}

fn unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64
}

impl<E: ProcessExecutor> RepositoryService<E> {
    /// Keep a receipt for Undo; all original index-lock, path and partial-result
    /// protections remain in discard_mutation.
    pub fn discard_recoverable(
        &self,
        request: &DiscardRequest,
        cancellation: &CancellationToken,
    ) -> Result<RecoverableDiscard, RepositoryError> {
        if !request.confirmed {
            return Err(ValidationError::ConfirmationRequired.into());
        }
        if !request.worktree.is_absolute() {
            return Err(
                ValidationError::InvalidWorktree(request.worktree.display().to_string()).into(),
            );
        }
        for path in selection_pathspecs(&request.tracked)
            .into_iter()
            .chain(request.untracked.iter().cloned())
        {
            if path.as_path().components().any(|component| {
                component
                    .as_os_str()
                    .as_bytes()
                    .eq_ignore_ascii_case(b".git")
            }) {
                return Err(ValidationError::Io(
                    "Git administrative paths cannot be discarded".into(),
                )
                .into());
            }
        }
        let _protection = crate::worktree_cleanup::protect(&request.worktree)
            .map_err(|error| ValidationError::Io(error.to_string()))?;
        let root = self.captured_worktree_root(&request.worktree)?;
        let directory = root
            .metadata()
            .map_err(|error| ValidationError::Io(error.to_string()))?;
        let worktree_identity = (directory.dev(), directory.ino());
        let tracked = selection_pathspecs(&request.tracked);
        let directories = parent_modes(&root, tracked.iter().chain(&request.untracked));
        let mut before = BTreeMap::new();
        let (captured, versions) =
            self.capture_recovery_entries(&request.worktree, &root, &tracked, true, cancellation);
        for (path, result) in captured {
            let saved = result.map_err(|reason| {
                ValidationError::Io(format!("Cannot back up {path}: {reason}"))
            })?;
            before.insert(path, saved);
        }
        let mut trashed = Vec::new();
        let outcome =
            self.discard_mutation(request, cancellation, &mut trashed, &root, &versions)?;
        let mut entries = if matches!(outcome, MutationOutcome::NotApplied { .. }) {
            Vec::new()
        } else {
            let (after, _) = self.capture_recovery_entries(
                &request.worktree,
                &root,
                &tracked,
                false,
                &CancellationToken::new(),
            );
            changed_recovery_entries(&root, before, &after)
        };
        // The atomic Trash move itself preserves untracked contents and modes.
        // Its receipt pins the post-move inode/version, so no per-file Git hash
        // subprocess or duplicate contents buffer is needed for untracked work.
        entries.extend(trashed.into_iter().map(|trash| DiscardRecoveryEntry {
            path: trash.path.clone(),
            before: SavedEntry::Trashed,
            after: SavedEntry::Absent,
            after_version: None,
            trash: Some(trash),
        }));
        // Every directory considered is an ancestor of an actually removed
        // file, and the held worktree root is never itself considered.
        for entry in &entries {
            if entry.after == SavedEntry::Absent {
                prune_empty_parents(&root, &entry.path);
            }
        }
        Ok(RecoverableDiscard {
            outcome,
            recovery: DiscardRecovery {
                live_recovery: LiveDiscardRecovery::default(),
                worktree: request.worktree.clone(),
                worktree_identity,
                directories,
                entries,
                expires_unix_ms: unix_ms().saturating_add(60_000),
            },
        })
    }

    /// Worktree hunk reversal needs the same Undo receipt as whole-file
    /// discard, including new/deleted files and both endpoints of a rename.
    pub fn apply_patch_recoverable(
        &self,
        worktree: &Path,
        patch: &[u8],
        reverse: bool,
        expected_head: Option<&GitHeadGuard>,
        cancellation: &CancellationToken,
    ) -> Result<RecoverableDiscard, RepositoryError> {
        if !worktree.is_absolute() {
            return Err(ValidationError::InvalidWorktree(worktree.display().to_string()).into());
        }
        let _protection = crate::worktree_cleanup::protect(worktree)
            .map_err(|error| ValidationError::Io(error.to_string()))?;
        let root = self.captured_worktree_root(worktree)?;
        let directory = root
            .metadata()
            .map_err(|error| ValidationError::Io(error.to_string()))?;
        let worktree_identity = (directory.dev(), directory.ino());
        let mut paths = Vec::new();
        let mut seen = HashSet::new();
        // apply --numstat reports only the destination of a rename. Reading
        // both orientations captures both endpoints with Git's own NUL parser.
        for reversed in [false, true] {
            let mut command = self.git_read(worktree, ["apply", "--numstat", "-z"]);
            if reversed {
                command = command.arg("--reverse");
            }
            let output = self.run_checked(&command.arg("-").stdin(patch.to_vec()), cancellation)?;
            let records = invalid_output(parse_numstat(&output.stdout), output)?;
            for record in records {
                for path in std::iter::once(record.path).chain(record.old_path) {
                    if path.as_path().components().any(|component| {
                        component
                            .as_os_str()
                            .as_bytes()
                            .eq_ignore_ascii_case(b".git")
                    }) {
                        return Err(ValidationError::Io(
                            "Git administrative paths cannot be patched".into(),
                        )
                        .into());
                    }
                    if seen.insert(path.clone()) {
                        paths.push(path);
                    }
                }
            }
        }
        let directories = parent_modes(&root, paths.iter());
        let mut before = BTreeMap::new();
        let (captured, versions) =
            self.capture_recovery_entries(worktree, &root, &paths, true, cancellation);
        for (path, result) in captured {
            before.insert(
                path.clone(),
                result.map_err(|reason| {
                    ValidationError::Io(format!("Cannot back up {path}: {reason}"))
                })?,
            );
        }
        // Resolve HEAD before the final file fence. No additional Git read
        // may sit between that fence and the destructive apply subprocess.
        let head_matches = match expected_head {
            Some(GitHeadGuard::Unborn) => self.resolve_head(worktree, cancellation)?.is_none(),
            Some(GitHeadGuard::Commit(expected)) => {
                self.resolve_head(worktree, cancellation)?.as_ref() == Some(expected)
            }
            None => true,
        };
        let outcome = if !head_matches {
            MutationOutcome::NotApplied { failure: MutationFailure::Rejected { reason: "HEAD changed after this patch was prepared; reload the diff before applying it".into() } }
        } else if let Err(reason) = verify_captured_versions(&root, &versions, cancellation) {
            MutationOutcome::NotApplied {
                failure: MutationFailure::Rejected { reason },
            }
        } else {
            let mut command = self.git_mutation(worktree, ["apply", "--whitespace=nowarn"]);
            if reverse {
                command = command.arg("--reverse");
            }
            let command = command.arg("-").stdin(patch.to_vec()).without_timeout();
            self.operation_failure(&command, cancellation)?
                .unwrap_or(MutationOutcome::Applied { paths: Vec::new() })
        };
        let entries = if matches!(outcome, MutationOutcome::NotApplied { .. }) {
            Vec::new()
        } else {
            let (after, _) = self.capture_recovery_entries(
                worktree,
                &root,
                &paths,
                false,
                &CancellationToken::new(),
            );
            changed_recovery_entries(&root, before, &after)
        };
        for entry in &entries {
            if entry.after == SavedEntry::Absent {
                prune_empty_parents(&root, &entry.path);
            }
        }
        Ok(RecoverableDiscard {
            outcome,
            recovery: DiscardRecovery {
                live_recovery: LiveDiscardRecovery::default(),
                worktree: worktree.to_owned(),
                worktree_identity,
                directories,
                entries,
                expires_unix_ms: unix_ms().saturating_add(60_000),
            },
        })
    }

    /// Hash pinned regular-file descriptors in bounded batches. A mass discard
    /// uses one Git invocation per256 files rather than one per selected path.
    fn capture_recovery_entries(
        &self,
        worktree: &Path,
        root: &fs_safety::Root,
        paths: &[RepoPath],
        store: bool,
        cancellation: &CancellationToken,
    ) -> (
        BTreeMap<RepoPath, Result<SavedEntry, String>>,
        CaptureVersions,
    ) {
        let mut result = BTreeMap::new();
        let mut versions = BTreeMap::new();
        for batch in paths.chunks(256) {
            let mut files = Vec::new();
            for path in batch {
                let metadata = match fs_safety::metadata_beneath(root, path.as_path()) {
                    Ok(metadata) => metadata,
                    Err(error) if error.is_not_found() => {
                        result.insert(path.clone(), Ok(SavedEntry::Absent));
                        versions.insert(path.clone(), None);
                        continue;
                    }
                    Err(error) => {
                        result.insert(path.clone(), Err(error.to_string()));
                        continue;
                    }
                };
                if !metadata.is_file() {
                    let saved = self
                        .capture_recovery_entry(worktree, root, path, store, cancellation)
                        .and_then(|saved| {
                            let after = fs_safety::metadata_beneath(root, path.as_path())
                                .map_err(|error| error.to_string())?;
                            if metadata_version(&metadata) != metadata_version(&after) {
                                return Err("file changed while making its backup".into());
                            }
                            versions.insert(path.clone(), Some(metadata_version(&after)));
                            Ok(saved)
                        });
                    result.insert(path.clone(), saved);
                    continue;
                }
                match fs_safety::open_beneath(root, path.as_path()).and_then(|file| {
                    let opened = fs_safety::metadata_of(&file).map_err(|error| {
                        fs_safety::ResolveError::io(&root.path().join(path.as_path()), &error)
                    })?;
                    if metadata_version(&metadata) != metadata_version(&opened) {
                        return Err(fs_safety::ResolveError::io(
                            &root.path().join(path.as_path()),
                            &io::Error::other("file changed while opening its backup"),
                        ));
                    }
                    Ok((file, opened))
                }) {
                    Ok((file, opened)) => files.push((path.clone(), file, opened)),
                    Err(error) => {
                        result.insert(path.clone(), Err(error.to_string()));
                    }
                }
            }
            if files.is_empty() {
                continue;
            }
            let mut input = Vec::new();
            for (_, file, _) in &files {
                input.extend_from_slice(
                    format!("/proc/{}/fd/{}\n", std::process::id(), file.as_raw_fd()).as_bytes(),
                );
            }
            let mut command =
                self.git_mutation(worktree, ["hash-object", "--no-filters", "--stdin-paths"]);
            if store {
                command = command.arg("-w");
            }
            let output = self
                .run_checked(&command.stdin(input), cancellation)
                .map_err(|error| error.to_string());
            let hashes = output.as_ref().map(|output| {
                output
                    .stdout
                    .split(|byte| *byte == b'\n')
                    .filter(|line| !line.is_empty())
                    .collect::<Vec<_>>()
            });
            for (index, (path, file, opened)) in files.iter().enumerate() {
                let saved = (|| {
                    let hashes = hashes.as_ref().map_err(|reason| reason.to_string())?;
                    if hashes.len() != files.len() {
                        return Err("Git did not return a hash for every backup".into());
                    }
                    let after = file.metadata().map_err(|error| error.to_string())?;
                    if metadata_version(opened) != metadata_version(&after) {
                        return Err("file changed while making its backup".into());
                    }
                    let oid =
                        Oid::parse_object(String::from_utf8_lossy(hashes[index]).into_owned())
                            .map_err(|error| error.to_string())?;
                    Ok(SavedEntry::Blob {
                        oid,
                        mode: opened.mode() & 0o7777,
                        bytes: opened.len(),
                    })
                })();
                if saved.is_ok() {
                    versions.insert(path.clone(), Some(metadata_version(opened)));
                }
                result.insert(path.clone(), saved);
            }
        }
        (result, versions)
    }

    fn capture_recovery_entry(
        &self,
        worktree: &Path,
        root: &fs_safety::Root,
        path: &RepoPath,
        store: bool,
        cancellation: &CancellationToken,
    ) -> Result<SavedEntry, String> {
        let metadata = match fs_safety::metadata_beneath(root, path.as_path()) {
            Ok(metadata) => metadata,
            Err(error) if error.is_not_found() => return Ok(SavedEntry::Absent),
            Err(error) => return Err(error.to_string()),
        };
        if metadata.file_type().is_symlink() {
            let target = fs_safety::read_link_beneath(root, path.as_path())
                .map_err(|error| error.to_string())?;
            if store {
                self.run_checked(
                    &self
                        .git_mutation(worktree, ["hash-object", "-w", "--stdin"])
                        .stdin(target.as_os_str().as_bytes().to_vec()),
                    cancellation,
                )
                .map_err(|error| error.to_string())?;
            }
            return Ok(SavedEntry::Symlink { target });
        }
        if !metadata.is_file() {
            return Err("only regular files and symlinks can be backed up".into());
        }
        let file =
            fs_safety::open_beneath(root, path.as_path()).map_err(|error| error.to_string())?;
        let opened = file.metadata().map_err(|error| error.to_string())?;
        if (metadata.dev(), metadata.ino()) != (opened.dev(), opened.ino()) {
            return Err("file changed while opening its backup".into());
        }
        // The descriptor path pins the opened file rather than resolving its
        // worktree name again, and avoids allocating the whole file on stdin.
        let descriptor_path = format!("/proc/{}/fd/{}", std::process::id(), file.as_raw_fd());
        let mut command = self.git_mutation(worktree, ["hash-object", "--no-filters"]);
        if store {
            command = command.arg("-w");
        }
        let output = self
            .run_checked(&command.arg("--").arg(descriptor_path), cancellation)
            .map_err(|error| error.to_string())?;
        let after = file.metadata().map_err(|error| error.to_string())?;
        if metadata_version(&opened) != metadata_version(&after) {
            return Err("file changed while making its backup".into());
        }
        let oid = Oid::parse_object(String::from_utf8_lossy(&output.stdout).trim())
            .map_err(|error| error.to_string())?;
        Ok(SavedEntry::Blob {
            oid,
            mode: opened.mode() & 0o7777,
            bytes: opened.len(),
        })
    }

    /// Resolves only retained artifacts/metadata. This does NOT perform the
    /// remaining file Undo or consume its intent, and needs no pathname Git
    /// recapture. Run on a worker and keep the original receipt on failure.
    pub fn resolve_discard_recovery(
        &self,
        recovery: &DiscardRecovery,
        cancellation: &CancellationToken,
    ) -> Result<DiscardRecoveryResolution, RepositoryError> {
        if cancellation.is_cancelled() {
            return Err(
                ValidationError::Io("Discard recovery was cancelled before retry".into()).into(),
            );
        }
        if !recovery.live_recovery.needs_resolution() {
            return Ok(DiscardRecoveryResolution {
                remaining_paths: recovery.remaining_paths(),
            });
        }
        if let Some(authority) = recovery.live_recovery.authority() {
            self.resolve_recovery(&authority, cancellation)?;
        }
        recovery.live_recovery.acknowledge_resolution();
        Ok(DiscardRecoveryResolution {
            remaining_paths: recovery.remaining_paths(),
        })
    }

    pub fn undo_discard(
        &self,
        recovery: &DiscardRecovery,
        cancellation: &CancellationToken,
    ) -> Result<MutationOutcome, RepositoryError> {
        if recovery.live_recovery.needs_resolution() {
            return Ok(MutationOutcome::NotApplied { failure: MutationFailure::Rejected {
                reason: "Resolve the pending discard recovery first. No additional paths were undone; the original receipt retains the remaining paths.".into(),
            } });
        }
        if !recovery.can_undo() {
            return Ok(MutationOutcome::NotApplied { failure: MutationFailure::Rejected { reason: "Undo discard has expired; backups remain in Git objects and the system Trash".into() } });
        }
        let protection = Arc::new(
            crate::worktree_cleanup::protect(&recovery.worktree)
                .map_err(|error| ValidationError::Io(error.to_string()))?,
        );
        let root = self.captured_worktree_root(&recovery.worktree)?;
        let descriptors = match protection.as_ref() {
            Some(protection) => {
                if !protection
                    .protects_root(&root)
                    .map_err(|error| ValidationError::Io(error.to_string()))?
                {
                    return Err(ValidationError::Io("The cleanup lease no longer protects this exact recovery worktree; no path was undone".into()).into());
                }
                protection.retained_descriptor_count()
            }
            None => 0,
        };
        let context =
            crate::recovery_authority::RecoveryContext::with_guard(protection, descriptors)
                .map_err(ValidationError::Io)?;
        let directory = root
            .metadata()
            .map_err(|error| ValidationError::Io(error.to_string()))?;
        if (directory.dev(), directory.ino()) != recovery.worktree_identity {
            return Ok(MutationOutcome::NotApplied {
                failure: MutationFailure::Rejected {
                    reason: "The worktree directory was replaced; backups were preserved".into(),
                },
            });
        }
        let mut paths = Vec::new();
        let mut failures = Vec::new();
        let mut created_directories =
            RecoveryDirectoryFrontier::new_with_recovery(&recovery.live_recovery, &context);
        // Component order keeps each subtree contiguous. Release restored
        // siblings before entering the next one; descriptor use scales with
        // active ancestor depth, not the number of files/directories in Undo.
        let mut entries = recovery.entries.iter().collect::<Vec<_>>();
        entries.sort_by(|left, right| {
            left.path
                .as_path()
                .components()
                .cmp(right.path.as_path().components())
        });
        for entry in entries {
            if recovery.live_recovery.is_completed(&entry.path) {
                continue;
            }
            created_directories.flush_before(Some(&entry.path), &mut failures);
            let result = if recovery.live_recovery.needs_resolution() {
                Err("An earlier recovery entry is unresolved; this path was not changed".into())
            } else if cancellation.is_cancelled() {
                Err("Undo cancelled".into())
            } else {
                self.undo_discard_entry(
                    recovery,
                    &root,
                    entry,
                    cancellation,
                    &mut created_directories,
                )
            };
            match result {
                Ok(warning) => {
                    recovery.live_recovery.mark_completed(&entry.path);
                    paths.push(entry.path.clone());
                    if let Some(reason) = warning {
                        failures.push(MutationFailure::Reconciliation {
                            reason: format!("{} was restored: {reason}", entry.path),
                        });
                    }
                }
                Err(reason) => failures.push(MutationFailure::Path {
                    path: entry.path.clone(),
                    reason,
                }),
            }
        }
        // Includes cancelled and failed paths: no created inode loses its
        // retained authority before the actual archived-mode chmod.
        created_directories.flush_before(None, &mut failures);
        failures.extend(created_directories.warnings.into_values());
        if recovery.live_recovery.is_uncertain() {
            return Ok(MutationOutcome::OutcomeUnknown {
                failure: MutationFailure::Reconciliation {
                    reason: format!(
                        "Undo publication/putback remains unresolved; {} earlier paths were confirmed restored. Live filesystem recovery authority was retained. {}",
                        paths.len(),
                        failures
                            .iter()
                            .map(ToString::to_string)
                            .collect::<Vec<_>>()
                            .join("; ")
                    ),
                },
            });
        }
        if paths.is_empty() && failures.len() > 1 {
            return Ok(MutationOutcome::NotApplied {
                failure: MutationFailure::Rejected {
                    reason: failures
                        .iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                        .join("; "),
                },
            });
        }
        Ok(report_discard(paths, failures))
    }

    fn undo_discard_entry(
        &self,
        recovery: &DiscardRecovery,
        root: &fs_safety::Root,
        entry: &DiscardRecoveryEntry,
        cancellation: &CancellationToken,
        created_directories: &mut RecoveryDirectoryFrontier,
    ) -> Result<Option<String>, String> {
        if let SavedEntry::Unavailable { reason } = &entry.after {
            return Err(format!(
                "the post-discard state could not be verified: {reason}; backup was preserved"
            ));
        }
        let current_version = fs_safety::metadata_beneath(root, entry.path.as_path())
            .ok()
            .map(|metadata| metadata_version(&metadata));
        if current_version != entry.after_version {
            return Err("changed since discard; newer work was preserved".into());
        }
        let (parent, name) = recovery_parent(
            root,
            &entry.path,
            &recovery.directories,
            created_directories,
        )?;
        if let Some(trash) = &entry.trash {
            return restore_trashed_entry(trash, &parent, &name, cancellation, || {});
        }
        self.undo_saved_entry(
            recovery,
            root,
            entry,
            &parent,
            &name,
            &created_directories.context,
            cancellation,
        )
    }
}

fn parent_modes<'a>(
    root: &fs_safety::Root,
    paths: impl Iterator<Item = &'a RepoPath>,
) -> BTreeMap<RepoPath, u32> {
    let mut modes = BTreeMap::new();
    let mut seen = HashSet::new();
    for path in paths {
        for directory in path
            .as_path()
            .ancestors()
            .skip(1)
            .filter(|path| !path.as_os_str().is_empty())
        {
            let Ok(directory) = RepoPath::parse(directory) else {
                continue;
            };
            if !seen.insert(directory.clone()) {
                continue;
            }
            if let Ok(metadata) = fs_safety::metadata_beneath(root, directory.as_path())
                && metadata.is_dir()
            {
                modes.insert(directory, metadata.mode() & 0o7777);
            }
        }
    }
    modes
}

/// Keeps actual publication receipts alive until archived permissions have
/// been restored. The ordered caller flushes completed subtrees, so siblings
/// do not accumulate Root descriptors for a large recovery selection.
struct RecoveryDirectoryFrontier {
    owned: BTreeMap<RepoPath, OwnedRecoveryDirectory>,
    warnings: BTreeMap<RepoPath, MutationFailure>,
    live: LiveDiscardRecovery,
    context: crate::recovery_authority::RecoveryContext,
}

struct OwnedRecoveryDirectory {
    publication: fs_safety::DirectoryPublication,
    mode: u32,
    slot: Option<crate::recovery_authority::RecoverySlot>,
    mode_installed: bool,
    temporary_installed: bool,
    sync_file: Option<fs::File>,
}
impl OwnedRecoveryDirectory {
    fn install_temporary(&mut self) -> Result<(), String> {
        if self.temporary_installed {
            return Ok(());
        }
        // The producer alone restores its temporary staging permissions. Do
        // not overwrite that live witness with a consumer chmod first.
        self.publication
            .retry_publication_steps()
            .map_err(|error| error.to_string())?;
        let actual = self
            .publication
            .directory()
            .metadata()
            .map_err(|error| error.to_string())?;
        let initial = self.publication.initial_metadata();
        if (actual.mode(), actual.uid(), actual.gid())
            != (initial.mode(), initial.uid(), initial.gid())
        {
            return Err("The newly published directory metadata changed before recovery setup; it was preserved".into());
        }
        set_exact_directory_mode(self.publication.directory(), 0o700 | (self.mode & 0o2000))
            .map_err(|error| error.to_string())?;
        self.temporary_installed = true;
        Ok(())
    }
    fn finish(&mut self) -> Result<(), String> {
        // Cleanup only the producer's private staging, never the restored
        // nonempty public directory. Its original inode stays pinned here.
        self.install_temporary()?;
        self.publication
            .retry_publication_steps()
            .map_err(|error| error.to_string())?;
        let metadata = self
            .publication
            .directory()
            .metadata()
            .map_err(|error| error.to_string())?;
        let initial = self.publication.initial_metadata();
        let expected = if self.mode_installed {
            self.mode
        } else {
            0o700 | (self.mode & 0o2000)
        };
        if metadata.mode() & 0o7777 != expected & 0o7777
            || (metadata.uid(), metadata.gid()) != (initial.uid(), initial.gid())
        {
            return Err("The created directory's permissions or ownership differ from the recovery proof; current metadata was preserved".into());
        }
        if self.sync_file.is_none() {
            self.sync_file = Some(
                open_directory_sync_handle(self.publication.directory())
                    .map_err(|error| error.to_string())?,
            );
        }
        if !self.mode_installed {
            set_exact_directory_mode(self.publication.directory(), self.mode)
                .map_err(|error| error.to_string())?;
            self.mode_installed = true;
        }
        // The readable directory FD predates chmod, so even archived000 mode
        // can be synced without reopening an inaccessible pathname.
        self.sync_file
            .as_ref()
            .unwrap()
            .sync_all()
            .map_err(|error| {
                format!("Archived permissions are restored, but durability is unconfirmed: {error}")
            })
    }
    fn retain(mut self, live: &LiveDiscardRecovery) {
        let description = format!(
            "Created recovery directory originally named {}",
            self.publication.directory().path().display()
        );
        let slot = self
            .slot
            .take()
            .expect("directory recovery was reserved before creation");
        live.retain(slot.retain(description, move || self.finish()));
    }
}

fn open_directory_sync_handle(root: &fs_safety::Root) -> io::Result<fs::File> {
    use std::os::fd::FromRawFd;
    // SAFETY: the retained directory owns its FD; openat(".") resolves that
    // exact inode and returns a fresh CLOEXEC File, before restrictive chmod.
    let descriptor = unsafe {
        libc::openat(
            root.descriptor(),
            c".".as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
        )
    };
    if descriptor < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { fs::File::from_raw_fd(descriptor) })
}

impl RecoveryDirectoryFrontier {
    #[cfg(test)]
    fn new(_root: &fs_safety::Root) -> Self {
        Self::new_with_recovery(
            &LiveDiscardRecovery::default(),
            &crate::recovery_authority::RecoveryContext::with_guard((), 0).unwrap(),
        )
    }
    fn new_with_recovery(
        live: &LiveDiscardRecovery,
        context: &crate::recovery_authority::RecoveryContext,
    ) -> Self {
        Self {
            owned: BTreeMap::new(),
            warnings: BTreeMap::new(),
            live: live.clone(),
            context: context.clone(),
        }
    }
    fn flush_before(&mut self, next: Option<&RepoPath>, failures: &mut Vec<MutationFailure>) {
        let mut done = self
            .owned
            .keys()
            .filter(|path| next.is_none_or(|next| !next.as_path().starts_with(path.as_path())))
            .cloned()
            .collect::<Vec<_>>();
        done.sort_by(|a, b| {
            b.as_path()
                .components()
                .count()
                .cmp(&a.as_path().components().count())
                .then_with(|| a.cmp(b))
        });
        for path in done {
            let mut owned = self
                .owned
                .remove(&path)
                .expect("selected owned frontier entry");
            if let Err(error) = owned.finish() {
                failures.push(MutationFailure::Path { path, reason: format!("The created directory's permission restoration/recovery remains unresolved: {error}") });
                owned.retain(&self.live);
            }
        }
    }
    fn held_parent(
        &self,
        root: &fs_safety::Root,
        key: &RepoPath,
    ) -> Result<Option<fs_safety::Root>, String> {
        let Some(owned) = self.owned.get(key) else {
            return Ok(None);
        };
        let named =
            fs_safety::metadata_beneath(root, key.as_path()).map_err(|error| error.to_string())?;
        if !named.is_dir() || (named.dev(), named.ino()) != owned.publication.identity() {
            return Err(
                "the created parent directory was moved or replaced; its replacement was preserved"
                    .into(),
            );
        }
        owned
            .publication
            .directory()
            .try_clone()
            .map(Some)
            .map_err(|error| error.to_string())
    }
}

/// chmod can succeed while the kernel clears unsupported set-id bits. Report
/// the actual retained inode's mode instead of equating syscall success with
/// exact restoration.
fn set_exact_directory_mode(directory: &fs_safety::Root, mode: u32) -> io::Result<()> {
    fs_safety::set_directory_permissions(directory, mode)?;
    verify_directory_mode(directory, mode)
}

fn verify_directory_mode(directory: &fs_safety::Root, expected: u32) -> io::Result<()> {
    let actual = directory.metadata()?.mode() & 0o7777;
    if actual != expected & 0o7777 {
        return Err(io::Error::other(format!(
            "requested permissions {:04o}, but the retained directory has {:04o}",
            expected & 0o7777,
            actual,
        )));
    }
    Ok(())
}

fn recovery_parent(
    root: &fs_safety::Root,
    path: &RepoPath,
    modes: &BTreeMap<RepoPath, u32>,
    created: &mut RecoveryDirectoryFrontier,
) -> Result<(fs_safety::Root, OsString), String> {
    let mut parent = root.try_clone().map_err(|error| error.to_string())?;
    let mut relative = PathBuf::new();
    for component in path
        .as_path()
        .parent()
        .into_iter()
        .flat_map(|path| path.components())
    {
        relative.push(component.as_os_str());
        let key = RepoPath::parse(&relative).map_err(|error| error.to_string())?;
        if let Some(held) = created.held_parent(root, &key)? {
            parent = held;
            continue;
        }
        let mode = modes.get(&key).copied().unwrap_or(0o755);
        // Creation ownership depends on the actual requested parent. A
        // protected checkout top cannot authorize mkdir in a writable nested
        // directory. Published parents stay private while descendants need it;
        // their archived mode is restored by the component-ordered frontier.
        let temporary_mode = 0o700 | (mode & 0o2000);
        let slot = created.context.reserve()?;
        // Own the exact parent before mkdir so error cleanup retains both its
        // lease and the directory requiring fsync, without a post-effect clone.
        let preparation_parent = parent.try_clone().map_err(|error| error.to_string())?;
        let result = match fs_safety::create_directory_beneath(
            &parent,
            Path::new(component.as_os_str()),
            temporary_mode,
        ) {
            Ok(result) => result,
            Err(mut error) => {
                let message = error.to_string();
                if let Some(mut preparation) = error.take_recovery() {
                    let description = format!(
                        "Recovery parent preparation originally named {}",
                        preparation.diagnostic_path().display()
                    );
                    created.live.retain(slot.retain(description, move || {
                        preparation
                            .retry_cleanup()
                            .map_err(|error| error.to_string())?;
                        fs_safety::sync_directory(&preparation_parent)
                            .map_err(|error| error.to_string())
                    }));
                }
                return Err(message);
            }
        };
        parent = match result {
            fs_safety::DirectoryCreation::Existing(directory) => directory,
            fs_safety::DirectoryCreation::Unverified { directory, reason } => {
                let description = format!(
                    "Unverified recovery directory originally named {}",
                    directory.path().display()
                );
                let retained = match directory.try_clone() {
                    Ok(retained) => retained,
                    Err(error) => {
                        created.live.retain(slot.retain(description, move || {
                            if directory.metadata().map_err(|error|error.to_string())?.nlink() == 0 { Ok(()) }
                            else { Err("The unverified directory is still retained; its entry was not modified".into()) }
                        }));
                        return Err(error.to_string());
                    }
                };
                created.live.retain(slot.retain(description, move || {
                    if retained.metadata().map_err(|error|error.to_string())?.nlink() == 0 { Ok(()) }
                    else { Err("Creation ownership is unverified; the retained directory was left unchanged".into()) }
                }));
                created.warnings.insert(key.clone(), MutationFailure::Path { path: key, reason: format!("The directory's creation ownership is unverified; its permissions were preserved and exact archived-mode restoration is unconfirmed: {reason}") });
                directory
            }
            fs_safety::DirectoryCreation::Published(publication) => {
                let publication_error = publication.publication_error().map(str::to_owned);
                if let Some(error) = publication_error {
                    let staging = publication
                        .pending_staging_path()
                        .map(|path| path.display().to_string())
                        .unwrap_or_else(|| "unavailable".into());
                    created.warnings.insert(key.clone(), MutationFailure::Path { path: key.clone(), reason: format!("The created directory was published, but staging cleanup is unconfirmed: {error}; originally opened recovery spelling: {staging}") });
                }
                // Retain BEFORE fallible chmod/clone so even partial setup keeps
                // the exact created inode for final mode restoration.
                created.owned.insert(
                    key.clone(),
                    OwnedRecoveryDirectory {
                        publication,
                        mode,
                        slot: Some(slot),
                        mode_installed: false,
                        temporary_installed: false,
                        sync_file: None,
                    },
                );
                created.owned.get_mut(&key).unwrap().install_temporary()?;
                created
                    .held_parent(root, &key)?
                    .ok_or("the created parent authority is unavailable")?
            }
        };
    }
    let name = path
        .as_path()
        .file_name()
        .ok_or("invalid recovery path")?
        .to_owned();
    Ok((parent, name))
}

fn changed_recovery_entries(
    root: &fs_safety::Root,
    before: BTreeMap<RepoPath, SavedEntry>,
    after: &BTreeMap<RepoPath, Result<SavedEntry, String>>,
) -> Vec<DiscardRecoveryEntry> {
    let mut entries = Vec::new();
    for (path, original) in before {
        match after.get(&path) {
            Some(Ok(after)) if *after != original => {
                let after_version = fs_safety::metadata_beneath(&root, path.as_path())
                    .ok()
                    .map(|metadata| metadata_version(&metadata));
                entries.push(DiscardRecoveryEntry {
                    path,
                    before: original,
                    after: after.clone(),
                    after_version,
                    trash: None,
                });
            }
            Some(Err(reason)) => entries.push(DiscardRecoveryEntry {
                path,
                before: original,
                after: SavedEntry::Unavailable {
                    reason: reason.clone(),
                },
                after_version: None,
                trash: None,
            }),
            _ => {}
        }
    }
    entries
}

pub(super) type CaptureVersions = BTreeMap<RepoPath, Option<ConflictMarkerVersion>>;

pub(super) fn verify_recovery_root(root: &fs_safety::Root) -> Result<(), String> {
    let opened = root.metadata().map_err(|error| error.to_string())?;
    let named = fs::symlink_metadata(root.path()).map_err(|error| error.to_string())?;
    if (opened.dev(), opened.ino()) != (named.dev(), named.ino()) {
        return Err(
            "The worktree directory changed after backup; no further contents were discarded"
                .into(),
        );
    }
    Ok(())
}

pub(super) fn verify_captured_versions(
    root: &fs_safety::Root,
    versions: &CaptureVersions,
    cancellation: &CancellationToken,
) -> Result<(), String> {
    verify_recovery_root(root)?;
    for (path, expected) in versions {
        if cancellation.is_cancelled() {
            return Err("Discard cancelled before changing working contents".into());
        }
        let current = match fs_safety::metadata_beneath(root, path.as_path()) {
            Ok(metadata) => Some(metadata_version(&metadata)),
            Err(error) if error.is_not_found() => None,
            Err(error) => return Err(format!("Cannot verify {path} after backup: {error}")),
        };
        if current != *expected {
            return Err(format!(
                "{path} changed after its backup; newer contents were preserved. Retry discard to back up the current version"
            ));
        }
    }
    Ok(())
}

fn metadata_version(metadata: &fs::Metadata) -> (u64, u64, u64, i64, i64, i64, i64) {
    (
        metadata.dev(),
        metadata.ino(),
        metadata.len(),
        metadata.mtime(),
        metadata.mtime_nsec(),
        metadata.ctime(),
        metadata.ctime_nsec(),
    )
}

pub(super) fn trash_untracked(
    root: &fs_safety::Root,
    path: &RepoPath,
) -> Result<Option<TrashReceipt>, MutationFailure> {
    let reject = |reason| MutationFailure::Path {
        path: path.clone(),
        reason,
    };
    let (parent, name) = fs_safety::open_parent_beneath(root, path.as_path())
        .map_err(|error| reject(error.to_string()))?;
    let metadata = match fs_safety::metadata_beneath(&parent, Path::new(&name)) {
        Ok(metadata) => metadata,
        Err(error) if error.is_not_found() => return Ok(None),
        Err(error) => return Err(reject(error.to_string())),
    };
    if !metadata.is_file() && !metadata.file_type().is_symlink() {
        return Err(reject(
            "only regular files and symlinks are moved to Trash".into(),
        ));
    }
    #[cfg(not(test))]
    let home = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share")))
        .ok_or_else(|| reject("HOME and XDG_DATA_HOME are unavailable".into()))?;
    #[cfg(test)]
    let home = TEST_TRASH_BASE.with(|directory| directory.path().to_owned());
    let mut trash = home.join("Trash");
    private_directory(&trash).map_err(reject)?;
    let mut original = root.path().join(path.as_path());
    if fs::metadata(&trash)
        .map_err(|error| reject(error.to_string()))?
        .dev()
        != metadata.dev()
    {
        let mut mount = parent.path();
        while let Some(next) = mount.parent() {
            if fs::metadata(next)
                .map_err(|error| reject(error.to_string()))?
                .dev()
                != metadata.dev()
            {
                break;
            }
            mount = next;
        }
        trash = mount.join(format!(".Trash-{}", unsafe { libc::geteuid() }));
        private_directory(&trash).map_err(reject)?;
        original = original
            .strip_prefix(mount)
            .map_err(|error| reject(error.to_string()))?
            .to_owned();
    }
    let files = trash.join("files");
    let info = trash.join("info");
    private_directory(&files).map_err(reject)?;
    private_directory(&info).map_err(reject)?;
    let files_root = fs_safety::Root::open(&files).map_err(|error| reject(error.to_string()))?;
    let id = Uuid::new_v4().to_string();
    let file_path = files.join(&id);
    let info_path = info.join(format!("{id}.trashinfo"));
    let mut metadata_file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&info_path)
        .map_err(|error| reject(error.to_string()))?;
    let escaped = original
        .as_os_str()
        .as_bytes()
        .iter()
        .map(|byte| {
            if byte.is_ascii_alphanumeric() || b"/-_.~".contains(byte) {
                char::from(*byte).to_string()
            } else {
                format!("%{byte:02X}")
            }
        })
        .collect::<String>();
    let date = trash_date();
    if let Err(error) = write!(
        metadata_file,
        "[Trash Info]\nPath={escaped}\nDeletionDate={date}\n"
    )
    .and_then(|()| metadata_file.sync_all())
    {
        let _ = fs::remove_file(&info_path);
        return Err(reject(error.to_string()));
    }
    if let Err(error) = fs_safety::rename_new_between(&parent, &name, &files_root, OsStr::new(&id))
    {
        let _ = fs::remove_file(&info_path);
        return Err(reject(error.to_string()));
    }
    let moved = fs_safety::metadata_beneath(&files_root, Path::new(&id)).ok();
    // renameat can move a directory too. A final-component swap must not turn
    // the earlier regular-file classification into a silent directory discard.
    // Restore a raced entry without overwriting anything newer; if that name
    // is occupied, keep its contents and original-path metadata in Trash.
    let changed = moved.as_ref().is_none_or(|moved| {
        (
            moved.dev(),
            moved.ino(),
            moved.len(),
            moved.mtime(),
            moved.mtime_nsec(),
            moved.mode(),
        ) != (
            metadata.dev(),
            metadata.ino(),
            metadata.len(),
            metadata.mtime(),
            metadata.mtime_nsec(),
            metadata.mode(),
        )
    });
    let warning = if changed {
        match fs_safety::rename_new_between(&files_root, OsStr::new(&id), &parent, &name) {
            Ok(()) => {
                let _ = fs::remove_file(&info_path);
                return Err(reject(
                    "changed while moving to Trash; the entry was restored without discarding it"
                        .into(),
                ));
            }
            Err(error) => Some(format!(
                "changed while moving to Trash; the raced entry is preserved at {} because its original name could not be restored: {error}",
                file_path.display()
            )),
        }
    } else {
        None
    };
    let _ = fs_safety::sync_directory(&parent);
    let _ = fs_safety::sync_directory(&files_root);
    let version = moved.as_ref().map(metadata_version);
    Ok(Some(TrashReceipt {
        warning,
        path: path.clone(),
        file: file_path,
        info: info_path,
        version,
    }))
}

// Renaming changes ctime. Other content/mode/identity fields must remain
// unchanged across our moves; the full version is checked before the first one.
fn moved_entry_version(metadata: &fs::Metadata) -> (u64, u64, u64, i64, i64, u32) {
    (
        metadata.dev(),
        metadata.ino(),
        metadata.len(),
        metadata.mtime(),
        metadata.mtime_nsec(),
        metadata.mode(),
    )
}

// Hash only when Undo is requested, with bounded memory and cancellation.
// Discard still moves untracked contents without a per-file Git subprocess.
fn trash_content_digest(
    root: &fs_safety::Root,
    path: &Path,
    cancellation: &CancellationToken,
) -> Result<[u8; 32], String> {
    use sha2::Digest;
    if cancellation.is_cancelled() {
        return Err("Undo cancelled while verifying Trash contents".into());
    }
    let metadata = fs_safety::metadata_beneath(root, path).map_err(|error| error.to_string())?;
    let mut digest = sha2::Sha256::new();
    if metadata.file_type().is_symlink() {
        let target = fs_safety::read_link_beneath(root, path).map_err(|error| error.to_string())?;
        digest.update(target.as_os_str().as_bytes());
    } else if metadata.is_file() {
        let mut file = fs_safety::open_beneath(root, path).map_err(|error| error.to_string())?;
        let mut buffer = [0; 64 * 1024];
        loop {
            if cancellation.is_cancelled() {
                return Err("Undo cancelled while verifying Trash contents".into());
            }
            let count = file.read(&mut buffer).map_err(|error| error.to_string())?;
            if count == 0 {
                break;
            }
            digest.update(&buffer[..count]);
        }
    } else {
        return Err("the Trash entry is no longer a regular file or symlink".into());
    }
    Ok(digest.finalize().into())
}

fn restore_trashed_entry(
    trash: &TrashReceipt,
    parent: &fs_safety::Root,
    name: &OsStr,
    cancellation: &CancellationToken,
    before_move: impl FnOnce(),
) -> Result<Option<String>, String> {
    let directory = trash.file.parent().ok_or("invalid Trash receipt")?;
    private_directory(directory)?;
    let trash_root = fs_safety::Root::open(directory).map_err(|error| error.to_string())?;
    let trash_name = trash.file.file_name().ok_or("invalid Trash receipt")?;
    let expected = fs_safety::metadata_beneath(&trash_root, Path::new(trash_name))
        .map_err(|error| error.to_string())?;
    if trash.version != Some(metadata_version(&expected)) {
        return Err("the Trash entry changed or could not be verified; it was preserved".into());
    }
    let content = trash_content_digest(&trash_root, Path::new(trash_name), cancellation)?;
    let after_read = fs_safety::metadata_beneath(&trash_root, Path::new(trash_name))
        .map_err(|error| error.to_string())?;
    if trash.version != Some(metadata_version(&after_read)) {
        return Err(
            "the Trash entry changed while verifying its contents; it was preserved".into(),
        );
    }
    // Keep a raced source entirely inside Trash until the move's actual leaf
    // has been checked. No worktree file is installed by an unverified move.
    let holding = OsString::from(format!(".cagents-undo-{}", Uuid::new_v4()));
    before_move();
    fs_safety::rename_new_within(&trash_root, trash_name, &holding)
        .map_err(|error| error.to_string())?;
    let put_back = || -> String {
        match fs_safety::rename_new_within(&trash_root, &holding, trash_name) {
            Ok(()) => format!(
                "the entry and original-path metadata were preserved in Trash at {}",
                trash.file.display()
            ),
            Err(error) => format!(
                "the entry is preserved at {}; original-path metadata remains at {}; its former Trash name could not be restored: {error}",
                directory.join(&holding).display(),
                trash.info.display()
            ),
        }
    };
    let verified = (|| -> Result<(), String> {
        let moved = fs_safety::metadata_beneath(&trash_root, Path::new(&holding))
            .map_err(|error| error.to_string())?;
        if moved_entry_version(&moved) != moved_entry_version(&expected) {
            return Err("the Trash entry changed while Undo was acquiring it".into());
        }
        // Rename itself changes ctime. Verify bytes as well, so a writer that
        // preserves size/mtime cannot hide behind our legitimate ctime change.
        let actual = trash_content_digest(&trash_root, Path::new(&holding), cancellation)?;
        let after = fs_safety::metadata_beneath(&trash_root, Path::new(&holding))
            .map_err(|error| error.to_string())?;
        if actual != content || metadata_version(&moved) != metadata_version(&after) {
            return Err("the Trash entry contents changed while Undo was acquiring it".into());
        }
        Ok(())
    })();
    if let Err(reason) = verified {
        return Err(format!("{reason}; {}", put_back()));
    }
    if let Err(error) = fs_safety::rename_new_between(&trash_root, &holding, parent, name) {
        return Err(format!(
            "could not restore without replacing newer work: {error}; {}",
            put_back()
        ));
    }
    let installed = fs_safety::metadata_beneath(parent, Path::new(name));
    if installed.as_ref().map(moved_entry_version).ok() != Some(moved_entry_version(&expected)) {
        // An external writer may still hold the inode or replace its new name.
        // Do not move an unverified destination: that could relocate newer work.
        return Ok(Some(format!(
            "contents or identity changed during restore; inspect {}; original Trash metadata was retained at {}",
            parent.path().join(name).display(),
            trash.info.display()
        )));
    }
    let mut warnings = Vec::new();
    if let Err(error) = fs::remove_file(&trash.info) {
        warnings.push(format!("could not remove Trash metadata: {error}"));
    }
    if let Err(error) = fs_safety::sync_directory(&trash_root) {
        warnings.push(format!("Trash directory sync failed: {error}"));
    }
    if let Err(error) = fs_safety::sync_directory(parent) {
        warnings.push(format!("restored directory sync failed: {error}"));
    }
    Ok((!warnings.is_empty()).then(|| warnings.join("; ")))
}

fn private_directory(path: &Path) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    match fs::create_dir(path) {
        Ok(()) => fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .map_err(|error| error.to_string())?,
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error.to_string()),
    }
    let metadata = fs::symlink_metadata(path).map_err(|error| error.to_string())?;
    if !metadata.is_dir()
        || metadata.file_type().is_symlink()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o077 != 0
    {
        return Err(format!(
            "{} is not a private owned Trash directory (0700 or stricter required)",
            path.display()
        ));
    }
    Ok(())
}

fn trash_date() -> String {
    let seconds = (unix_ms() / 1000) as libc::time_t;
    // SAFETY: localtime_r initializes only the provided tm for this timestamp.
    let mut local: libc::tm = unsafe { std::mem::zeroed() };
    if unsafe { libc::localtime_r(&seconds, &mut local) }.is_null() {
        return "1970-01-01T00:00:00".into();
    }
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}",
        local.tm_year + 1900,
        local.tm_mon + 1,
        local.tm_mday,
        local.tm_hour,
        local.tm_min,
        local.tm_sec
    )
}

fn prune_empty_parents(root: &fs_safety::Root, path: &RepoPath) {
    let mut parent = path.as_path().parent();
    while let Some(directory) = parent.filter(|path| !path.as_os_str().is_empty()) {
        match fs_safety::remove_empty_directory_beneath(root, directory) {
            Ok(true) => parent = directory.parent(),
            _ => break,
        }
    }
}

#[cfg(test)]
thread_local! {
    // Legacy safety stress tests create hundreds of discarded fixture entries;
    // keep them out of the desktop user's actual Trash.
    static TEST_TRASH_BASE: tempfile::TempDir = tempfile::tempdir().expect("test Trash directory");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trash_restore_verifies_the_acquired_leaf_without_overwriting_newer_work() {
        for kind in [
            "unchanged",
            "write",
            "preserve-mtime",
            "replace",
            "destination",
        ] {
            // Each thread owns its private test Trash; no process environment
            // or desktop Trash is changed by these deterministic interleavings.
            std::thread::spawn(move || {
                let work = tempfile::tempdir().unwrap();
                let root = fs_safety::Root::open(work.path()).unwrap();
                let path = RepoPath::parse("new.txt").unwrap();
                fs::write(work.path().join("new.txt"), b"original\n").unwrap();
                let trash = trash_untracked(&root, &path).unwrap().unwrap();
                let modified = fs::metadata(&trash.file).unwrap().modified().unwrap();
                let changed = || match kind {
                    "write" | "preserve-mtime" => {
                        fs::write(&trash.file, b"modified\n").unwrap();
                        if kind == "preserve-mtime" {
                            let file = fs::OpenOptions::new()
                                .write(true)
                                .open(&trash.file)
                                .unwrap();
                            file.set_times(fs::FileTimes::new().set_modified(modified))
                                .unwrap();
                        }
                    }
                    "replace" => {
                        fs::rename(&trash.file, trash.file.with_extension("original")).unwrap();
                        fs::write(&trash.file, b"modified\n").unwrap();
                    }
                    "destination" => {
                        fs::write(work.path().join("new.txt"), b"newer work\n").unwrap()
                    }
                    _ => {}
                };
                let result = restore_trashed_entry(
                    &trash,
                    &root,
                    OsStr::new("new.txt"),
                    &CancellationToken::new(),
                    changed,
                );
                if kind == "unchanged" {
                    assert_eq!(result.unwrap(), None);
                    assert_eq!(
                        fs::read(work.path().join("new.txt")).unwrap(),
                        b"original\n"
                    );
                    assert!(!trash.file.exists() && !trash.info.exists());
                } else {
                    let reason = result.unwrap_err();
                    assert!(
                        trash.info.exists(),
                        "original-path metadata must survive refusal"
                    );
                    if kind == "destination" {
                        assert!(reason.contains("newer work"));
                        assert_eq!(
                            fs::read(work.path().join("new.txt")).unwrap(),
                            b"newer work\n"
                        );
                        assert_eq!(fs::read(&trash.file).unwrap(), b"original\n");
                    } else {
                        assert!(reason.contains("changed"), "{kind}: {reason}");
                        if kind == "preserve-mtime" {
                            assert!(reason.contains("contents changed"));
                        }
                        assert!(!work.path().join("new.txt").exists());
                        assert_eq!(fs::read(&trash.file).unwrap(), b"modified\n");
                        if kind == "replace" {
                            assert_eq!(
                                fs::read(trash.file.with_extension("original")).unwrap(),
                                b"original\n"
                            );
                        }
                    }
                }
            })
            .join()
            .unwrap();
        }
    }

    #[test]
    fn readable_existing_trash_directories_refuse_without_moving_private_work() {
        for exposed in ["all", "Trash", "files", "info"] {
            std::thread::spawn(move || {
                let work = tempfile::tempdir().unwrap();
                let root = fs_safety::Root::open(work.path()).unwrap();
                fs::write(work.path().join("private.txt"), b"private contents\n").unwrap();
                fs::set_permissions(
                    work.path().join("private.txt"),
                    fs::Permissions::from_mode(0o644),
                )
                .unwrap();
                fs::set_permissions(work.path(), fs::Permissions::from_mode(0o700)).unwrap();
                TEST_TRASH_BASE.with(|base| {
                    let trash = base.path().join("Trash");
                    private_directory(&trash).unwrap();
                    private_directory(&trash.join("files")).unwrap();
                    private_directory(&trash.join("info")).unwrap();
                    let targets = match exposed {
                        "all" => vec![trash.clone(), trash.join("files"), trash.join("info")],
                        "Trash" => vec![trash.clone()],
                        _ => vec![trash.join(exposed)],
                    };
                    for target in &targets {
                        fs::set_permissions(target, fs::Permissions::from_mode(0o755)).unwrap();
                    }
                    let result = trash_untracked(&root, &RepoPath::parse("private.txt").unwrap());
                    assert!(
                        result
                            .unwrap_err()
                            .to_string()
                            .contains("private owned Trash directory")
                    );
                    assert_eq!(
                        fs::read(work.path().join("private.txt")).unwrap(),
                        b"private contents\n"
                    );
                    assert_eq!(fs::read_dir(trash.join("files")).unwrap().count(), 0);
                    for target in &targets {
                        assert_eq!(
                            fs::metadata(target).unwrap().mode() & 0o777,
                            0o755,
                            "do not silently change the user's existing directory permissions"
                        );
                    }
                });
            })
            .join()
            .unwrap();
        }
    }
}

#[cfg(test)]
mod directory_frontier_tests {
    use super::*;
    fn path(value: &str) -> RepoPath {
        RepoPath::parse(value).unwrap()
    }

    #[test]
    fn recovery_directory_frontier_restores_completed_subtrees_without_blocking_needed_ancestors() {
        let dir = tempfile::tempdir().unwrap();
        let root = fs_safety::Root::open(dir.path()).unwrap();
        let modes = BTreeMap::from([
            (path("a"), 0o500),
            (path("a/b"), 0o701),
            (path("a-z"), 0o710),
        ]);
        let mut state = RecoveryDirectoryFrontier::new(&root);
        let mut failures = vec![];
        let (parent, _) = recovery_parent(&root, &path("a/b/first"), &modes, &mut state).unwrap();
        drop(parent);
        state.flush_before(Some(&path("a/b/second")), &mut failures);
        assert_eq!(
            fs::metadata(dir.path().join("a")).unwrap().mode() & 0o777,
            0o700,
            "a must stay writable until every descendant is handled"
        );
        let (parent, _) = recovery_parent(&root, &path("a/b/second"), &modes, &mut state).unwrap();
        drop(parent);
        state.flush_before(Some(&path("a-z/next")), &mut failures);
        assert_eq!(
            fs::metadata(dir.path().join("a")).unwrap().mode() & 0o777,
            0o500
        );
        assert_eq!(
            fs::metadata(dir.path().join("a/b")).unwrap().mode() & 0o777,
            0o701
        );
        let (parent, _) = recovery_parent(&root, &path("a-z/next"), &modes, &mut state).unwrap();
        drop(parent);
        state.flush_before(None, &mut failures);
        assert_eq!(
            fs::metadata(dir.path().join("a-z")).unwrap().mode() & 0o777,
            0o710
        );
        assert!(failures.is_empty(), "{failures:?}");
        fs::set_permissions(dir.path().join("a"), fs::Permissions::from_mode(0o700)).unwrap();
    }

    #[test]
    fn recovery_directory_frontier_restores_shared_modes_after_nested_private_creation() {
        let dir = tempfile::tempdir().unwrap();
        let root = fs_safety::Root::open(dir.path()).unwrap();
        let mut state = RecoveryDirectoryFrontier::new(&root);
        let modes = BTreeMap::from([(path("shared"), 0o775), (path("shared/nested"), 0o750)]);
        let (parent, _) =
            recovery_parent(&root, &path("shared/nested/file"), &modes, &mut state).unwrap();
        drop(parent);
        assert_eq!(
            fs::metadata(dir.path().join("shared")).unwrap().mode() & 0o7777,
            0o700
        );
        assert_eq!(
            state.owned.len(),
            2,
            "each actual private parent must publish its own held receipt"
        );
        assert!(state.warnings.is_empty());
        let mut failures = vec![];
        state.flush_before(None, &mut failures);
        assert!(failures.is_empty(), "{failures:?}");
        assert_eq!(
            fs::metadata(dir.path().join("shared")).unwrap().mode() & 0o7777,
            0o775
        );
        assert_eq!(
            fs::metadata(dir.path().join("shared/nested"))
                .unwrap()
                .mode()
                & 0o7777,
            0o750
        );
    }

    #[test]
    fn recovery_directory_frontier_preserves_setgid_inheritance_while_parents_are_private() {
        let dir = tempfile::tempdir().unwrap();
        let root = fs_safety::Root::open(dir.path()).unwrap();
        let mut state = RecoveryDirectoryFrontier::new(&root);
        let modes = BTreeMap::from([(path("parent"), 0o2775), (path("parent/nested"), 0o2750)]);
        let (parent, _) =
            recovery_parent(&root, &path("parent/nested/file"), &modes, &mut state).unwrap();
        let outer = fs::metadata(dir.path().join("parent")).unwrap();
        let inner = parent.metadata().unwrap();
        assert_eq!(outer.mode() & 0o7777, 0o2700);
        assert_eq!(inner.mode() & 0o7777, 0o2700);
        assert_eq!(
            inner.gid(),
            outer.gid(),
            "nested creation must retain the parent's actual SGID inheritance"
        );
        drop(parent);
        let mut failures = vec![];
        state.flush_before(None, &mut failures);
        assert!(failures.is_empty(), "{failures:?}");
        assert_eq!(
            fs::metadata(dir.path().join("parent")).unwrap().mode() & 0o7777,
            0o2775
        );
        assert_eq!(
            fs::metadata(dir.path().join("parent/nested"))
                .unwrap()
                .mode()
                & 0o7777,
            0o2750
        );
    }

    #[test]
    fn recovery_directory_mode_verification_reports_stripped_or_different_bits() {
        let dir = tempfile::tempdir().unwrap();
        let root = fs_safety::Root::open(dir.path()).unwrap();
        fs_safety::set_directory_permissions(&root, 0o700).unwrap();
        let error = verify_directory_mode(&root, 0o2700).unwrap_err();
        assert!(error.to_string().contains("2700"));
        assert!(error.to_string().contains("0700"));
        verify_directory_mode(&root, 0o700).unwrap();
    }

    #[test]
    fn recovery_directory_frontier_retains_failed_permissions_until_exact_original_retry() {
        let dir = tempfile::tempdir().unwrap();
        let root = fs_safety::Root::open(dir.path()).unwrap();
        let mut state = RecoveryDirectoryFrontier::new(&root);
        let modes = BTreeMap::from([(path("parent"), 0o500)]);
        let (held, _) = recovery_parent(&root, &path("parent/file"), &modes, &mut state).unwrap();
        fs_safety::set_directory_permissions(&held, 0o711).unwrap();
        let mut failures = vec![];
        state.flush_before(None, &mut failures);
        assert!(!failures.is_empty());
        assert_eq!(
            held.metadata().unwrap().mode() & 0o7777,
            0o711,
            "newer permissions must not be overwritten"
        );
        let authority = state.live.authority().unwrap();
        assert!(!authority.is_resolved());
        assert!(authority.retry().is_err());
        fs::rename(dir.path().join("parent"), dir.path().join("original")).unwrap();
        fs::create_dir(dir.path().join("parent")).unwrap();
        fs::set_permissions(dir.path().join("parent"), fs::Permissions::from_mode(0o751)).unwrap();
        // Explicitly resolve the unexpected metadata on the original inode.
        fs_safety::set_directory_permissions(&held, 0o700).unwrap();
        authority.retry().unwrap();
        assert!(authority.is_resolved());
        assert_eq!(
            fs::metadata(dir.path().join("original")).unwrap().mode() & 0o7777,
            0o500
        );
        assert_eq!(
            fs::metadata(dir.path().join("parent")).unwrap().mode() & 0o7777,
            0o751
        );
        fs_safety::set_directory_permissions(&held, 0o700).unwrap();
    }

    #[test]
    fn recovery_directory_frontier_syncs_restrictive_archived_mode_using_preopened_directory() {
        let dir = tempfile::tempdir().unwrap();
        let root = fs_safety::Root::open(dir.path()).unwrap();
        let mut state = RecoveryDirectoryFrontier::new(&root);
        let modes = BTreeMap::from([(path("parent"), 0)]);
        let (held, _) = recovery_parent(&root, &path("parent/file"), &modes, &mut state).unwrap();
        let mut failures = vec![];
        state.flush_before(None, &mut failures);
        assert!(failures.is_empty(), "{failures:?}");
        assert!(!state.live.is_pending());
        assert_eq!(held.metadata().unwrap().mode() & 0o7777, 0);
        fs_safety::set_directory_permissions(&held, 0o700).unwrap();
    }

    #[test]
    fn discard_recovery_requires_explicit_ack_even_when_an_authority_clone_already_resolved() {
        let dir = tempfile::tempdir().unwrap();
        let recovery = DiscardRecovery {
            live_recovery: LiveDiscardRecovery::default(),
            worktree: dir.path().join("not-a-repository"),
            worktree_identity: (0, 0),
            directories: BTreeMap::new(),
            expires_unix_ms: 0,
            entries: vec![DiscardRecoveryEntry {
                path: path("remaining"),
                before: SavedEntry::Absent,
                after: SavedEntry::Absent,
                after_version: None,
                trash: None,
            }],
        };
        let slot = crate::recovery_authority::RecoverySlot::reserve().unwrap();
        recovery
            .live_recovery
            .retain(slot.retain("owned test recovery".into(), || Ok(())));
        let service = RepositoryService::system("git");
        let cancel = CancellationToken::new();
        service
            .resolve_recovery(&recovery.live_recovery.authority().unwrap(), &cancel)
            .unwrap();
        assert!(!recovery.live_recovery.is_pending());
        assert!(recovery.live_recovery.needs_resolution());
        assert!(recovery.can_undo());
        assert!(
            matches!(
                service.undo_discard(&recovery, &cancel).unwrap(),
                MutationOutcome::NotApplied { .. }
            ),
            "the admission guard must refuse before opening the nonexistent worktree"
        );
        let resolution = service
            .resolve_discard_recovery(&recovery, &cancel)
            .unwrap();
        assert_eq!(resolution.remaining_paths, 1);
        assert!(!recovery.live_recovery.needs_resolution());
        assert!(recovery.can_undo());
        assert!(recovery.undo_deadline_unix_ms().unwrap() > unix_ms());
    }

    #[test]
    fn recovery_directory_frontier_never_chmods_existing_or_unverified_parents() {
        for unverified in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            if unverified {
                fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o777)).unwrap();
            } else {
                fs::create_dir(dir.path().join("parent")).unwrap();
                fs::set_permissions(dir.path().join("parent"), fs::Permissions::from_mode(0o751))
                    .unwrap();
            }
            let root = fs_safety::Root::open(dir.path()).unwrap();
            let mut state = RecoveryDirectoryFrontier::new(&root);
            let modes = BTreeMap::from([(path("parent"), 0o710)]);
            let (parent, _) =
                recovery_parent(&root, &path("parent/file"), &modes, &mut state).unwrap();
            drop(parent);
            let before = fs::metadata(dir.path().join("parent")).unwrap().mode();
            let mut failures = vec![];
            state.flush_before(None, &mut failures);
            assert_eq!(
                fs::metadata(dir.path().join("parent")).unwrap().mode(),
                before
            );
            assert!(state.owned.is_empty());
            if unverified {
                assert!(
                    state
                        .warnings
                        .values()
                        .any(|failure| failure.to_string().contains("unverified"))
                );
            }
        }
    }

    #[test]
    fn recovery_directory_frontier_keeps_original_inode_through_replacement_and_final_chmod() {
        let dir = tempfile::tempdir().unwrap();
        let root = fs_safety::Root::open(dir.path()).unwrap();
        let mut state = RecoveryDirectoryFrontier::new(&root);
        let modes = BTreeMap::from([(path("parent"), 0o500)]);
        let (parent, _) =
            recovery_parent(&root, &path("parent/first"), &modes, &mut state).unwrap();
        let identity = parent.metadata().unwrap().ino();
        drop(parent);
        fs::rename(dir.path().join("parent"), dir.path().join("original")).unwrap();
        fs::create_dir(dir.path().join("parent")).unwrap();
        fs::set_permissions(dir.path().join("parent"), fs::Permissions::from_mode(0o711)).unwrap();
        assert!(recovery_parent(&root, &path("parent/second"), &modes, &mut state).is_err());
        let mut failures = vec![];
        state.flush_before(None, &mut failures);
        assert_eq!(
            fs::metadata(dir.path().join("original")).unwrap().ino(),
            identity
        );
        assert_eq!(
            fs::metadata(dir.path().join("original")).unwrap().mode() & 0o777,
            0o500
        );
        assert_eq!(
            fs::metadata(dir.path().join("parent")).unwrap().mode() & 0o777,
            0o711
        );
        assert!(failures.is_empty(), "{failures:?}");
        fs::set_permissions(
            dir.path().join("original"),
            fs::Permissions::from_mode(0o700),
        )
        .unwrap();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn recovery_directory_frontier_descriptor_use_stays_bounded_across_many_siblings() {
        let dir = tempfile::tempdir().unwrap();
        let root = fs_safety::Root::open(dir.path()).unwrap();
        let mut state = RecoveryDirectoryFrontier::new(&root);
        let mut failures = vec![];
        let baseline = fs::read_dir("/proc/self/fd").unwrap().count();
        for index in 0..256 {
            let file = path(&format!("dir-{index:04}/file"));
            state.flush_before(Some(&file), &mut failures);
            let (parent, _) = recovery_parent(&root, &file, &BTreeMap::new(), &mut state).unwrap();
            drop(parent);
            assert!(state.owned.len() <= 1);
            assert!(
                fs::read_dir("/proc/self/fd").unwrap().count() <= baseline + 12,
                "sibling receipts must release their descriptors"
            );
        }
        state.flush_before(None, &mut failures);
        assert!(failures.is_empty(), "{failures:?}");
        assert!(fs::read_dir("/proc/self/fd").unwrap().count() <= baseline + 2);
    }
}
