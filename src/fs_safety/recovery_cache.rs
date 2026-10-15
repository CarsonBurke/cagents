//! Worker-only retained-parent discovery and per-operation recovery holdings.
//! Candidate discovery reads retained ancestors only. A candidate is never a
//! placement or write grant: the domain must obtain real issuer/cleanup proof
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

/// At most eight original held candidates and a bounded256-edge walk. Retain
/// and charge yielded handles in the actual domain intake; prefer trying one
/// at a time. No pathname is reopened and no directory is created here.
pub struct RecoveryParentCandidates {
    cursor: Arc<Root>,
    source: RecoveryDirectoryStamp,
    walked: usize,
    yielded: usize,
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
            let step = (|| -> io::Result<Option<Arc<Root>>> {
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
                        Ok(Some(parent))
                    }
                    Err(error)
                        if matches!(
                            error.kind(),
                            io::ErrorKind::PermissionDenied | io::ErrorKind::Unsupported
                        ) =>
                    {
                        Ok(None)
                    }
                    Err(error) => Err(error),
                }
            })();
            match step {
                Ok(Some(parent)) => {
                    self.yielded += 1;
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
/// Its original publication, issued parent context and SAME pre-effect slot
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
    enrollment_attempted: bool,
    enrollment_confirmed: bool,
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
        self.enrollment_attempted = true;
        self.context
            .register_payload_root(self.publication.directory())?;
        self.enrollment_confirmed = true;
        self.context
            .validate_payload_root(self.publication.directory())?;
        sync_directory(self.publication.directory())?;
        sync_directory(self.context.placement().directory())
    }
    fn retry_failed_preparation(&mut self) -> Result<(), String> {
        if !self.publication.is_removed() {
            self.initialize_private_mode().map_err(|e| e.to_string())?;
            if self.enrollment_attempted && !self.enrollment_confirmed {
                // Unknown handoff is not canceled by removing H. Complete the
                // SAME original-root idempotent enrollment before settlement.
                self.context
                    .register_payload_root(self.publication.directory())
                    .map_err(|e| e.to_string())?;
                self.enrollment_confirmed = true;
            }
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

/// Create and register ACTUAL per-operation H inside its ALREADY issued outside
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
    if let Err(cause) = reserved.context.placement().require_payload_registration() {
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
    // registry handles stay under the measured128-FD admission, not this bound.
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
                // Preserve the observed Root and the original domain obligation.
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
        enrollment_attempted: false,
        enrollment_confirmed: false,
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

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::{cell::Cell, fs, os::unix::fs::PermissionsExt};

    #[test]
    fn private_verification_preserves_original_identity_across_name_replacement() {
        let fixture = tempfile::tempdir().unwrap();
        let path = fixture.path().join("holding");
        fs::create_dir(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        let held = Root::open(&path).unwrap();
        let expected = held.metadata().unwrap();
        fs::rename(&path, fixture.path().join("original")).unwrap();
        fs::create_dir(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        let replacement = Root::open(&path).unwrap();
        verify_recovery_directory_private(&held, &expected).unwrap();
        assert!(verify_recovery_directory_private(&replacement, &expected).is_err());
        fs::write(fixture.path().join("original/payload"), b"owned").unwrap();
        verify_recovery_directory_private(&held, &expected).unwrap();
        assert_eq!(fs::read_dir(&path).unwrap().count(), 0);
    }

    #[test]
    fn private_verification_refuses_mode_drift_without_repair_or_new_baseline() {
        let fixture = tempfile::tempdir().unwrap();
        fs::set_permissions(fixture.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let held = Root::open(fixture.path()).unwrap();
        let expected = held.metadata().unwrap();
        for mode in [0o711, 0o1700, 0o2700] {
            fs::set_permissions(fixture.path(), fs::Permissions::from_mode(mode)).unwrap();
            let observed = held.metadata().unwrap();
            assert_ne!(observed.mode() & 0o7777, 0o700);
            assert!(verify_recovery_directory_private(&held, &expected).is_err());
            assert!(verify_recovery_directory_private(&held, &observed).is_err());
            assert_eq!(held.metadata().unwrap().mode(), observed.mode());
        }
        fs::set_permissions(fixture.path(), fs::Permissions::from_mode(0o700)).unwrap();
        verify_recovery_directory_private(&held, &expected).unwrap();
    }

    fn context() -> (tempfile::TempDir, FilesystemRecoveryContext) {
        let source = tempfile::tempdir().unwrap();
        let held = Root::open(source.path()).unwrap();
        let context = crate::filesystem_recovery_context::fixtures::fixture_context(&held).unwrap();
        (source, context)
    }
    fn names(root: &Root) -> Vec<OsString> {
        let mut names: Vec<_> = root
            .read_dir()
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn candidate_discovery_retains_original_ancestor_after_public_replacement() {
        let fixture = tempfile::tempdir().unwrap();
        let parent = fixture.path().join("parent");
        fs::create_dir(&parent).unwrap();
        fs::create_dir(parent.join("source")).unwrap();
        let expected = fs::metadata(&parent).unwrap();
        let source = Arc::new(Root::open(&parent.join("source")).unwrap());
        fs::rename(&parent, fixture.path().join("original-parent")).unwrap();
        fs::create_dir(&parent).unwrap();
        fs::create_dir(parent.join("source")).unwrap();
        fs::write(parent.join("foreign"), b"untouched").unwrap();
        let first = RecoveryParentCandidates::new(source)
            .unwrap()
            .next()
            .unwrap()
            .unwrap();
        let actual = first.metadata().unwrap();
        assert_eq!(
            (actual.dev(), actual.ino()),
            (expected.dev(), expected.ino())
        );
        assert_eq!(fs::read(parent.join("foreign")).unwrap(), b"untouched");
        assert!(
            fs::read_dir(fixture.path().join("original-parent"))
                .unwrap()
                .all(|entry| entry.unwrap().file_name() == "source")
        );
    }

    #[test]
    fn candidate_discovery_skips_shared_parent_and_bounds_retained_candidates() {
        let fixture = tempfile::tempdir().unwrap();
        let mut path = fixture.path().to_path_buf();
        for index in 0..12 {
            path.push(format!("level-{index}"));
            fs::create_dir(&path).unwrap();
        }
        let source_path = path.join("source");
        fs::create_dir(&source_path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o777)).unwrap();
        let excluded = recovery_directory_stamp(&Root::open(&path).unwrap()).unwrap();
        let candidates: Vec<_> =
            RecoveryParentCandidates::new(Arc::new(Root::open(&source_path).unwrap()))
                .unwrap()
                .collect::<io::Result<_>>()
                .unwrap();
        assert_eq!(candidates.len(), MAX_CANDIDATES);
        assert!(
            candidates
                .iter()
                .all(|root| recovery_directory_stamp(root).unwrap() != excluded)
        );
        assert!(fs::read_dir(source_path).unwrap().next().is_none());
    }

    #[test]
    fn factory_returns_actual_published_holding_and_same_context_reservation() {
        let (_source, context) = context();
        let holding = prepare_recovery_directory(
            context.reserve_directory().unwrap(),
            OsStr::new("holding"),
            || false,
        )
        .unwrap();
        assert_eq!(
            holding.directory().metadata().unwrap().mode() & 0o7777,
            0o700
        );
        assert!(Arc::ptr_eq(
            holding.context().placement(),
            context.placement()
        ));
        holding
            .context()
            .validate_payload_root(holding.directory())
            .unwrap();
        assert_eq!(
            names(context.placement().directory()),
            [OsString::from("holding")]
        );
        let (mut publication, (), private, retained, slot) = holding.into_parts();
        assert_eq!(private.mode() & 0o7777, 0o700);
        assert_eq!(publication.identity(), {
            let held = publication.directory().metadata().unwrap();
            (held.dev(), held.ino())
        });
        publication.cleanup_empty().unwrap();
        sync_directory(retained.placement().directory()).unwrap();
        assert!(publication.is_removed());
        drop((publication, retained, slot));
    }

    #[test]
    fn existing_holding_and_pre_effect_cancellation_leave_names_untouched() {
        let (_source, context) = context();
        let parent = context.placement().directory();
        fs::create_dir(parent.path().join("existing")).unwrap();
        fs::write(parent.path().join("existing/foreign"), b"preserved").unwrap();
        let before = names(parent);
        let error = prepare_recovery_directory(
            context.reserve_directory().unwrap(),
            OsStr::new("existing"),
            || false,
        )
        .unwrap_err();
        assert!(
            matches!(error, RecoveryDirectoryError::Rejected(ref e) if e.kind() == io::ErrorKind::AlreadyExists)
        );
        let error = prepare_recovery_directory(
            context.reserve_directory().unwrap(),
            OsStr::new("cancelled"),
            || true,
        )
        .unwrap_err();
        assert!(
            matches!(error, RecoveryDirectoryError::Rejected(ref e) if e.kind() == io::ErrorKind::Interrupted)
        );
        assert_eq!(names(parent), before);
        assert_eq!(
            fs::read(parent.path().join("existing/foreign")).unwrap(),
            b"preserved"
        );
    }

    #[test]
    fn cancellation_after_publication_retains_exact_replacement_safe_cleanup() {
        let (_source, context) = context();
        let parent = context.placement().directory();
        let calls = Cell::new(0);
        let error = prepare_recovery_directory(
            context.reserve_directory().unwrap(),
            OsStr::new("holding"),
            || {
                calls.set(calls.get() + 1);
                if calls.get() == 2 {
                    fs::rename(
                        parent.path().join("holding"),
                        parent.path().join("original"),
                    )
                    .unwrap();
                    fs::create_dir(parent.path().join("holding")).unwrap();
                    fs::write(parent.path().join("holding/foreign"), b"preserved").unwrap();
                    true
                } else {
                    false
                }
            },
        )
        .unwrap_err();
        let recovery = error.recovery().unwrap();
        assert!(!recovery.is_resolved());
        assert!(recovery.retry().is_err());
        assert_eq!(
            fs::read(parent.path().join("holding/foreign")).unwrap(),
            b"preserved"
        );
        fs::remove_file(parent.path().join("holding/foreign")).unwrap();
        fs::remove_dir(parent.path().join("holding")).unwrap();
        fs::rename(
            parent.path().join("original"),
            parent.path().join("holding"),
        )
        .unwrap();
        recovery.retry().unwrap();
        assert!(recovery.is_resolved());
        assert!(names(parent).is_empty());
    }

    #[test]
    fn moved_actual_holding_refuses_retry_inside_checkout_then_resolves_after_repair() {
        let (source, context) = context();
        let parent = context.placement().directory();
        let calls = Cell::new(0);
        let error = prepare_recovery_directory(
            context.reserve_directory().unwrap(),
            OsStr::new("holding"),
            || {
                calls.set(calls.get() + 1);
                if calls.get() == 2 {
                    fs::rename(
                        parent.path().join("holding"),
                        source.path().join("moved-holding"),
                    )
                    .unwrap();
                    true
                } else {
                    false
                }
            },
        )
        .unwrap_err();
        let recovery = error.recovery().unwrap();
        assert!(recovery.retry().is_err());
        assert!(source.path().join("moved-holding").is_dir());
        fs::rename(
            source.path().join("moved-holding"),
            parent.path().join("holding"),
        )
        .unwrap();
        recovery.retry().unwrap();
        assert!(recovery.is_resolved());
        assert!(names(parent).is_empty());
    }
    #[derive(Default)]
    struct UnknownCreation {
        sealed: bool,
        in_flight: usize,
        limit: Option<usize>,
    }
    struct UnknownCreationGuard(Arc<std::sync::Mutex<UnknownCreation>>);
    impl Drop for UnknownCreationGuard {
        fn drop(&mut self) {
            let mut state = self.0.lock().unwrap();
            state.in_flight = state.in_flight.checked_sub(1).expect("one guard admission");
        }
    }
    struct UnknownEnrollment {
        creation: Arc<std::sync::Mutex<UnknownCreation>>,
        attempts: std::sync::atomic::AtomicUsize,
        payload: std::sync::Mutex<Option<Root>>,
        _outside: tempfile::TempDir,
        _runtime: tempfile::TempDir,
    }
    impl RecoveryRegistrationLease for UnknownEnrollment {
        fn validate(&self) -> io::Result<()> {
            Ok(())
        }
        fn begin_payload_creation(&self) -> io::Result<Arc<dyn Send + Sync>> {
            // This complete isolated test topology grants only counted owned
            // creation admission. Sealing blocks new work, never old cleanup.
            let mut state = self.creation.lock().unwrap();
            if state.sealed || state.limit.is_some_and(|limit| state.in_flight >= limit) {
                return Err(io::Error::other("Fixture creation admission unavailable"));
            }
            state.in_flight = state
                .in_flight
                .checked_add(1)
                .expect("bounded fixture admission");
            Ok(Arc::new(UnknownCreationGuard(self.creation.clone())))
        }
        fn descriptor_budget(&self) -> usize {
            1
        }
        fn register_payload(&self, root: &Root) -> io::Result<()> {
            let mut held = self.payload.lock().unwrap();
            if let Some(original) = held.as_ref() {
                if recovery_directory_stamp(original)? != recovery_directory_stamp(root)? {
                    return Err(io::Error::other("Fixture refused a successor payload"));
                }
            } else {
                *held = Some(root.try_clone()?);
            }
            let attempt = self
                .attempts
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if attempt < 2 {
                Err(io::Error::other(
                    "Injected unknown enrollment after retained handoff",
                ))
            } else {
                Ok(())
            }
        }
    }

    #[test]
    fn unknown_enrollment_retries_same_holding_before_cleanup_or_resolution() {
        let source_fixture = tempfile::tempdir().unwrap();
        let source = Arc::new(Root::open(source_fixture.path()).unwrap());
        let outside = tempfile::tempdir_in(source.path().parent().unwrap()).unwrap();
        let runtime = tempfile::tempdir().unwrap();
        let protection = Arc::new(Some(
            crate::worktree_cleanup::fixture_protection(runtime.path(), source.path(), false)
                .unwrap(),
        ));
        let parent = Arc::new(Root::open(outside.path()).unwrap());
        // Complete isolated topology; the owned lease deliberately records the
        // real original Root BEFORE returning unknown, as a daemon handoff can.
        let registry = Arc::new(UnknownEnrollment {
            creation: Arc::default(),
            attempts: std::sync::atomic::AtomicUsize::new(0),
            payload: std::sync::Mutex::new(None),
            _outside: outside,
            _runtime: runtime,
        });
        let placement = RecoveryPlacement::acquire(
            parent.clone(),
            source.clone(),
            Arc::from([source.clone()]),
            RecoveryRegistryCheck::from_lease(registry.clone()),
        )
        .unwrap();
        let context = FilesystemRecoveryContext::new(&source, protection, placement).unwrap();
        let error = prepare_recovery_directory(
            context.reserve_directory().unwrap(),
            OsStr::new("holding"),
            || false,
        )
        .unwrap_err();
        let recovery = error.recovery().unwrap();
        assert_eq!(
            registry.attempts.load(std::sync::atomic::Ordering::SeqCst),
            1
        );
        assert_eq!(registry.creation.lock().unwrap().in_flight, 1);
        registry.creation.lock().unwrap().sealed = true;
        assert!(
            context.reserve_directory().is_err(),
            "retirement refuses new mkdir admission"
        );
        assert!(recovery.retry().is_err());
        assert!(!recovery.is_resolved());
        assert_eq!(
            registry.attempts.load(std::sync::atomic::Ordering::SeqCst),
            2
        );
        assert!(
            parent.path().join("holding").is_dir(),
            "unknown enrollment must not remove H"
        );
        assert_eq!(
            registry.creation.lock().unwrap().in_flight,
            1,
            "same-slot uncertain recovery retains creation admission"
        );
        recovery.retry().unwrap();
        assert!(recovery.is_resolved());
        assert_eq!(
            registry.attempts.load(std::sync::atomic::Ordering::SeqCst),
            3
        );
        assert_eq!(
            registry.creation.lock().unwrap().in_flight,
            0,
            "positive retry cleanup releases the final original slot guard"
        );
        assert!(names(&parent).is_empty());
    }

    #[test]
    fn restrictive_umask_holding_is_private_and_enrolled_before_payload_admission() {
        const CHILD: &str = "CAGENTS_FACTORY_RESTRICTIVE_UMASK_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "fs_safety::recovery_cache::tests::restrictive_umask_holding_is_private_and_enrolled_before_payload_admission", "--test-threads=1", "--nocapture"])
                .env(CHILD, "1").output().unwrap();
            let stdout = String::from_utf8_lossy(&output.stdout);
            assert!(
                output.status.success(),
                "{stdout}\n{}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(stdout.contains("CAGENTS_FACTORY_UMASK_BODY_COMPLETED"));
            assert!(stdout.contains("1 passed; 0 failed; 0 ignored"));
            return;
        }
        let (_source, context) = context();
        let admission = context.reserve_directory().unwrap();
        // Only this exact isolated child changes its process-global umask.
        let previous = unsafe { libc::umask(0o777) };
        let result = prepare_recovery_directory(admission, OsStr::new("holding"), || false);
        unsafe {
            libc::umask(previous);
        }
        let holding = result.unwrap();
        assert_eq!(
            holding.directory().metadata().unwrap().mode() & 0o7777,
            0o700
        );
        holding
            .context()
            .validate_payload_root(holding.directory())
            .unwrap();
        let (mut publication, (), private, retained, _slot) = holding.into_parts();
        assert_eq!(private.mode() & 0o7777, 0o700);
        assert_eq!(publication.initial_metadata().mode() & 0o7777, 0);
        publication.cleanup_empty().unwrap();
        sync_directory(retained.placement().directory()).unwrap();
        assert!(publication.is_removed());
        println!("CAGENTS_FACTORY_UMASK_BODY_COMPLETED");
    }

    #[test]
    fn prechecked_original_file_owner_stays_in_same_slot_through_failed_factory_retry() {
        let (source, context) = context();
        fs::write(source.path().join("original"), b"original bytes").unwrap();
        let owner = Arc::new(fs::File::open(source.path().join("original")).unwrap());
        let weak = Arc::downgrade(&owner);
        let calls = Cell::new(0);
        let error = prepare_recovery_directory_with_owner(
            context.reserve_directory().unwrap(),
            OsStr::new("holding"),
            || {
                calls.set(calls.get() + 1);
                calls.get() == 2
            },
            owner,
        )
        .unwrap_err();
        assert!(
            weak.upgrade().is_some(),
            "post-effect error must retain original file descriptor"
        );
        let recovery = error.recovery().unwrap();
        fs::write(
            context
                .placement()
                .directory()
                .path()
                .join("holding/partial"),
            b"retained",
        )
        .unwrap();
        assert!(recovery.retry().is_err());
        assert!(
            weak.upgrade().is_some(),
            "failed cleanup must keep same original owner"
        );
        fs::remove_file(
            context
                .placement()
                .directory()
                .path()
                .join("holding/partial"),
        )
        .unwrap();
        recovery.retry().unwrap();
        assert!(recovery.is_resolved());
        assert!(
            weak.upgrade().is_none(),
            "positive cleanup and sync release the retained original owner"
        );
    }

    #[test]
    fn successful_factory_transfers_original_owner_with_publication_context_and_slot() {
        let (source, context) = context();
        fs::write(source.path().join("original"), b"original bytes").unwrap();
        let owner = Arc::new(fs::File::open(source.path().join("original")).unwrap());
        let weak = Arc::downgrade(&owner);
        let holding = prepare_recovery_directory_with_owner(
            context.reserve_directory().unwrap(),
            OsStr::new("holding"),
            || false,
            owner,
        )
        .unwrap();
        let (mut publication, owner, private, retained, slot) = holding.into_parts();
        assert!(weak.upgrade().is_some());
        assert_eq!(owner.metadata().unwrap().len(), 14);
        assert_eq!(private.mode() & 0o7777, 0o700);
        publication.cleanup_empty().unwrap();
        sync_directory(retained.placement().directory()).unwrap();
        drop((publication, owner, retained, slot));
        assert!(weak.upgrade().is_none());
    }

    #[test]
    fn native_owner_drop_stays_charged_on_transfer_rejection_and_callback_unwind() {
        struct NativeOwner {
            _file: Arc<fs::File>,
            entered: std::sync::mpsc::SyncSender<()>,
            release: std::sync::mpsc::Receiver<()>,
        }
        impl Drop for NativeOwner {
            fn drop(&mut self) {
                let _ = self.entered.send(());
                let _ = self.release.recv();
            }
        }
        #[derive(Clone, Copy)]
        enum Case {
            Transfer,
            Existing,
            PreEffectPanic,
        }
        for case in [Case::Transfer, Case::Existing, Case::PreEffectPanic] {
            let existing = matches!(case, Case::Existing);
            let panicking = matches!(case, Case::PreEffectPanic);
            let source = tempfile::tempdir().unwrap();
            let outside = tempfile::tempdir().unwrap();
            let runtime = tempfile::tempdir().unwrap();
            let source_root = Arc::new(Root::open(source.path()).unwrap());
            let parent = Arc::new(Root::open(outside.path()).unwrap());
            let registry = Arc::new(UnknownEnrollment {
                creation: Arc::new(std::sync::Mutex::new(UnknownCreation {
                    limit: Some(1),
                    ..UnknownCreation::default()
                })),
                // This case needs immediate successful enrollment, not the other
                // fixture's first two deliberately unknown replies.
                attempts: std::sync::atomic::AtomicUsize::new(2),
                payload: std::sync::Mutex::new(None),
                _outside: outside,
                _runtime: runtime,
            });
            let placement = RecoveryPlacement::acquire(
                parent,
                source_root.clone(),
                Arc::from([source_root.clone()]),
                RecoveryRegistryCheck::from_lease(registry.clone()),
            )
            .unwrap();
            let protection = Arc::new(Some(
                crate::worktree_cleanup::fixture_protection(
                    registry._runtime.path(),
                    source.path(),
                    false,
                )
                .unwrap(),
            ));
            let context =
                FilesystemRecoveryContext::new(&source_root, protection, placement).unwrap();
            fs::write(source.path().join("original"), b"retained original").unwrap();
            let file = Arc::new(fs::File::open(source.path().join("original")).unwrap());
            let weak = Arc::downgrade(&file);
            let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(1);
            let (release_tx, release_rx) = std::sync::mpsc::sync_channel(1);
            if existing {
                fs::create_dir(registry._outside.path().join("holding")).unwrap();
            }
            let admission = context.reserve_directory().unwrap();
            let owner = NativeOwner {
                _file: file,
                entered: entered_tx,
                release: release_rx,
            };
            let worker = std::thread::spawn(move || {
                let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let result = if panicking {
                        prepare_recovery_directory_using(
                            admission,
                            OsStr::new("holding"),
                            || false,
                            owner,
                            |_, _, _, _| panic!("fixture pre-effect creation callback panic"),
                        )
                    } else {
                        prepare_recovery_directory_with_owner(
                            admission,
                            OsStr::new("holding"),
                            || false,
                            owner,
                        )
                    };
                    if existing {
                        assert!(matches!(result, Err(RecoveryDirectoryError::Rejected(_))));
                    } else {
                        drop(result.unwrap().into_parts());
                    }
                }));
                assert_eq!(caught.is_err(), panicking);
            });
            let entered = entered_rx
                .recv_timeout(std::time::Duration::from_secs(3))
                .is_ok();
            let original_is_live = weak.upgrade().is_some();
            let charged = registry.creation.lock().unwrap().in_flight;
            let refused = context.reserve_directory().is_err();
            // Always release/join before assertions, including the old bad order.
            let _ = release_tx.send(());
            let joined = worker.join().is_ok();
            assert!(entered && joined && original_is_live);
            assert_eq!(
                charged, 1,
                "T's original native owner must stay charged through Drop"
            );
            assert!(
                refused,
                "the isolated one-owner admission must remain exhausted"
            );
            assert!(weak.upgrade().is_none());
            assert_eq!(registry.creation.lock().unwrap().in_flight, 0);
            drop(context.reserve_directory().unwrap());
            if panicking {
                assert!(names(context.placement().directory()).is_empty());
            }
        }
    }

    #[test]
    fn real_preparation_failure_keeps_original_owner_until_actual_private_repair() {
        let (source, context) = context();
        fs::write(source.path().join("original"), b"original bytes").unwrap();
        let file = Arc::new(fs::File::open(source.path().join("original")).unwrap());
        let weak = Arc::downgrade(&file);
        let parent = context.placement().directory().try_clone().unwrap();
        let error = fixture_recovery_directory_preparation_failure(
            context.reserve_directory().unwrap(),
            file,
        );
        let recovery = error
            .recovery()
            .expect("real producer failure must retain Preparation");
        drop(context);
        assert!(weak.upgrade().is_some());
        assert!(recovery.retry().is_err());
        assert!(
            weak.upgrade().is_some(),
            "failed prepublication retry retains exact original owner"
        );
        let entries = names(&parent);
        assert_eq!(entries.len(), 1);
        let outer = parent.path().join(&entries[0]);
        assert_eq!(
            fs::read(outer.join("directory")).unwrap(),
            b"retained fixture artifact"
        );
        // Explicit repair of this isolated test's real interference. Retry
        // still uses the retained Preparation, never this diagnostic spelling.
        fs::remove_file(outer.join("directory")).unwrap();
        recovery.retry().unwrap();
        assert!(recovery.is_resolved());
        assert!(weak.upgrade().is_none());
        assert!(names(&parent).is_empty());
    }
}
