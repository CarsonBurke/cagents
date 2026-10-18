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
    fn finish_setup(&mut self) -> io::Result<()> {
        // No payload exists yet. Resolve producer pending work, then apply
        // witnessed private access before ancestry walking an umask000 inode.
        self.initialize_private_mode()?;
        self.context
            .validate_payload_root(self.publication.directory())?;
        sync_directory(self.publication.directory())?;
        sync_directory(self.context.placement().directory())
    }
    fn retry_failed_preparation(&mut self) -> Result<(), String> {
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
        sync_directory(self.context.placement().directory()).map_err(|e| e.to_string())
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
                context.placement().validate().map_err(|e| e.to_string())?;
                if directory.metadata().map_err(|e| e.to_string())?.nlink() == 0 {
                    return sync_directory(context.placement().directory()).map_err(|e| e.to_string());
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
                    context.placement().validate().map_err(|e| e.to_string())?;
                    preparation.retry_cleanup().map_err(|e| e.to_string())?;
                    sync_directory(context.placement().directory()).map_err(|e| e.to_string())
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
        pending.finish_setup()
    };
    if let Err(cause) = result {
        let recovery = slot.retain(
            "Recovery holding initialization retains its original directory".into(),
            move || pending.retry_failed_preparation(),
        );
        return Err(RecoveryDirectoryError::Retained { cause, recovery });
    }
    Ok(RecoveryDirectory {
        publication: pending.publication,
        owner: pending.owner,
        private_metadata: pending
            .private_metadata
            .expect("successful private setup records actual metadata"),
        context: pending.context,
        slot,
    })
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
