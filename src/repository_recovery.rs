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

// Original preimage proofs only. Native Trash transport, move authorization
// and publication remain separate; this module enables no desktop effects.
#[path = "repository/native_trash_preimage.rs"]
mod native_preimage;
#[path = "repository/native_trash.rs"]
pub(super) mod native_trash;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SavedEntry {
    Absent,
    Trashed,
    Unavailable {
        reason: String,
    },
    Blob {
        oid: Oid,
        mode: u32,
        bytes: u64,
        access: SavedFileAccess,
    },
    Symlink {
        target: PathBuf,
    },
}

// Opaque, inert metadata authenticated on the original backup FD. Keeping
// bytes rather than a metadata-template FD preserves bounded descriptor use.
const MAX_ACCESS_ACL_BYTES: usize = 64 * 1024;
const MAX_CAPTURE_ACL_BYTES: usize = 16 * 1024 * 1024;
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SavedFileAccess {
    uid: u32,
    gid: u32,
    acl: Option<Arc<[u8]>>,
}
fn access_version(metadata: &fs::Metadata) -> ((u64, u64, u64, i64, i64, i64, i64), u32, u32, u32) {
    (
        metadata_version(metadata),
        metadata.mode(),
        metadata.uid(),
        metadata.gid(),
    )
}
impl SavedFileAccess {
    fn capture(file: &fs::File) -> Result<Self, String> {
        let before = file.metadata().map_err(|error| error.to_string())?;
        let acl = fs_safety::capture_access_acl(file).map_err(|error| error.to_string())?;
        if acl
            .as_ref()
            .is_some_and(|bytes| bytes.len() > MAX_ACCESS_ACL_BYTES)
        {
            return Err(
                "The original access ACL exceeds the bounded recovery metadata limit".into(),
            );
        }
        let after = file.metadata().map_err(|error| error.to_string())?;
        if !before.is_file() || access_version(&before) != access_version(&after) {
            return Err("The original file access policy changed while being backed up".into());
        }
        Ok(Self {
            uid: before.uid(),
            gid: before.gid(),
            acl: acl.map(Arc::from),
        })
    }
    fn verify(&self, file: &fs::File) -> Result<(), String> {
        let before = file.metadata().map_err(|error| error.to_string())?;
        if (before.uid(), before.gid()) != (self.uid, self.gid)
            || fs_safety::capture_access_acl(file)
                .map_err(|error| error.to_string())?
                .as_deref()
                != self.acl.as_deref()
            || access_version(&before)
                != access_version(&file.metadata().map_err(|error| error.to_string())?)
        {
            return Err(
                "The recovery file's original ownership or access ACL could not be preserved"
                    .into(),
            );
        }
        Ok(())
    }
    fn install(&self, file: &fs::File, mode: u32) -> Result<(), String> {
        fs_safety::restore_ownership(file, self.uid, self.gid)
            .map_err(|error| error.to_string())?;
        file.set_permissions(fs::Permissions::from_mode(mode))
            .map_err(|error| error.to_string())?;
        fs_safety::restore_access_acl(file, self.acl.as_deref())
            .map_err(|error| error.to_string())?;
        self.verify(file)?;
        if file.metadata().map_err(|error| error.to_string())?.mode() & 0o7777 != mode & 0o7777 {
            return Err("The kernel did not preserve the original recovery mode after ownership/access restoration".into());
        }
        Ok(())
    }
}

pub use native_trash::TrashReceipt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscardRecoveryEntry {
    pub path: RepoPath,
    pub before: SavedEntry,
    pub after: SavedEntry,
    pub after_version: Option<(u64, u64, u64, i64, i64, i64, i64)>,
    pub trash: Option<TrashReceipt>,
}

/// Inert, worker-cached disclosure. Neither diagnostic pathname authorizes IO;
/// only the receipt-local nonce selects its already-held original target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoverOriginalFile {
    pub nonce: Uuid,
    pub path: RepoPath,
    pub original_path: PathBuf,
    pub retained_path: PathBuf,
    pub bytes: u64,
    pub mode: u32,
    pub sha256: [u8; 32],
    pub published_unconfirmed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscardRecovery {
    /// Ephemeral exact recovery authority; never serialized or reconstructed
    /// from a diagnostic pathname. Clones share the same bounded payload.
    pub live_recovery: LiveDiscardRecovery,
    custody: Option<Arc<DiscardCustody>>,
    pub worktree: PathBuf,
    pub worktree_identity: (u64, u64),
    pub directories: BTreeMap<RepoPath, u32>,
    pub entries: Vec<DiscardRecoveryEntry>,
    pub expires_unix_ms: u64,
}

// Original physical source and local outside placement stay alive with the
// opaque receipt. It is never serialized or reconstructed from a path.
struct DiscardCustody {
    git: PathBuf,
    executor: Arc<dyn ProcessExecutor>,
    source: WorktreeAuthority,
    context: crate::filesystem_recovery_context::FilesystemRecoveryContext,
}
impl fmt::Debug for DiscardCustody {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DiscardCustody")
            .field("source", &self.source)
            .field("context", &self.context)
            .finish_non_exhaustive()
    }
}
struct OriginalRecoveryExecutor(Arc<dyn ProcessExecutor>);
impl ProcessExecutor for OriginalRecoveryExecutor {
    fn execute(
        &self,
        command: &CommandSpec,
        cancellation: &CancellationToken,
    ) -> Result<ProcessOutput, ProcessFailure> {
        self.0.execute(command, cancellation)
    }
}
impl PartialEq for DiscardCustody {
    fn eq(&self, other: &Self) -> bool {
        self.source.same_scope(&other.source)
    }
}
impl Eq for DiscardCustody {}

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
    originals: Vec<publication::PendingOriginal>,
}
impl LiveDiscardRecovery {
    fn add_original(&self, original: publication::PendingOriginal) {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .originals
            .push(original);
    }
    fn originals(&self) -> Vec<publication::PendingOriginal> {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .originals
            .clone()
    }
    pub fn recoverable_originals(&self) -> Vec<RecoverOriginalFile> {
        // No IO or native capabilities escape to the UI.
        self.originals()
            .into_iter()
            .filter_map(|p| p.disclosure())
            .collect()
    }
    fn record_confirmed_originals(&self) {
        for original in self.originals().into_iter().filter(|p| p.is_confirmed()) {
            self.mark_completed(&original.disclosure.path);
        }
    }
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
    pub(super) fn is_uncertain(&self) -> bool {
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

#[derive(Debug, Clone, PartialEq, Eq)]
struct CheckoutFilePolicy {
    parent_identity: (u64, u64),
    mode: u32,
    access: SavedFileAccess,
}
type CheckoutPolicies = BTreeMap<RepoPath, Result<CheckoutFilePolicy, String>>;

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
        known_checkouts: &[PathBuf],
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
        let root = self.captured_worktree_root(&request.worktree)?;
        let source = self.recovery_source_authority(&request.worktree, &root)?;
        let context =
            crate::filesystem_recovery_context::FilesystemRecoveryContext::acquire_with_owner(
                &root,
                known_checkouts,
                source.clone(),
                source.recovery_descriptor_count(),
            )
            .map_err(|error| ValidationError::Io(error.to_string()))?;
        let custody = Arc::new(DiscardCustody {
            git: self.git.clone(),
            executor: self.creation_executor.clone(),
            source,
            context,
        });
        let directory = root
            .metadata()
            .map_err(|error| ValidationError::Io(error.to_string()))?;
        let worktree_identity = (directory.dev(), directory.ino());
        let tracked = selection_pathspecs(&request.tracked);
        // This is the intended restore source, captured BEFORE any worktree
        // mutation. A later worktree read cannot establish what Git wrote:
        // another editor may already have replaced those bytes.
        let restore_index =
            self.capture_restore_index(&request.worktree, &tracked, cancellation)?;
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
        let mut link_policies = BTreeMap::new();
        for (path, saved) in &before {
            if matches!(saved, SavedEntry::Symlink { .. }) {
                let metadata = fs_safety::metadata_beneath(&root, path.as_path())
                    .map_err(|error| ValidationError::Io(error.to_string()))?;
                if versions.get(path) != Some(&Some(metadata_version(&metadata))) {
                    return Err(ValidationError::Io(
                        "The original symlink changed while capturing its access policy".into(),
                    )
                    .into());
                }
                link_policies.insert(
                    path.clone(),
                    (metadata.mode(), metadata.uid(), metadata.gid()),
                );
            }
        }
        let checkout_policies = self.capture_checkout_policies(&root, &restore_index, cancellation);
        let live_recovery = LiveDiscardRecovery::default();
        let mut native_batch = match native_trash::prepare(
            custody.source.retained_worktree_root(),
            &request.untracked,
            &custody.context,
            &live_recovery,
            cancellation,
        ) {
            Ok(batch) => batch,
            Err(reason) => {
                return Ok(RecoverableDiscard {
                    outcome: MutationOutcome::NotApplied {
                        failure: MutationFailure::Rejected { reason },
                    },
                    recovery: DiscardRecovery {
                        live_recovery,
                        custody: Some(custody),
                        worktree: request.worktree.clone(),
                        worktree_identity,
                        directories,
                        entries: Vec::new(),
                        expires_unix_ms: unix_ms().saturating_add(60_000),
                    },
                });
            }
        };
        let mut trashed = Vec::new();
        let outcome = self.discard_recovery_mutation(
            request,
            cancellation,
            &mut trashed,
            &root,
            &versions,
            &mut native_batch,
            &live_recovery,
        )?;
        let mut entries = if matches!(outcome, MutationOutcome::NotApplied { .. }) {
            Vec::new()
        } else {
            let (mut after, after_versions) = self.capture_recovery_entries(
                &request.worktree,
                &root,
                &tracked,
                false,
                &CancellationToken::new(),
            );
            self.authenticate_restore_entries(
                &request.worktree,
                &before,
                &restore_index,
                &checkout_policies,
                &root,
                &link_policies,
                &after_versions,
                &mut after,
                &CancellationToken::new(),
            );
            changed_recovery_entries(before, &after, &after_versions)
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
                live_recovery,
                custody: Some(custody),
                worktree: request.worktree.clone(),
                worktree_identity,
                directories,
                entries,
                expires_unix_ms: unix_ms().saturating_add(60_000),
            },
        })
    }

    fn capture_restore_index(
        &self,
        worktree: &Path,
        paths: &[RepoPath],
        cancellation: &CancellationToken,
    ) -> Result<BTreeMap<RepoPath, Vec<IndexEntry>>, RepositoryError> {
        let mut entries = BTreeMap::new();
        for batch in bounded_pathspec_batches(paths) {
            let selected = batch.iter().collect::<HashSet<_>>();
            for path in batch {
                entries.entry(path.clone()).or_insert_with(Vec::new);
            }
            let command = self
                .git_literal_read(worktree, ["ls-files", "-s", "-z", "--"])
                .args(batch.iter().map(|path| path.as_os_str()));
            let output = self.run_checked(&command, cancellation)?;
            for entry in invalid_output(parse_index_entries(&output.stdout), output)? {
                // Literal pathspecs still select descendants of a directory.
                // Only an exact selected file can authorize its Undo.
                if selected.contains(&entry.path) {
                    entries.get_mut(&entry.path).unwrap().push(entry);
                }
            }
        }
        Ok(entries)
    }

    fn authenticate_restore_entries(
        &self,
        worktree: &Path,
        before: &BTreeMap<RepoPath, SavedEntry>,
        expected: &BTreeMap<RepoPath, Vec<IndexEntry>>,
        policies: &CheckoutPolicies,
        root: &fs_safety::Root,
        link_policies: &BTreeMap<RepoPath, (u32, u32, u32)>,
        versions: &CaptureVersions,
        after: &mut BTreeMap<RepoPath, Result<SavedEntry, String>>,
        cancellation: &CancellationToken,
    ) {
        let paths = expected.keys().cloned().collect::<Vec<_>>();
        let observed = self.capture_restore_index(worktree, &paths, cancellation);
        let current_policies = self.capture_checkout_policies(root, expected, cancellation);
        for (path, intended) in expected {
            let verified = (|| {
                let saved = after
                    .get(path)
                    .ok_or("The post-discard file was not captured")?
                    .as_ref()
                    .map_err(Clone::clone)?;
                // An exact semantic no-op needs no Undo entry. Git can leave
                // an already-indexed inode in place with its existing policy,
                // instead of creating an inode with checkout creation policy.
                if before.get(path) == Some(saved) {
                    return Ok(());
                }
                if observed.as_ref().map_err(ToString::to_string)?.get(path) != Some(intended) {
                    return Err("The selected index changed during discard".to_owned());
                }
                match (intended.as_slice(), before.get(path), saved) {
                    // Rename expansion may name an old path that is no
                    // longer in the index. Git did not restore it; a change
                    // there cannot become an authorized Undo after-state.
                    ([], Some(original), saved) if original == saved => Ok(()),
                    (
                        [entry],
                        _,
                        SavedEntry::Blob {
                            oid, mode, access, ..
                        },
                    ) if entry.stage == 0 && matches!(entry.mode, 0o100644 | 0o100755) => {
                        // Raw bytes, never a clean-filter hash: distinct newer
                        // worktree contents can normalize to the same index OID.
                        // Git creates a new file with 0666/0777 requested, not
                        // with the discarded file's policy. The original-parent
                        // anonymous probe sampled the actual kernel umask,
                        // ownership and default ACL before any restore effect.
                        let policy = policies
                            .get(path)
                            .ok_or("The original checkout creation policy is unavailable")?
                            .as_ref()
                            .map_err(Clone::clone)?;
                        if current_policies
                            .get(path)
                            .and_then(|policy| policy.as_ref().ok())
                            != Some(policy)
                        {
                            return Err("The original checkout parent or its effective creation policy changed".into());
                        }
                        if oid != &entry.oid || *mode != policy.mode || access != &policy.access {
                            return Err("The restored bytes or access policy do not match the original indexed restore intent".into());
                        }
                        Ok(())
                    }
                    ([entry], Some(SavedEntry::Symlink { .. }), SavedEntry::Symlink { target })
                        if entry.stage == 0 && entry.mode == SYMLINK_MODE =>
                    {
                        let metadata = fs_safety::metadata_beneath(root, path.as_path())
                            .map_err(|error| error.to_string())?;
                        if versions.get(path) != Some(&Some(metadata_version(&metadata)))
                            || link_policies.get(path)
                                != Some(&(metadata.mode(), metadata.uid(), metadata.gid()))
                        {
                            return Err("The restored symlink access policy changed or cannot be authenticated".into());
                        }
                        let output = self
                            .run_checked(
                                &self
                                    .backup_object_command(
                                        worktree,
                                        ["hash-object", "--no-filters", "--stdin"],
                                    )
                                    .stdin(target.as_os_str().as_bytes().to_vec()),
                                cancellation,
                            )
                            .map_err(|error| error.to_string())?;
                        let actual =
                            Oid::parse_object(String::from_utf8_lossy(&output.stdout).trim())
                                .map_err(|error| error.to_string())?;
                        if actual != entry.oid {
                            return Err("The restored symlink target differs from the original indexed restore intent".into());
                        }
                        Ok(())
                    }
                    // Unmerged entries, gitlinks, transformed checkout bytes,
                    // symlinks without an original same-type policy have no
                    // authenticated complete baseline here. Preserve the backup
                    // and refuse Undo instead of adopting a posthoc snapshot.
                    _ => Err(
                        "The exact indexed post-discard file policy could not be authenticated"
                            .into(),
                    ),
                }
            })();
            if let Err(reason) = verified {
                after.insert(path.clone(), Err(format!(
                    "{reason}; newer work was preserved and the original backup remains available"
                )));
            }
        }
    }

    fn capture_checkout_policies(
        &self,
        root: &fs_safety::Root,
        index: &BTreeMap<RepoPath, Vec<IndexEntry>>,
        cancellation: &CancellationToken,
    ) -> CheckoutPolicies {
        let mut policies = BTreeMap::new();
        let mut remaining_acl_bytes = MAX_CAPTURE_ACL_BYTES;
        for (path, entries) in index {
            let [entry] = entries.as_slice() else {
                continue;
            };
            if entry.stage != 0 || !matches!(entry.mode, 0o100644 | 0o100755) {
                continue;
            }
            let policy = (|| {
                if cancellation.is_cancelled() {
                    return Err("Checkout policy capture cancelled".to_owned());
                }
                let (parent, _) = fs_safety::open_parent_beneath(root, path.as_path())
                    .map_err(|error| error.to_string())?;
                let identity = parent.metadata().map_err(|error| error.to_string())?;
                // Git 2.51 entry.c:create_file requests these creation bits;
                // the kernel determines the actual result. Do not chmod the
                // anonymous probe, emulate umask, or substitute a 0700 holding.
                let requested = if entry.mode & 0o100 != 0 {
                    0o777
                } else {
                    0o666
                };
                let probe = fs_safety::create_anonymous_in(&parent, requested)
                    .map_err(|error| error.to_string())?;
                let access = SavedFileAccess::capture(&probe)?;
                let metadata = probe.metadata().map_err(|error| error.to_string())?;
                let acl_bytes = access.acl.as_ref().map_or(0, |acl| acl.len());
                if acl_bytes > remaining_acl_bytes {
                    return Err("Checkout policy ACL capacity is exhausted".into());
                }
                remaining_acl_bytes -= acl_bytes;
                Ok(CheckoutFilePolicy {
                    parent_identity: (identity.dev(), identity.ino()),
                    mode: metadata.mode() & 0o7777,
                    access,
                })
                // The unnamed inode and parent FD close on every path. There
                // is no target effect, cleanup name or per-file retained FD.
            })();
            policies.insert(path.clone(), policy);
        }
        policies
    }

    // Recovery backups must land in the ORIGINAL object database. Do not
    // inherit object-directory/config injections or run filters/hooks while
    // authenticating a preimage. The canonical scoped executor reinstalls
    // its owned original private/common roles after this command is built.
    fn backup_object_command(
        &self,
        worktree: &Path,
        args: impl IntoIterator<Item = impl Into<OsString>>,
    ) -> CommandSpec {
        let mut command = self
            .git_mutation(
                worktree,
                ["--no-replace-objects", "-c", "core.hooksPath=/dev/null"],
            )
            .args(args);
        for (name, _) in std::env::vars_os() {
            if name.as_encoded_bytes().starts_with(b"GIT_") {
                command = command.env_remove(name);
            }
        }
        command
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_NO_LAZY_FETCH", "1")
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
        let mut remaining_acl_bytes = MAX_CAPTURE_ACL_BYTES;
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
            let mut command = self
                .backup_object_command(worktree, ["hash-object", "--no-filters", "--stdin-paths"]);
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
                    let access = SavedFileAccess::capture(file)?;
                    let acl_bytes = access.acl.as_ref().map_or(0, |bytes| bytes.len());
                    if acl_bytes > remaining_acl_bytes {
                        return Err("Recovery access metadata capacity is exhausted; the file access policy could not be retained".into());
                    }
                    remaining_acl_bytes -= acl_bytes;
                    let after = file.metadata().map_err(|error| error.to_string())?;
                    if access_version(opened) != access_version(&after) {
                        return Err("file changed while making its backup".into());
                    }
                    let oid =
                        Oid::parse_object(String::from_utf8_lossy(hashes[index]).into_owned())
                            .map_err(|error| error.to_string())?;
                    Ok(SavedEntry::Blob {
                        oid,
                        mode: opened.mode() & 0o7777,
                        bytes: opened.len(),
                        access,
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
                        .backup_object_command(worktree, ["hash-object", "-w", "--stdin"])
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
        let mut command = self.backup_object_command(worktree, ["hash-object", "--no-filters"]);
        if store {
            command = command.arg("-w");
        }
        let output = self
            .run_checked(&command.arg("--").arg(descriptor_path), cancellation)
            .map_err(|error| error.to_string())?;
        let access = SavedFileAccess::capture(&file)?;
        let after = file.metadata().map_err(|error| error.to_string())?;
        if access_version(&opened) != access_version(&after) {
            return Err("file changed while making its backup".into());
        }
        let oid = Oid::parse_object(String::from_utf8_lossy(&output.stdout).trim())
            .map_err(|error| error.to_string())?;
        Ok(SavedEntry::Blob {
            oid,
            mode: opened.mode() & 0o7777,
            bytes: opened.len(),
            access,
        })
    }

    /// Publish one complete authenticated private blob to its exact original
    /// ABSENT target. Occupied files and replaced parents are never repaired or
    /// overwritten. A known publication is reverified, never attempted twice.
    pub fn recover_original_file(
        &self,
        recovery: &DiscardRecovery,
        nonce: Uuid,
        cancellation: &CancellationToken,
    ) -> Result<DiscardRecoveryResolution, RepositoryError> {
        if !recovery.live_recovery.needs_resolution() {
            return Err(
                ValidationError::Io("This discard recovery is no longer pending".into()).into(),
            );
        }
        let original = recovery
            .live_recovery
            .originals()
            .into_iter()
            .find(|p| p.disclosure.nonce == nonce)
            .ok_or_else(|| {
                ValidationError::Io(
                    "The original-file action expired or belongs to another receipt".into(),
                )
            })?;
        let result = original.recover(cancellation);
        recovery.live_recovery.record_confirmed_originals();
        result.map_err(ValidationError::Io)?;
        // No holding lock is held while entering RecoveryWork -> Holding.
        self.resolve_discard_recovery(recovery, cancellation)
    }

    /// Resolves only retained artifacts/metadata, or confirms a known typed
    /// original-file publication. This does NOT perform the
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
            let result = authority.retry();
            recovery.live_recovery.record_confirmed_originals();
            result.map_err(ValidationError::Io)?;
        }
        // A pending native source rename is completed only after retry has
        // reverified its SAME original inode/content/access at that endpoint.
        // No callback captures the live authority itself (avoids an Arc cycle).
        for entry in &recovery.entries {
            if entry
                .trash
                .as_ref()
                .is_some_and(native_trash::confirmed_restore)
            {
                recovery.live_recovery.mark_completed(&entry.path);
            }
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
        let custody = recovery.custody.as_ref().ok_or_else(|| {
            ValidationError::Io(
                "Undo requires its original worker-owned receipt; no path was undone".into(),
            )
        })?;
        custody.source.validate()?;
        let root = custody.source.retained_worktree_root();
        let context = &custody.context;
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
        context
            .validate_for_source(&root)
            .map_err(|error| ValidationError::Io(error.to_string()))?;
        let source = &custody.source;
        let original_service = RepositoryService::new(
            custody.git.clone(),
            Arc::new(OriginalRecoveryExecutor(custody.executor.clone())),
        )
        .with_worktree_authority(source.clone())?;
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
                original_service.undo_discard_entry(
                    recovery,
                    &root,
                    &source,
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
        source: &WorktreeAuthority,
        entry: &DiscardRecoveryEntry,
        cancellation: &CancellationToken,
        created_directories: &mut RecoveryDirectoryFrontier,
    ) -> Result<Option<String>, String> {
        if let SavedEntry::Unavailable { reason } = &entry.after {
            return Err(format!(
                "the post-discard state could not be verified: {reason}; backup was preserved"
            ));
        }
        let current_version = match fs_safety::metadata_beneath(root, entry.path.as_path()) {
            Ok(metadata) => Some(metadata_version(&metadata)),
            Err(cause) if cause.is_not_found() => None,
            Err(cause) => {
                return Err(format!(
                    "Current source entry cannot be verified: {cause}; no entry was changed"
                ));
            }
        };
        if current_version != entry.after_version {
            return Err("changed since discard; newer work was preserved".into());
        }
        let (parent, name) = recovery_parent(
            root,
            &entry.path,
            &recovery.directories,
            created_directories,
        )?;
        created_directories
            .context
            .validate_for_source(root)
            .map_err(|error| error.to_string())?;
        if let Some(trash) = &entry.trash {
            return native_trash::restore(
                trash,
                recovery,
                &parent,
                &name,
                cancellation,
                &created_directories.context,
            );
        }
        self.undo_saved_entry(
            recovery,
            source,
            entry,
            &parent,
            &name,
            cancellation,
            &created_directories.context,
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
    context: crate::filesystem_recovery_context::FilesystemRecoveryContext,
}

struct OwnedRecoveryDirectory {
    publication: fs_safety::DirectoryPublication,
    mode: u32,
    mode_installed: bool,
    temporary_installed: bool,
    pending_mode: Option<RecoveryModeTransition>,
    sync_file: Option<fs::File>,
    context: crate::filesystem_recovery_context::FilesystemRecoveryContext,
    // The preopened sync FD and original directory close before this charge.
    slot: Option<crate::recovery_authority::RecoverySlot>,
}
/// A chmod may succeed before a subsequent metadata read fails. Retain the
/// intended transition before that effect so retry recognizes its exact
/// introduced after-state without adopting arbitrary current permissions.
struct RecoveryModeTransition {
    before: u32,
    after: u32,
    uid: u32,
    gid: u32,
}

fn recovery_mode_transition(
    directory: &fs_safety::Root,
    before: u32,
    after: u32,
    uid: u32,
    gid: u32,
    pending: &mut Option<RecoveryModeTransition>,
    after_chmod: impl FnOnce() -> io::Result<()>,
) -> Result<(), String> {
    let actual = directory.metadata().map_err(|error| error.to_string())?;
    if (actual.uid(), actual.gid()) != (uid, gid) {
        return Err("The recovery directory ownership changed; its metadata was preserved".into());
    }
    if let Some(transition) = pending.as_ref() {
        if (
            transition.before,
            transition.after,
            transition.uid,
            transition.gid,
        ) != (before & 0o7777, after & 0o7777, uid, gid)
        {
            return Err(
                "The pending recovery permission transition differs from its retained request"
                    .into(),
            );
        }
        if actual.mode() & 0o7777 == transition.after {
            // Earlier fchmod completed, but verification failed. This is the
            // retained intended state, not a freshly adopted mode baseline.
            *pending = None;
            return Ok(());
        }
    }
    if actual.mode() & 0o7777 != before & 0o7777 {
        return Err(
            "The recovery directory permissions changed; current metadata was preserved".into(),
        );
    }
    *pending = Some(RecoveryModeTransition {
        before: before & 0o7777,
        after: after & 0o7777,
        uid,
        gid,
    });
    fs_safety::set_directory_permissions(directory, after).map_err(|error| error.to_string())?;
    after_chmod().map_err(|error| error.to_string())?;
    let actual = directory.metadata().map_err(|error| error.to_string())?;
    if (actual.mode() & 0o7777, actual.uid(), actual.gid()) != (after & 0o7777, uid, gid) {
        return Err("The recovery permission change could not be verified; live transition authority was retained".into());
    }
    *pending = None;
    Ok(())
}

impl OwnedRecoveryDirectory {
    fn install_temporary(&mut self) -> Result<(), String> {
        self.context
            .placement()
            .validate()
            .map_err(|error| error.to_string())?;
        if self.temporary_installed {
            return Ok(());
        }
        // The producer alone restores its temporary staging permissions. Do
        // not overwrite that live witness with a consumer chmod first.
        self.publication
            .retry_publication_steps()
            .map_err(|error| error.to_string())?;
        let initial = self.publication.initial_metadata();
        recovery_mode_transition(
            self.publication.directory(),
            initial.mode(),
            0o700 | (self.mode & 0o2000),
            initial.uid(),
            initial.gid(),
            &mut self.pending_mode,
            || Ok(()),
        )?;
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
        if (self.mode_installed && metadata.mode() & 0o7777 != self.mode & 0o7777)
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
            recovery_mode_transition(
                self.publication.directory(),
                0o700 | (self.mode & 0o2000),
                self.mode,
                initial.uid(),
                initial.gid(),
                &mut self.pending_mode,
                || Ok(()),
            )?;
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
    fn new_with_recovery(
        live: &LiveDiscardRecovery,
        context: &crate::filesystem_recovery_context::FilesystemRecoveryContext,
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
        let retry_context = created.context.clone();
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
                        retry_context
                            .placement()
                            .validate()
                            .map_err(|error| error.to_string())?;
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
                            retry_context.placement().validate().map_err(|error| error.to_string())?;
                            if directory.metadata().map_err(|error|error.to_string())?.nlink() == 0 { Ok(()) }
                            else { Err("The unverified directory is still retained; its entry was not modified".into()) }
                        }));
                        return Err(error.to_string());
                    }
                };
                created.live.retain(slot.retain(description, move || {
                    retry_context.placement().validate().map_err(|error| error.to_string())?;
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
                        pending_mode: None,
                        sync_file: None,
                        context: created.context.clone(),
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
    before: BTreeMap<RepoPath, SavedEntry>,
    after: &BTreeMap<RepoPath, Result<SavedEntry, String>>,
    versions: &CaptureVersions,
) -> Vec<DiscardRecoveryEntry> {
    let mut entries = Vec::new();
    for (path, original) in before {
        match after.get(&path) {
            Some(Ok(after)) if *after != original => {
                let after_version = versions.get(&path).copied().flatten();
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

pub(super) type CaptureVersions = BTreeMap<RepoPath, Option<(u64, u64, u64, i64, i64, i64, i64)>>;

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

#[cfg(all(test, target_os = "linux"))]
mod restore_baseline_tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    struct PrivateUmask(libc::mode_t);
    impl PrivateUmask {
        fn new() -> Self {
            // Only reached in each existing isolated same-ELF owning child.
            // No parent or parallel test shares this process's mask.
            Self(unsafe { libc::umask(0o077) })
        }
    }
    impl Drop for PrivateUmask {
        fn drop(&mut self) {
            unsafe { libc::umask(self.0) };
        }
    }

    fn git(root: &Path, args: &[&str]) -> Vec<u8> {
        let command = CommandSpec::new("git")
            .arg("-C")
            .arg(root)
            .args(args.iter().copied())
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1");
        let output = SystemProcessExecutor
            .execute(&command, &CancellationToken::new())
            .unwrap();
        assert!(
            output.success,
            "Git fixture command failed: {:?}",
            output.stderr
        );
        output.stdout
    }

    #[derive(Clone, Copy)]
    enum Edit {
        None,
        Contents,
        Permissions,
        Index,
        Symlink,
    }

    struct AfterRestore {
        root: PathBuf,
        edit: Edit,
        reached: AtomicBool,
    }
    impl ProcessExecutor for AfterRestore {
        fn execute(
            &self,
            command: &CommandSpec,
            cancellation: &CancellationToken,
        ) -> Result<ProcessOutput, ProcessFailure> {
            let output = SystemProcessExecutor.execute(command, cancellation)?;
            if output.success && command.arguments.iter().any(|arg| arg == "restore") {
                assert!(!self.reached.swap(true, Ordering::SeqCst));
                // Actual Git has already completed its writes. This is the
                // real process-return -> receipt-capture boundary, not a mock
                // restore result or a write after the receipt was published.
                match self.edit {
                    Edit::None => {}
                    Edit::Contents => {
                        fs::write(self.root.join("changed"), b"newer editor work\n").unwrap()
                    }
                    Edit::Permissions => {
                        let path = self.root.join("changed");
                        let mode = fs::metadata(&path).unwrap().mode() & 0o7777;
                        fs::set_permissions(path, fs::Permissions::from_mode(mode ^ 0o020))
                            .unwrap();
                    }
                    Edit::Index => {
                        fs::write(self.root.join("changed"), b"newer staged work\n").unwrap();
                        git(&self.root, &["add", "--", "changed"]);
                    }
                    Edit::Symlink => {
                        fs::remove_file(self.root.join("changed")).unwrap();
                        std::os::unix::fs::symlink("newer-target", self.root.join("changed"))
                            .unwrap();
                    }
                }
            }
            Ok(output)
        }
    }

    fn request(root: &Path, names: &[&str]) -> DiscardRequest {
        DiscardRequest {
            worktree: root.to_owned(),
            tracked: names
                .iter()
                .map(|name| PathSelection::new(RepoPath::parse(name).unwrap()))
                .collect(),
            untracked: vec![],
            confirmed: true,
        }
    }

    #[test]
    fn undo_does_not_adopt_competing_bytes_policy_or_index_after_actual_restore() {
        if crate::repository::test_fixture::isolated_fixture_case(concat!(
            module_path!(),
            "::undo_does_not_adopt_competing_bytes_policy_or_index_after_actual_restore"
        )) {
            return;
        }
        let _private_umask = PrivateUmask::new();
        for edit in [Edit::None, Edit::Contents, Edit::Permissions, Edit::Index] {
            let directory = tempfile::tempdir().unwrap();
            let root = directory.path();
            git(root, &["init", "-q", "-b", "main"]);
            for name in ["changed", "sibling"] {
                fs::write(root.join(name), b"indexed base\n").unwrap();
            }
            git(root, &["add", "--all"]);
            for name in ["changed", "sibling"] {
                fs::write(root.join(name), b"discarded original\n").unwrap();
                fs::set_permissions(root.join(name), fs::Permissions::from_mode(0o640)).unwrap();
            }
            let executor = Arc::new(AfterRestore {
                root: root.to_owned(),
                edit,
                reached: AtomicBool::new(false),
            });
            let service = RepositoryService::new("git", executor.clone())
                .with_worktree_authority(WorktreeAuthority::capture(root).unwrap())
                .unwrap();
            let discarded = service
                .discard_recoverable(
                    &request(root, &["changed", "sibling"]),
                    &CancellationToken::new(),
                    &[],
                )
                .unwrap();
            assert!(executor.reached.load(Ordering::SeqCst));
            assert!(matches!(discarded.outcome, MutationOutcome::Applied { .. }));
            assert_eq!(discarded.recovery.remaining_paths(), 2);
            let entry = discarded
                .recovery
                .entries
                .iter()
                .find(|entry| entry.path == RepoPath::parse("changed").unwrap())
                .unwrap();
            let SavedEntry::Blob { oid, .. } = &entry.before else {
                panic!("original backup must remain held")
            };
            assert_eq!(
                git(root, &["cat-file", "blob", oid.as_str()]),
                b"discarded original\n"
            );
            let current = fs::read(root.join("changed")).unwrap();
            let mode = fs::metadata(root.join("changed")).unwrap().mode();
            let index = git(root, &["ls-files", "-s", "-z"]);
            let undone = service
                .undo_discard(&discarded.recovery, &CancellationToken::new())
                .unwrap();
            assert_eq!(
                fs::read(root.join("sibling")).unwrap(),
                b"discarded original\n"
            );
            if matches!(edit, Edit::None) {
                assert!(!matches!(entry.after, SavedEntry::Unavailable { .. }));
                assert!(matches!(undone, MutationOutcome::Applied { .. }));
                assert_eq!(
                    fs::read(root.join("changed")).unwrap(),
                    b"discarded original\n"
                );
                assert_eq!(
                    fs::metadata(root.join("changed")).unwrap().mode() & 0o7777,
                    0o640
                );
                assert_eq!(discarded.recovery.remaining_paths(), 0);
            } else {
                assert!(matches!(entry.after, SavedEntry::Unavailable { .. }));
                assert_eq!(fs::read(root.join("changed")).unwrap(), current);
                assert_eq!(fs::metadata(root.join("changed")).unwrap().mode(), mode);
                assert_eq!(discarded.recovery.remaining_paths(), 1);
                // A second Undo cannot bless or overwrite the refused path.
                service
                    .undo_discard(&discarded.recovery, &CancellationToken::new())
                    .unwrap();
                assert_eq!(fs::read(root.join("changed")).unwrap(), current);
                assert_eq!(fs::metadata(root.join("changed")).unwrap().mode(), mode);
                assert_eq!(discarded.recovery.remaining_paths(), 1);
            }
            assert_eq!(git(root, &["ls-files", "-s", "-z"]), index);
        }
    }

    #[test]
    fn undo_authenticates_symlink_targets_and_refuses_unproven_checkout_policies() {
        if crate::repository::test_fixture::isolated_fixture_case(concat!(
            module_path!(),
            "::undo_authenticates_symlink_targets_and_refuses_unproven_checkout_policies"
        )) {
            return;
        }
        let _private_umask = PrivateUmask::new();
        for edit in [Edit::None, Edit::Symlink] {
            let directory = tempfile::tempdir().unwrap();
            let root = directory.path();
            git(root, &["init", "-q", "-b", "main"]);
            std::os::unix::fs::symlink("indexed-target", root.join("changed")).unwrap();
            git(root, &["add", "--all"]);
            fs::remove_file(root.join("changed")).unwrap();
            std::os::unix::fs::symlink("discarded-target", root.join("changed")).unwrap();
            let executor = Arc::new(AfterRestore {
                root: root.to_owned(),
                edit,
                reached: AtomicBool::new(false),
            });
            let service = RepositoryService::new("git", executor.clone())
                .with_worktree_authority(WorktreeAuthority::capture(root).unwrap())
                .unwrap();
            let discarded = service
                .discard_recoverable(&request(root, &["changed"]), &CancellationToken::new(), &[])
                .unwrap();
            assert!(executor.reached.load(Ordering::SeqCst));
            assert!(matches!(discarded.outcome, MutationOutcome::Applied { .. }));
            assert_eq!(discarded.recovery.remaining_paths(), 1);
            service
                .undo_discard(&discarded.recovery, &CancellationToken::new())
                .unwrap();
            let expected = if matches!(edit, Edit::None) {
                "discarded-target"
            } else {
                "newer-target"
            };
            assert_eq!(
                fs::read_link(root.join("changed")).unwrap(),
                PathBuf::from(expected)
            );
            assert_eq!(
                discarded.recovery.remaining_paths(),
                usize::from(matches!(edit, Edit::Symlink))
            );
        }
        // A no-op keeps its preexisting policy, which need not equal a newly
        // created checkout's private-umask policy. It must not mint a refused
        // recovery obligation for an unchanged file.
        {
            let directory = tempfile::tempdir().unwrap();
            let root = directory.path();
            git(root, &["init", "-q", "-b", "main"]);
            fs::write(root.join("changed"), b"indexed base\n").unwrap();
            fs::set_permissions(root.join("changed"), fs::Permissions::from_mode(0o640)).unwrap();
            git(root, &["add", "--all"]);
            let original = fs::metadata(root.join("changed")).unwrap();
            let service = RepositoryService::system("git")
                .with_worktree_authority(WorktreeAuthority::capture(root).unwrap())
                .unwrap();
            let discarded = service
                .discard_recoverable(&request(root, &["changed"]), &CancellationToken::new(), &[])
                .unwrap();
            assert!(matches!(discarded.outcome, MutationOutcome::Applied { .. }));
            assert_eq!(fs::read(root.join("changed")).unwrap(), b"indexed base\n");
            assert_eq!(
                metadata_version(&fs::metadata(root.join("changed")).unwrap()),
                metadata_version(&original)
            );
            assert!(discarded.recovery.entries.is_empty());
            assert!(!discarded.recovery.can_undo());
        }
        for policy in ["deleted", "type-change", "crlf", "exec-reset"] {
            let directory = tempfile::tempdir().unwrap();
            let root = directory.path();
            git(root, &["init", "-q", "-b", "main"]);
            fs::write(root.join("changed"), b"indexed base\n").unwrap();
            if policy == "exec-reset" {
                fs::set_permissions(root.join("changed"), fs::Permissions::from_mode(0o755))
                    .unwrap();
            }
            git(root, &["add", "--all"]);
            match policy {
                "deleted" => fs::remove_file(root.join("changed")).unwrap(),
                "type-change" => {
                    fs::remove_file(root.join("changed")).unwrap();
                    std::os::unix::fs::symlink("discarded-target", root.join("changed")).unwrap();
                }
                "crlf" => {
                    git(root, &["config", "core.autocrlf", "true"]);
                    fs::write(root.join("changed"), b"discarded original\n").unwrap();
                }
                "exec-reset" => {
                    fs::set_permissions(root.join("changed"), fs::Permissions::from_mode(0o644))
                        .unwrap();
                    fs::write(root.join("changed"), b"discarded original\n").unwrap();
                }
                _ => unreachable!(),
            }
            let service = RepositoryService::system("git")
                .with_worktree_authority(WorktreeAuthority::capture(root).unwrap())
                .unwrap();
            let discarded = service
                .discard_recoverable(&request(root, &["changed"]), &CancellationToken::new(), &[])
                .unwrap();
            assert!(matches!(discarded.outcome, MutationOutcome::Applied { .. }));
            assert_eq!(discarded.recovery.remaining_paths(), 1);
            assert_eq!(
                matches!(
                    discarded.recovery.entries[0].after,
                    SavedEntry::Unavailable { .. }
                ),
                policy == "crlf",
                "{policy}"
            );
            let current = fs::read(root.join("changed")).unwrap();
            let metadata = fs::metadata(root.join("changed")).unwrap();
            let undone = service
                .undo_discard(&discarded.recovery, &CancellationToken::new())
                .unwrap();
            match policy {
                "crlf" => {
                    assert!(matches!(undone, MutationOutcome::NotApplied { .. }));
                    assert_eq!(fs::read(root.join("changed")).unwrap(), current);
                    assert_eq!(
                        metadata_version(&fs::metadata(root.join("changed")).unwrap()),
                        metadata_version(&metadata)
                    );
                    assert_eq!(discarded.recovery.remaining_paths(), 1);
                }
                "deleted" => {
                    assert!(matches!(undone, MutationOutcome::Applied { .. }));
                    assert!(
                        fs::symlink_metadata(root.join("changed"))
                            .is_err_and(|error| error.kind() == io::ErrorKind::NotFound)
                    );
                    assert_eq!(discarded.recovery.remaining_paths(), 0);
                }
                "type-change" => {
                    assert!(matches!(undone, MutationOutcome::Applied { .. }));
                    assert_eq!(
                        fs::read_link(root.join("changed")).unwrap(),
                        PathBuf::from("discarded-target")
                    );
                    assert_eq!(discarded.recovery.remaining_paths(), 0);
                }
                "exec-reset" => {
                    assert!(matches!(undone, MutationOutcome::Applied { .. }));
                    assert_eq!(
                        fs::read(root.join("changed")).unwrap(),
                        b"discarded original\n"
                    );
                    assert_eq!(
                        fs::metadata(root.join("changed")).unwrap().mode() & 0o7777,
                        0o644
                    );
                    assert_eq!(discarded.recovery.remaining_paths(), 0);
                }
                _ => unreachable!(),
            }
        }
    }
}
