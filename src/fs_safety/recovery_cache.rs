//! Worker-only retained-parent discovery and per-operation recovery holdings.
//! Candidate discovery reads retained ancestors only. A candidate is never a
//! placement or write grant: the worker must obtain real physical placement and cleanup proof
//! before constructing the FilesystemRecoveryContext passed to the factory.
use super::*;
use crate::{
    filesystem_recovery_context::{FilesystemRecoveryContext, FilesystemRecoveryReservation},
    recovery_authority::{MAX_RECOVERY_DESCRIPTORS, RecoveryAuthority, RecoverySlot},
};
use std::{os::unix::fs::MetadataExt, sync::Arc};

/// Recheck the original holding against the factory's trusted post-setup
/// observation. This checks the held inode only: it neither repairs permissions
/// nor proves current placement, ACL policy or atomicity with a later effect.
/// Directory timestamps are deliberately excluded because payload work changes them.
pub(crate) fn verify_recovery_directory_private(
    directory: &Root,
    expected: &std::fs::Metadata,
) -> io::Result<()> {
    let current = directory.metadata()?;
    if !expected.is_dir()
        || expected.uid() != unsafe { libc::geteuid() }
        || expected.mode() & 0o7777 != 0o700
        || !current.is_dir()
        || current.dev() != expected.dev()
        || current.ino() != expected.ino()
        || current.uid() != expected.uid()
        || current.gid() != expected.gid()
        || current.mode() & 0o7777 != expected.mode() & 0o7777
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "Recovery holding identity or private permissions changed",
        ));
    }
    Ok(())
}

const MAX_CANDIDATES: usize = 8;
const MAX_PARENT_WALK: usize = 256;

/// At most eight protected and eight sticky original held candidates and a
/// bounded256-edge walk. Sticky candidates do not consume the protected-parent
/// allowance that existed before sibling bootstrap support. Retain
/// and charge yielded handles in the actual domain intake; prefer trying one
/// at a time. No pathname is reopened and no directory is created here.
pub struct RecoveryParentCandidates {
    cursor: Arc<Root>,
    source: RecoveryDirectoryStamp,
    walked: usize,
    yielded: usize,
    sticky_yielded: usize,
    finished: bool,
}
impl RecoveryParentCandidates {
    pub fn new(source: Arc<Root>) -> io::Result<Self> {
        let stamp = recovery_directory_stamp(&source)?;
        Ok(Self {
            cursor: source,
            source: stamp,
            walked: 0,
            yielded: 0,
            sticky_yielded: 0,
            finished: false,
        })
    }
}
impl Iterator for RecoveryParentCandidates {
    type Item = io::Result<Arc<Root>>;
    fn next(&mut self) -> Option<Self::Item> {
        if self.finished || self.yielded == MAX_CANDIDATES {
            return None;
        }
        while self.walked < MAX_PARENT_WALK {
            self.walked += 1;
            let step = (|| -> io::Result<Option<(Arc<Root>, bool)>> {
                let previous = recovery_directory_stamp(&self.cursor)?;
                let parent = Arc::new(retained_parent_directory(&self.cursor)?);
                let current = recovery_directory_stamp(&parent)?;
                if current == previous
                    || current.device != self.source.device
                    || current.mount_id != self.source.mount_id
                {
                    self.finished = true;
                    return Ok(None);
                }
                self.cursor = parent.clone();
                match DirectoryNamespace::acquire(&parent) {
                    Ok(namespace) => {
                        drop(namespace);
                        Ok(Some((parent, false)))
                    }
                    Err(error)
                        if matches!(
                            error.kind(),
                            io::ErrorKind::PermissionDenied | io::ErrorKind::Unsupported
                        ) =>
                    {
                        // A sticky namespace grants only a fresh exclusive
                        // sibling bootstrap, never ordinary publication.
                        if self.sticky_yielded < MAX_CANDIDATES
                            && StickyParent::acquire(parent.clone()).is_ok()
                        {
                            Ok(Some((parent, true)))
                        } else {
                            Ok(None)
                        }
                    }
                    Err(error) => Err(error),
                }
            })();
            match step {
                Ok(Some((parent, sticky))) => {
                    if sticky {
                        self.sticky_yielded += 1;
                    } else {
                        self.yielded += 1;
                    }
                    return Some(Ok(parent));
                }
                Ok(None) if !self.finished => continue,
                Ok(None) => return None,
                Err(error) => {
                    self.finished = true;
                    return Some(Err(error));
                }
            }
        }
        self.finished = true;
        Some(Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Recovery ancestor discovery exceeded its bounded parent inspection",
        )))
    }
}
impl std::iter::FusedIterator for RecoveryParentCandidates {}

/// The ACTUAL per-operation holding H, not an intermediate cache parent.
/// Its original publication, held parent context and SAME pre-effect slot
/// travel together until the consumer transfers all three into its live owner.
/// Dropping this value never removes names or claims successful cleanup.
#[must_use = "retain the publication and its existing reservation through actual resolution"]
pub(crate) struct RecoveryDirectory<T = ()> {
    publication: DirectoryPublication,
    owner: T,
    private_metadata: std::fs::Metadata,
    context: FilesystemRecoveryContext,
    slot: RecoverySlot,
}
impl<T> std::fmt::Debug for RecoveryDirectory<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RecoveryDirectory")
            .field("publication", &self.publication)
            .field("context", &self.context)
            .finish_non_exhaustive()
    }
}
impl<T> RecoveryDirectory<T> {
    pub(crate) fn directory(&self) -> &Root {
        self.publication.directory()
    }
    pub(crate) fn context(&self) -> &FilesystemRecoveryContext {
        &self.context
    }
    /// Actual held-FD observation after private setup, not mkdir's umask-applied
    /// initial mode. Consumers must revalidate before later permission effects.
    pub(crate) fn private_metadata(&self) -> &std::fs::Metadata {
        &self.private_metadata
    }
    /// Transfer ALL authority into the lower Holding/Undo owner. No uncharged
    /// bare-Root conversion or second reservation is provided. A destructuring
    /// caller must immediately form its aggregate, with native owners before
    /// the slot, before fallible work: separate locals drop in reverse order.
    pub(crate) fn into_parts(
        self,
    ) -> (
        DirectoryPublication,
        T,
        std::fs::Metadata,
        FilesystemRecoveryContext,
        RecoverySlot,
    ) {
        // Normal direct Drop of this transfer tuple must also close T's native
        // owners before the reservation/creation guard releases its charge.
        (
            self.publication,
            self.owner,
            self.private_metadata,
            self.context,
            self.slot,
        )
    }
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum RecoveryDirectoryError {
    #[error("Recovery holding preparation was refused: {0}")]
    Rejected(#[source] io::Error),
    #[error("Recovery holding preparation failed: {cause}; retained recovery: {recovery:?}")]
    Retained {
        #[source]
        cause: io::Error,
        recovery: RecoveryAuthority,
    },
}
impl RecoveryDirectoryError {
    pub(crate) fn recovery(&self) -> Option<&RecoveryAuthority> {
        match self {
            Self::Retained { recovery, .. } => Some(recovery),
            Self::Rejected(_) => None,
        }
    }
}

// Keep incoming native owners charged across ordinary rejection AND callback
// unwind. This ordering does not turn an effectful panic into a recovery receipt.
struct ReservedDirectoryOwner<T> {
    owner: T,
    context: FilesystemRecoveryContext,
    slot: RecoverySlot,
}

struct PendingDirectory<T> {
    publication: DirectoryPublication,
    owner: T,
    private_metadata: Option<std::fs::Metadata>,
    context: FilesystemRecoveryContext,
    setup_attempted: bool,
    setup_complete: bool,
}
impl<T> PendingDirectory<T> {
    fn initialize_private_mode(&mut self) -> io::Result<()> {
        self.publication
            .retry_publication_steps()
            .map_err(io::Error::other)?;
        let initial = self.publication.initial_metadata();
        let current = self.publication.directory().metadata()?;
        let mode = current.mode() & 0o7777;
        if current.uid() != initial.uid()
            || current.gid() != initial.gid()
            || (self.setup_complete && mode != 0o700)
            || (!self.setup_complete
                && mode != initial.mode() & 0o7777
                && !(self.setup_attempted && mode == 0o700))
        {
            return Err(io::Error::other(
                "Published holding permissions changed; newer observed state was preserved",
            ));
        }
        if mode != 0o700 {
            // This is an explicit PRIVATE holding policy, applied only to the
            // original retained Published inode after producer pending work.
            self.setup_attempted = true;
            set_directory_permissions(self.publication.directory(), 0o700)?;
        }
        let checked = self.publication.directory().metadata()?;
        if checked.uid() != initial.uid()
            || checked.gid() != initial.gid()
            || checked.mode() & 0o7777 != 0o700
        {
            return Err(io::Error::other(
                "Kernel did not preserve exact private holding ownership/mode",
            ));
        }
        self.setup_complete = true;
        // Includes native ACL absence where that adapter is supported. This
        // wrapper does not claim Darwin placement or creation qualification.
        drop(DirectoryNamespace::acquire(self.publication.directory())?);
        self.private_metadata = Some(checked);
        Ok(())
    }
    fn qualify_setup(&mut self) -> io::Result<()> {
        // No payload exists yet. Resolve producer pending work, then apply
        // witnessed private access before ancestry walking an umask000 inode.
        self.initialize_private_mode()?;
        self.context
            .validate_payload_root(self.publication.directory())
    }
    fn confirm_directory_durability(
        &self,
        policy: PreparationPolicy,
        mut sync: impl FnMut(&Root) -> io::Result<()>,
    ) -> io::Result<()> {
        let holding = sync(self.publication.directory());
        if matches!(policy, PreparationPolicy::Strict) {
            holding?;
            return sync(self.context.placement().directory());
        }
        // Save observes both directory durability steps. Retain the first
        // error while still attempting the parent confirmation.
        let parent = sync(self.context.placement().directory());
        holding.and(parent)
    }
    fn retry_failed_preparation(&mut self, policy: PreparationPolicy) -> Result<(), String> {
        if !self.publication.is_removed() {
            self.initialize_private_mode().map_err(|e| e.to_string())?;
            self.context
                .validate_payload_root(self.publication.directory())
                .map_err(|e| e.to_string())?;
            self.publication
                .resolve_recovery()
                .map_err(|e| e.to_string())?;
            self.publication
                .cleanup_empty()
                .map_err(|e| e.to_string())?;
        }
        // Once the original is positively removed, no unlinked ancestry or
        // permissions are consulted; only the retained parent's durability.
        confirm_removed_preparation_parent(policy, self.context.placement().directory())
    }
}

/// Create the actual per-operation H inside its already-held outside
/// parent P. The caller reserves via context.reserve_directory BEFORE effects
/// and transfers that exact non-cloneable admission here. No H0, derived
/// placement, extra ledger, source-file acquisition or path-based authority.
/// Existing/Unverified directories never confer owned cleanup or file rights.
pub(crate) fn prepare_recovery_directory(
    admission: FilesystemRecoveryReservation,
    leaf: &OsStr,
    cancelled: impl Fn() -> bool,
) -> Result<RecoveryDirectory, RecoveryDirectoryError> {
    prepare_recovery_directory_with_owner(admission, leaf, cancelled, ())
}

/// Preserve the lower producer's already-pinned source/file/parent fields in
/// this SAME reservation on every post-effect failure. No second slot, owner
/// registry or loss of original descriptors while returning a recovery error.
/// On success into_parts transfers the owner together with all three guards.
/// A consumer initializes its private-mode phase from initial UID/GID plus
/// INTENDED700, not initial_metadata.mode(): the factory already established700.
pub(crate) fn prepare_recovery_directory_with_owner<T: Send + 'static>(
    admission: FilesystemRecoveryReservation,
    leaf: &OsStr,
    cancelled: impl Fn() -> bool,
    owner: T,
) -> Result<RecoveryDirectory<T>, RecoveryDirectoryError> {
    prepare_recovery_directory_using(
        admission,
        leaf,
        cancelled,
        owner,
        create_directory_in_namespace,
    )
}

/// Directory preparation durability only. Unconfirmed does not weaken
/// private identity, publication, placement or regular-file sync requirements.
/// Consumers must retain this warning through the final Save result; later
/// directory-sync success alone does not erase this missing preparation proof.
#[derive(Debug)]
#[must_use = "Save preparation durability must propagate to the final Save outcome"]
pub(crate) enum SavePreparationDurability {
    Confirmed,
    Unconfirmed(io::Error),
}

/// Save-only live custody of an actually qualified fresh holding, even when
/// confirming its directory durability fails. General recovery remains strict.
pub(crate) fn prepare_save_recovery_directory_with_owner<T: Send + 'static>(
    admission: FilesystemRecoveryReservation,
    leaf: &OsStr,
    cancelled: impl Fn() -> bool,
    owner: T,
) -> Result<(RecoveryDirectory<T>, SavePreparationDurability), RecoveryDirectoryError> {
    prepare_directory_using(
        admission,
        leaf,
        cancelled,
        owner,
        PreparationPolicy::SaveLiveCustody,
        create_directory_in_namespace,
        sync_save_preparation_directory,
    )
}

fn sync_save_preparation_directory(directory: &Root) -> io::Result<()> {
    #[cfg(test)]
    if FAIL_NEXT_SAVE_PREPARATION_SYNC.with(|pending| pending.replace(false)) {
        return Err(io::Error::other(
            "injected Save preparation directory sync failure",
        ));
    }
    sync_directory(directory)
}

#[cfg(test)]
std::thread_local! {
    static FAIL_NEXT_SAVE_PREPARATION_SYNC: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Hold this thread-bound guard through the actual Save call. Only the next
/// Save factory preparation-directory sync fails; all later syncs are real.
/// Dropping the guard clears an unconsumed fault after early qualification
/// failure, so another test on this worker thread cannot inherit it.
#[cfg(test)]
#[must_use = "retain the fault guard through the actual Save operation"]
pub(crate) fn fixture_fail_next_save_preparation_sync() -> impl Drop {
    struct FaultGuard(std::marker::PhantomData<std::rc::Rc<()>>);
    impl Drop for FaultGuard {
        fn drop(&mut self) {
            FAIL_NEXT_SAVE_PREPARATION_SYNC.with(|pending| pending.set(false));
        }
    }
    assert!(
        !FAIL_NEXT_SAVE_PREPARATION_SYNC.with(|pending| pending.replace(true)),
        "Save preparation directory-sync fault is already armed on this thread",
    );
    FaultGuard(std::marker::PhantomData)
}

#[derive(Clone, Copy)]
enum PreparationPolicy {
    Strict,
    SaveLiveCustody,
}

/// Only call after positive namespace removal or successful original staging
/// cleanup. Save failures already remain ordinary errors; a missing final
/// directory durability proof must not invent live custody of a removed object.
fn confirm_removed_preparation_parent(
    policy: PreparationPolicy,
    parent: &Root,
) -> Result<(), String> {
    match sync_removed_preparation_parent(parent) {
        Ok(()) => Ok(()),
        Err(error) => match policy {
            PreparationPolicy::Strict => Err(error.to_string()),
            PreparationPolicy::SaveLiveCustody => {
                tracing::warn!(error = %error,
                    "Save preparation cleanup removed its artifact but directory durability is unconfirmed");
                Ok(())
            }
        },
    }
}
fn retry_retained_preparation(
    policy: PreparationPolicy,
    context: &FilesystemRecoveryContext,
    preparation: &mut DirectoryPreparation,
) -> Result<(), String> {
    // Only positively completed original staging cleanup can bypass a stale
    // Save context. Unknown, nonempty or quarantined staging stays fenced.
    if !(matches!(policy, PreparationPolicy::SaveLiveCustody) && preparation.is_resolved()) {
        context.placement().validate().map_err(|e| e.to_string())?;
    }
    preparation.retry_cleanup().map_err(|e| e.to_string())?;
    confirm_removed_preparation_parent(policy, context.placement().directory())
}

fn sync_removed_preparation_parent(parent: &Root) -> io::Result<()> {
    #[cfg(test)]
    if FAIL_NEXT_PREPARATION_CLEANUP_SYNC.with(|pending| pending.replace(false)) {
        return Err(io::Error::other(
            "injected removed-preparation directory sync failure",
        ));
    }
    sync_directory(parent)
}

#[cfg(test)]
std::thread_local! {
    static FAIL_NEXT_PREPARATION_CLEANUP_SYNC: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}
#[cfg(test)]
fn fixture_fail_next_preparation_cleanup_sync() -> impl Drop {
    struct FaultGuard(std::marker::PhantomData<std::rc::Rc<()>>);
    impl Drop for FaultGuard {
        fn drop(&mut self) {
            FAIL_NEXT_PREPARATION_CLEANUP_SYNC.with(|pending| pending.set(false));
        }
    }
    assert!(!FAIL_NEXT_PREPARATION_CLEANUP_SYNC.with(|pending| pending.replace(true)));
    FaultGuard(std::marker::PhantomData)
}

fn prepare_recovery_directory_using<T: Send + 'static>(
    admission: FilesystemRecoveryReservation,
    leaf: &OsStr,
    cancelled: impl Fn() -> bool,
    owner: T,
    create: impl FnOnce(
        &DirectoryNamespace,
        &Root,
        &Path,
        u32,
    ) -> Result<DirectoryCreation, DirectoryCreateError>,
) -> Result<RecoveryDirectory<T>, RecoveryDirectoryError> {
    prepare_directory_using(
        admission,
        leaf,
        cancelled,
        owner,
        PreparationPolicy::Strict,
        create,
        sync_directory,
    )
    .map(|(directory, _)| directory)
}

fn prepare_directory_using<T: Send + 'static>(
    admission: FilesystemRecoveryReservation,
    leaf: &OsStr,
    cancelled: impl Fn() -> bool,
    owner: T,
    policy: PreparationPolicy,
    create: impl FnOnce(
        &DirectoryNamespace,
        &Root,
        &Path,
        u32,
    ) -> Result<DirectoryCreation, DirectoryCreateError>,
    sync: impl FnMut(&Root) -> io::Result<()>,
) -> Result<(RecoveryDirectory<T>, SavePreparationDurability), RecoveryDirectoryError> {
    let (context, slot) = admission.into_parts();
    let reserved = ReservedDirectoryOwner {
        owner,
        context,
        slot,
    };
    if components(Path::new(leaf)).is_none_or(|parts| parts.len() != 1) {
        return Err(RecoveryDirectoryError::Rejected(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Recovery holding requires one ordinary leaf name",
        )));
    }
    if let Err(cause) = reserved.context.placement().validate() {
        return Err(RecoveryDirectoryError::Rejected(cause));
    }
    if cancelled() {
        return Err(RecoveryDirectoryError::Rejected(io::Error::new(
            io::ErrorKind::Interrupted,
            "Recovery holding creation cancelled before any effect",
        )));
    }
    // Qualified producer retains <=4 private handles plus bounded transient
    // work, inside this existing16-private-FD reservation. Shared context and
    // captured known-root handles stay under the measured128-FD admission, not this bound.
    const {
        assert!(MAX_RECOVERY_DESCRIPTORS >= 16);
    }
    let publication = match create(
        reserved.context.placement().namespace(),
        reserved.context.placement().directory(),
        Path::new(leaf),
        0o700,
    ) {
        Ok(DirectoryCreation::Published(publication)) => publication,
        Ok(DirectoryCreation::Existing(_)) => {
            return Err(RecoveryDirectoryError::Rejected(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "Recovery holding already exists; it was left untouched",
            )));
        }
        Ok(DirectoryCreation::Unverified { directory, reason }) => {
            let cause = io::Error::other(format!(
                "Recovery holding has no creation authority: {reason}"
            ));
            let ReservedDirectoryOwner {
                owner,
                context,
                slot,
            } = reserved;
            let recovery = slot.retain("Unverified recovery holding was left untouched".into(), move || {
                // Preserve the observed Root and the original original local ownership.
                // No automatic deletion/chmod or later ownership conversion.
                let _ = &owner;
                if matches!(policy, PreparationPolicy::SaveLiveCustody)
                    && directory.metadata().map_err(|e| e.to_string())?.nlink() == 0
                {
                    // Observed unverified custody is already unlinked. No
                    // factory-owned artifact or user-file state needs a live
                    // namespace grant; a stale source must not invent one.
                    return confirm_removed_preparation_parent(policy, context.placement().directory());
                }
                context.placement().validate().map_err(|e| e.to_string())?;
                if directory.metadata().map_err(|e| e.to_string())?.nlink() == 0 {
                    return confirm_removed_preparation_parent(policy, context.placement().directory());
                }
                Err("Unverified holding remains linked and requires external resolution; no ownership cleanup was attempted".into())
            });
            return Err(RecoveryDirectoryError::Retained { cause, recovery });
        }
        Err(mut error) => {
            let cause = io::Error::other(error.to_string());
            let Some(mut preparation) = error.take_recovery() else {
                return Err(RecoveryDirectoryError::Rejected(cause));
            };
            let ReservedDirectoryOwner {
                owner,
                context,
                slot,
            } = reserved;
            let recovery = slot.retain(
                "Private holding staging requires retained cleanup".into(),
                move || {
                    let _ = &owner;
                    retry_retained_preparation(policy, &context, &mut preparation)
                },
            );
            return Err(RecoveryDirectoryError::Retained { cause, recovery });
        }
    };
    let ReservedDirectoryOwner {
        owner,
        context,
        slot,
    } = reserved;
    let mut pending = PendingDirectory {
        publication,
        owner,
        private_metadata: None,
        context,
        setup_attempted: false,
        setup_complete: false,
    };
    let result = if cancelled() {
        Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "Recovery holding creation cancelled after publication; original authority retained",
        ))
    } else {
        pending.qualify_setup()
    };
    if let Err(cause) = result {
        let recovery = slot.retain(
            "Recovery holding initialization retains its original directory".into(),
            move || pending.retry_failed_preparation(policy),
        );
        return Err(RecoveryDirectoryError::Retained { cause, recovery });
    }
    let durability = match pending.confirm_directory_durability(policy, sync) {
        Ok(()) => SavePreparationDurability::Confirmed,
        Err(cause) => match policy {
            PreparationPolicy::Strict => {
                let recovery = slot.retain(
                    "Recovery holding initialization retains its original directory".into(),
                    move || pending.retry_failed_preparation(policy),
                );
                return Err(RecoveryDirectoryError::Retained { cause, recovery });
            }
            PreparationPolicy::SaveLiveCustody => SavePreparationDurability::Unconfirmed(cause),
        },
    };
    Ok((
        RecoveryDirectory {
            publication: pending.publication,
            owner: pending.owner,
            private_metadata: pending
                .private_metadata
                .expect("successful private setup records actual metadata"),
            context: pending.context,
            slot,
        },
        durability,
    ))
}

/// Same factory preflight/error ownership path, with the real producer's
/// deterministic protected-INTERNAL acquisition failure. Test-only: no forged
/// receipt, second slot, alternate cleanup, or production authority bypass.
#[cfg(all(test, target_os = "linux"))]
pub(crate) fn fixture_recovery_directory_preparation_failure<T: Send + 'static>(
    admission: FilesystemRecoveryReservation,
    owner: T,
) -> RecoveryDirectoryError {
    prepare_recovery_directory_using(
        admission,
        OsStr::new("fixture-holding"),
        || false,
        owner,
        |_, actual_parent, _, _| Err(fixture_directory_preparation_failure(actual_parent)),
    )
    .unwrap_err()
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use crate::filesystem_recovery_context::fixtures::fixture_context;
    use std::sync::atomic::{AtomicBool, Ordering};

    fn cleanup_directory<T>(directory: RecoveryDirectory<T>) {
        let (mut publication, owner, _, context, slot) = directory.into_parts();
        publication.cleanup_empty().unwrap();
        sync_directory(context.placement().directory()).unwrap();
        drop(publication);
        drop(owner);
        drop(context);
        drop(slot);
    }

    #[test]
    fn directory_sync_failure_is_strict_for_general_recovery_and_a_typed_save_warning() {
        for failure_on in [1, 2] {
            for policy in [
                PreparationPolicy::Strict,
                PreparationPolicy::SaveLiveCustody,
            ] {
                let source = tempfile::tempdir().unwrap();
                let source_root = Root::open(source.path()).unwrap();
                let context = fixture_context(&source_root).unwrap();
                let owner = Arc::new(tempfile::tempfile().unwrap());
                let witness = Arc::downgrade(&owner);
                let mut sync_calls = 0;
                let result = prepare_directory_using(
                    context.reserve_directory().unwrap(),
                    OsStr::new("holding"),
                    || false,
                    owner,
                    policy,
                    create_directory_in_namespace,
                    |_| {
                        sync_calls += 1;
                        if sync_calls == failure_on {
                            Err(io::Error::other("injected directory durability failure"))
                        } else {
                            Ok(())
                        }
                    },
                );
                assert!(
                    witness.upgrade().is_some(),
                    "same original owner remains held"
                );
                match policy {
                    PreparationPolicy::Strict => {
                        let RecoveryDirectoryError::Retained { recovery, .. } = result.unwrap_err()
                        else {
                            panic!("general recovery must retain failed directory setup");
                        };
                        assert_eq!(sync_calls, failure_on);
                        drop(context);
                        recovery.retry().unwrap();
                        assert!(witness.upgrade().is_none());
                    }
                    PreparationPolicy::SaveLiveCustody => {
                        let (directory, durability) = result.unwrap();
                        assert_eq!(sync_calls, 2, "Save still attempts both directory syncs");
                        assert!(
                            matches!(&durability, SavePreparationDurability::Unconfirmed(error)
                            if error.to_string() == "injected directory durability failure")
                        );
                        verify_recovery_directory_private(
                            directory.directory(),
                            directory.private_metadata(),
                        )
                        .unwrap();
                        directory
                            .context()
                            .validate_payload_root(directory.directory())
                            .unwrap();
                        // Later success does not rewrite the preparation proof.
                        sync_directory(directory.directory()).unwrap();
                        sync_directory(directory.context().placement().directory()).unwrap();
                        assert!(matches!(
                            durability,
                            SavePreparationDurability::Unconfirmed(_)
                        ));
                        cleanup_directory(directory);
                        assert!(witness.upgrade().is_none());
                    }
                }
            }
        }
    }

    #[test]
    fn failed_preparation_cleanup_retires_save_custody_only_after_positive_removal() {
        for staging_failure in [false, true] {
            for policy in [
                PreparationPolicy::Strict,
                PreparationPolicy::SaveLiveCustody,
            ] {
                let source = tempfile::tempdir().unwrap();
                let source_root = Root::open(source.path()).unwrap();
                let context = fixture_context(&source_root).unwrap();
                let owner = Arc::new(tempfile::tempfile().unwrap());
                let witness = Arc::downgrade(&owner);
                let error = prepare_directory_using(
                    context.reserve_directory().unwrap(),
                    OsStr::new("holding"),
                    || false,
                    owner,
                    policy,
                    |namespace, parent, relative, mode| {
                        if staging_failure {
                            return Err(fixture_directory_preparation_failure(parent));
                        }
                        let created =
                            create_directory_in_namespace(namespace, parent, relative, mode)?;
                        set_directory_permissions(created.directory(), 0o750)
                            .expect("inject changed mode on the real fresh holding");
                        Ok(created)
                    },
                    |_| panic!("failed qualification must precede directory confirmation"),
                )
                .unwrap_err();
                let RecoveryDirectoryError::Retained { recovery, .. } = error else {
                    panic!("real failed preparation must retain original owner and custody");
                };
                if staging_failure {
                    let artifact = std::fs::read_dir(context.placement().directory().path())
                        .unwrap()
                        .map(|entry| entry.unwrap().path().join("directory"))
                        .find(|path| path.is_file())
                        .expect("real producer's partial artifact");
                    std::fs::remove_file(artifact).unwrap();
                } else {
                    let holding =
                        resolve_directory(context.placement().directory(), Path::new("holding"))
                            .unwrap();
                    set_directory_permissions(&holding, 0o700).unwrap();
                }
                let _fault = fixture_fail_next_preparation_cleanup_sync();
                let retry = recovery.retry();
                assert_eq!(
                    std::fs::read_dir(context.placement().directory().path())
                        .unwrap()
                        .count(),
                    0,
                    "final sync failure occurs only after exact artifact cleanup"
                );
                match policy {
                    PreparationPolicy::Strict => {
                        assert!(retry.is_err());
                        assert!(!recovery.is_resolved());
                        assert!(
                            witness.upgrade().is_some(),
                            "general recovery remains strict"
                        );
                        recovery.retry().unwrap();
                    }
                    PreparationPolicy::SaveLiveCustody => {
                        retry.unwrap();
                        assert!(
                            recovery.is_resolved(),
                            "removed Save artifacts are not live custody"
                        );
                    }
                }
                assert!(
                    witness.upgrade().is_none(),
                    "actual native owner retires with its authority"
                );
            }
        }
    }

    #[test]
    fn save_unverified_unlinked_observation_retires_before_stale_context_validation() {
        for policy in [
            PreparationPolicy::Strict,
            PreparationPolicy::SaveLiveCustody,
        ] {
            let source = tempfile::tempdir().unwrap();
            let source_root = Root::open(source.path()).unwrap();
            let context = fixture_context(&source_root).unwrap();
            let parent = context.placement().directory().path();
            let owner = Arc::new(tempfile::tempfile().unwrap());
            let witness = Arc::downgrade(&owner);
            let error = prepare_directory_using(
                context.reserve_directory().unwrap(),
                OsStr::new("unverified"),
                || false,
                owner,
                policy,
                |_, actual_parent, relative, mode| {
                    // Obtain Unverified from the REAL producer's shared-parent
                    // path, not a fabricated ownership/publication receipt.
                    set_directory_permissions(actual_parent, 0o1777).unwrap();
                    create_directory_beneath(actual_parent, relative, mode)
                },
                |_| panic!("an unverified producer cannot reach private setup sync"),
            )
            .unwrap_err();
            let RecoveryDirectoryError::Retained { recovery, .. } = error else {
                panic!("unverified observation retains its exact native owners");
            };
            std::fs::remove_dir(parent.join("unverified")).unwrap();
            // This private fixture invalidates placement using an observed Git
            // locator. It does not claim named-source replacement coverage.
            std::fs::write(parent.join(".git"), b"invalidated placement").unwrap();
            assert!(context.placement().validate().is_err());
            {
                let _fault = fixture_fail_next_preparation_cleanup_sync();
                let result = recovery.retry();
                match policy {
                    PreparationPolicy::Strict => {
                        assert!(result.is_err());
                        assert!(!recovery.is_resolved());
                        assert!(witness.upgrade().is_some());
                    }
                    PreparationPolicy::SaveLiveCustody => {
                        result.unwrap();
                        assert!(recovery.is_resolved());
                        assert!(witness.upgrade().is_none());
                    }
                }
            }
            std::fs::remove_file(parent.join(".git")).unwrap();
            if matches!(policy, PreparationPolicy::Strict) {
                recovery.retry().unwrap();
                assert!(witness.upgrade().is_none());
            }
        }
    }

    #[test]
    fn save_only_resolved_staging_bypasses_stale_context_without_adopting_unknown_entries() {
        let source = tempfile::tempdir().unwrap();
        let source_root = Root::open(source.path()).unwrap();
        let context = fixture_context(&source_root).unwrap();
        let parent = context.placement().directory().path();
        let mut error = fixture_directory_preparation_failure(context.placement().directory());
        let mut preparation = error
            .take_recovery()
            .expect("real producer retained staging");
        let artifact = std::fs::read_dir(parent)
            .unwrap()
            .map(|entry| entry.unwrap().path().join("directory"))
            .find(|path| path.is_file())
            .expect("real producer's partial artifact");
        std::fs::remove_file(artifact).unwrap();
        preparation.retry_cleanup().unwrap();
        assert!(preparation.is_resolved());
        std::fs::write(parent.join(".git"), b"invalidated placement").unwrap();
        assert!(
            retry_retained_preparation(PreparationPolicy::Strict, &context, &mut preparation)
                .is_err()
        );
        let _fault = fixture_fail_next_preparation_cleanup_sync();
        retry_retained_preparation(
            PreparationPolicy::SaveLiveCustody,
            &context,
            &mut preparation,
        )
        .unwrap();
        assert!(preparation.is_resolved());
        assert!(
            parent.join(".git").is_file(),
            "stale-context bypass never deletes unknown entries"
        );
    }

    #[test]
    fn one_shot_preparation_fault_uses_only_the_actual_save_factory_sync_path() {
        let source = tempfile::tempdir().unwrap();
        let source_root = Root::open(source.path()).unwrap();
        let context = fixture_context(&source_root).unwrap();
        let _fault = fixture_fail_next_save_preparation_sync();
        // General preparation neither consumes nor sees the Save-only fault.
        let general = prepare_recovery_directory_with_owner(
            context.reserve_directory().unwrap(),
            OsStr::new("general"),
            || false,
            (),
        )
        .unwrap();
        cleanup_directory(general);
        let (first, first_status) = prepare_save_recovery_directory_with_owner(
            context.reserve_directory().unwrap(),
            OsStr::new("first-save"),
            || false,
            (),
        )
        .unwrap();
        assert!(matches!(
            first_status,
            SavePreparationDurability::Unconfirmed(_)
        ));
        cleanup_directory(first);
        let (second, second_status) = prepare_save_recovery_directory_with_owner(
            context.reserve_directory().unwrap(),
            OsStr::new("second-save"),
            || false,
            (),
        )
        .unwrap();
        assert!(matches!(
            second_status,
            SavePreparationDurability::Confirmed
        ));
        cleanup_directory(second);
    }

    #[test]
    fn save_factory_confirms_successful_directory_preparation() {
        let source = tempfile::tempdir().unwrap();
        let source_root = Root::open(source.path()).unwrap();
        let context = fixture_context(&source_root).unwrap();
        let (directory, durability) = prepare_save_recovery_directory_with_owner(
            context.reserve_directory().unwrap(),
            OsStr::new("holding"),
            || false,
            (),
        )
        .unwrap();
        assert!(matches!(durability, SavePreparationDurability::Confirmed));
        cleanup_directory(directory);
    }

    #[test]
    fn save_private_mode_failure_remains_retained_before_directory_confirmation() {
        let source = tempfile::tempdir().unwrap();
        let source_root = Root::open(source.path()).unwrap();
        let context = fixture_context(&source_root).unwrap();
        let leaf = OsStr::new("holding");
        let error = prepare_directory_using(
            context.reserve_directory().unwrap(),
            leaf,
            || false,
            (),
            PreparationPolicy::SaveLiveCustody,
            |namespace, parent, relative, mode| {
                let created = create_directory_in_namespace(namespace, parent, relative, mode)?;
                set_directory_permissions(created.directory(), 0o750)
                    .expect("inject changed mode on the real fresh holding");
                Ok(created)
            },
            |_| panic!("private qualification failure must precede durability confirmation"),
        )
        .unwrap_err();
        let RecoveryDirectoryError::Retained { recovery, .. } = error else {
            panic!("private mode failure must retain original authority");
        };
        assert!(
            recovery.retry().is_err(),
            "newer private mode must not be repaired away"
        );
        let holding = resolve_directory(context.placement().directory(), Path::new(leaf)).unwrap();
        assert_eq!(holding.metadata().unwrap().mode() & 0o7777, 0o750);
        set_directory_permissions(&holding, 0o700).unwrap();
        recovery.retry().unwrap();
    }

    #[test]
    fn save_payload_placement_failure_remains_retained_before_directory_confirmation() {
        let source = tempfile::tempdir().unwrap();
        let source_root = Root::open(source.path()).unwrap();
        let context = fixture_context(&source_root).unwrap();
        let original = context.placement().directory().path().join("holding");
        let moved = source.path().join("moved-holding");
        let error = prepare_directory_using(
            context.reserve_directory().unwrap(),
            OsStr::new("holding"),
            || false,
            (),
            PreparationPolicy::SaveLiveCustody,
            |namespace, parent, relative, mode| {
                let created = create_directory_in_namespace(namespace, parent, relative, mode)?;
                std::fs::rename(created.directory().path(), &moved)
                    .expect("move the real fresh holding into the captured source");
                Ok(created)
            },
            |_| panic!("actual payload placement must qualify before durability confirmation"),
        )
        .unwrap_err();
        let RecoveryDirectoryError::Retained { recovery, .. } = error else {
            panic!("moved payload must retain original authority");
        };
        assert!(recovery.retry().is_err());
        assert!(moved.is_dir());
        std::fs::rename(&moved, &original).unwrap();
        recovery.retry().unwrap();
    }

    #[test]
    fn save_cancellation_after_real_publication_retains_the_original_receipt() {
        let source = tempfile::tempdir().unwrap();
        let source_root = Root::open(source.path()).unwrap();
        let context = fixture_context(&source_root).unwrap();
        let cancelled = AtomicBool::new(false);
        let error = prepare_directory_using(
            context.reserve_directory().unwrap(),
            OsStr::new("holding"),
            || cancelled.load(Ordering::SeqCst),
            (),
            PreparationPolicy::SaveLiveCustody,
            |namespace, parent, relative, mode| {
                let created = create_directory_in_namespace(namespace, parent, relative, mode)?;
                cancelled.store(true, Ordering::SeqCst);
                Ok(created)
            },
            |_| panic!("post-publication cancellation must retain setup before durability"),
        )
        .unwrap_err();
        let RecoveryDirectoryError::Retained { recovery, .. } = error else {
            panic!("post-publication cancellation must retain original receipt");
        };
        recovery.retry().unwrap();
    }
}
