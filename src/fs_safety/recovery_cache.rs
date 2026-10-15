//! Worker-only retained-parent discovery and per-operation recovery holdings.
//! Candidate discovery reads retained ancestors only. A candidate is never a
//! placement or write grant: the domain must obtain real issuer/cleanup proof
//! before constructing the FilesystemRecoveryContext passed to the factory.
use super::*;
use crate::{filesystem_recovery_context::{FilesystemRecoveryContext, FilesystemRecoveryReservation},
    recovery_authority::{RecoveryAuthority, RecoverySlot, MAX_RECOVERY_DESCRIPTORS}};
use std::{os::unix::fs::MetadataExt, sync::Arc};

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
        Ok(Self { cursor: source, source: stamp, walked: 0, yielded: 0, finished: false })
    }
}
#[cfg(target_os = "linux")]
fn retained_parent(root: &Root) -> io::Result<Root> {
    let directory = open_at(root.descriptor(), OsStr::new(".."),
        libc::O_PATH | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)?;
    // Diagnostic spelling only. All candidate proofs use this actual FD.
    Ok(Root { directory, path: root.path().join("..") })
}
#[cfg(not(target_os = "linux"))]
fn retained_parent(_root: &Root) -> io::Result<Root> {
    Err(io::Error::new(io::ErrorKind::Unsupported,
        "Recovery ancestor candidates require qualified native mount-identity inspection"))
}
impl Iterator for RecoveryParentCandidates {
    type Item = io::Result<Arc<Root>>;
    fn next(&mut self) -> Option<Self::Item> {
        if self.finished || self.yielded == MAX_CANDIDATES { return None; }
        while self.walked < MAX_PARENT_WALK {
            self.walked += 1;
            let step = (|| -> io::Result<Option<Arc<Root>>> {
                let previous = recovery_directory_stamp(&self.cursor)?;
                let parent = Arc::new(retained_parent(&self.cursor)?);
                let current = recovery_directory_stamp(&parent)?;
                if current == previous || current.device != self.source.device
                    || current.mount_id != self.source.mount_id {
                    self.finished = true; return Ok(None);
                }
                self.cursor = parent.clone();
                match DirectoryNamespace::acquire(&parent) {
                    Ok(namespace) => { drop(namespace); Ok(Some(parent)) }
                    Err(error) if matches!(error.kind(), io::ErrorKind::PermissionDenied | io::ErrorKind::Unsupported) => Ok(None),
                    Err(error) => Err(error),
                }
            })();
            match step {
                Ok(Some(parent)) => { self.yielded += 1; return Some(Ok(parent)); }
                Ok(None) if !self.finished => continue,
                Ok(None) => return None,
                Err(error) => { self.finished = true; return Some(Err(error)); }
            }
        }
        self.finished = true;
        Some(Err(io::Error::new(io::ErrorKind::Unsupported,
            "Recovery ancestor discovery exceeded its bounded parent inspection")))
    }
}
impl std::iter::FusedIterator for RecoveryParentCandidates {}

/// The ACTUAL per-operation holding H, not an intermediate cache parent.
/// Its original publication, issued parent context and SAME pre-effect slot
/// travel together until the consumer transfers all three into its live owner.
/// Dropping this value never removes names or claims successful cleanup.
#[must_use = "retain the publication and its existing reservation through actual resolution"]
pub(crate) struct RecoveryDirectory {
    publication: DirectoryPublication,
    context: FilesystemRecoveryContext,
    slot: RecoverySlot,
}
impl std::fmt::Debug for RecoveryDirectory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RecoveryDirectory").field("publication", &self.publication)
            .field("context", &self.context).finish_non_exhaustive()
    }
}
impl RecoveryDirectory {
    pub(crate) fn directory(&self) -> &Root { self.publication.directory() }
    pub(crate) fn context(&self) -> &FilesystemRecoveryContext { &self.context }
    /// Transfer ALL authority into the lower Holding/Undo owner. No uncharged
    /// bare-Root conversion or second reservation is provided.
    pub(crate) fn into_parts(self) -> (DirectoryPublication, FilesystemRecoveryContext, RecoverySlot) {
        (self.publication, self.context, self.slot)
    }
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum RecoveryDirectoryError {
    #[error("Recovery holding preparation was refused: {0}")]
    Rejected(#[source] io::Error),
    #[error("Recovery holding preparation failed: {cause}; retained recovery: {recovery:?}")]
    Retained { #[source] cause: io::Error, recovery: RecoveryAuthority },
}
impl RecoveryDirectoryError {
    pub(crate) fn recovery(&self) -> Option<&RecoveryAuthority> {
        match self { Self::Retained { recovery, .. } => Some(recovery), Self::Rejected(_) => None }
    }
}

struct PendingDirectory {
    publication: DirectoryPublication,
    context: FilesystemRecoveryContext,
    setup_attempted: bool,
    setup_complete: bool,
}
impl PendingDirectory {
    fn initialize_private_mode(&mut self) -> io::Result<()> {
        self.publication.retry_publication_steps().map_err(io::Error::other)?;
        let initial = self.publication.initial_metadata();
        let current = self.publication.directory().metadata()?;
        let mode = current.mode() & 0o7777;
        if current.uid() != initial.uid() || current.gid() != initial.gid()
            || (self.setup_complete && mode != 0o700)
            || (!self.setup_complete && mode != initial.mode() & 0o7777
                && !(self.setup_attempted && mode == 0o700)) {
            return Err(io::Error::other("Published holding permissions changed; newer observed state was preserved"));
        }
        if mode != 0o700 {
            // This is an explicit PRIVATE holding policy, applied only to the
            // original retained Published inode after producer pending work.
            self.setup_attempted = true;
            set_directory_permissions(self.publication.directory(), 0o700)?;
        }
        let checked = self.publication.directory().metadata()?;
        if checked.uid() != initial.uid() || checked.gid() != initial.gid()
            || checked.mode() & 0o7777 != 0o700 {
            return Err(io::Error::other("Kernel did not preserve exact private holding ownership/mode"));
        }
        self.setup_complete = true;
        // Includes native ACL absence where that adapter is supported. This
        // wrapper does not claim Darwin placement or creation qualification.
        drop(DirectoryNamespace::acquire(self.publication.directory())?);
        Ok(())
    }
    fn finish_setup(&mut self) -> io::Result<()> {
        self.context.validate_payload_root(self.publication.directory())?;
        self.initialize_private_mode()?;
        // A failed/unknown handoff retains this SAME lease and exact H. The
        // issuer already reserved its hard payload capacity before effects.
        self.context.register_payload_root(self.publication.directory())?;
        sync_directory(self.publication.directory())?;
        sync_directory(self.context.placement().directory())
    }
    fn retry_failed_preparation(&mut self) -> Result<(), String> {
        if !self.publication.is_removed() {
            self.context.validate_payload_root(self.publication.directory()).map_err(|e| e.to_string())?;
            self.initialize_private_mode().map_err(|e| e.to_string())?;
            self.publication.resolve_recovery().map_err(|e| e.to_string())?;
            self.publication.cleanup_empty().map_err(|e| e.to_string())?;
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
    admission: FilesystemRecoveryReservation, leaf: &OsStr, cancelled: impl Fn() -> bool,
) -> Result<RecoveryDirectory, RecoveryDirectoryError> {
    if components(Path::new(leaf)).is_none_or(|parts| parts.len() != 1) {
        return Err(RecoveryDirectoryError::Rejected(io::Error::new(io::ErrorKind::InvalidInput,
            "Recovery holding requires one ordinary leaf name")));
    }
    admission.context().placement().require_payload_registration().map_err(RecoveryDirectoryError::Rejected)?;
    if cancelled() {
        return Err(RecoveryDirectoryError::Rejected(io::Error::new(io::ErrorKind::Interrupted,
            "Recovery holding creation cancelled before any effect")));
    }
    let (context, slot) = admission.into_parts();
    // Qualified producer retains <=4 private handles plus bounded transient
    // work, inside this existing16-private-FD reservation. Shared context and
    // registry handles stay under the measured128-FD admission, not this bound.
    const { assert!(MAX_RECOVERY_DESCRIPTORS >= 16); }
    let publication = match create_directory_in_namespace(context.placement().namespace(),
        context.placement().directory(), Path::new(leaf), 0o700) {
        Ok(DirectoryCreation::Published(publication)) => publication,
        Ok(DirectoryCreation::Existing(_)) => return Err(RecoveryDirectoryError::Rejected(
            io::Error::new(io::ErrorKind::AlreadyExists, "Recovery holding already exists; it was left untouched"))),
        Ok(DirectoryCreation::Unverified { directory, reason }) => {
            let cause = io::Error::other(format!("Recovery holding has no creation authority: {reason}"));
            let recovery = slot.retain("Unverified recovery holding was left untouched".into(), move || {
                // Preserve the observed Root and the original domain obligation.
                // No automatic deletion/chmod or later ownership conversion.
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
            let Some(mut preparation) = error.take_recovery() else { return Err(RecoveryDirectoryError::Rejected(cause)); };
            let recovery = slot.retain("Private holding staging requires retained cleanup".into(), move || {
                context.placement().validate().map_err(|e| e.to_string())?;
                preparation.retry_cleanup().map_err(|e| e.to_string())?;
                sync_directory(context.placement().directory()).map_err(|e| e.to_string())
            });
            return Err(RecoveryDirectoryError::Retained { cause, recovery });
        }
    };
    let mut pending = PendingDirectory { publication, context, setup_attempted: false, setup_complete: false };
    let result = if cancelled() { Err(io::Error::new(io::ErrorKind::Interrupted,
        "Recovery holding creation cancelled after publication; original authority retained")) }
        else { pending.finish_setup() };
    if let Err(cause) = result {
        let recovery = slot.retain("Recovery holding initialization retains its original directory".into(),
            move || pending.retry_failed_preparation());
        return Err(RecoveryDirectoryError::Retained { cause, recovery });
    }
    Ok(RecoveryDirectory { publication: pending.publication, context: pending.context, slot })
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::{cell::Cell, fs, os::unix::fs::PermissionsExt};

    fn context() -> (tempfile::TempDir, FilesystemRecoveryContext) {
        let source = tempfile::tempdir().unwrap();
        let held = Root::open(source.path()).unwrap();
        let context = crate::filesystem_recovery_context::fixtures::fixture_context(&held).unwrap();
        (source, context)
    }
    fn names(root: &Root) -> Vec<OsString> {
        let mut names: Vec<_> = root.read_dir().unwrap().map(|entry| entry.unwrap().file_name()).collect();
        names.sort(); names
    }

    #[test]
    fn candidate_discovery_retains_original_ancestor_after_public_replacement() {
        let fixture = tempfile::tempdir().unwrap();
        let parent = fixture.path().join("parent");
        fs::create_dir(&parent).unwrap(); fs::create_dir(parent.join("source")).unwrap();
        let expected = fs::metadata(&parent).unwrap();
        let source = Arc::new(Root::open(&parent.join("source")).unwrap());
        fs::rename(&parent, fixture.path().join("original-parent")).unwrap();
        fs::create_dir(&parent).unwrap(); fs::create_dir(parent.join("source")).unwrap();
        fs::write(parent.join("foreign"), b"untouched").unwrap();
        let first = RecoveryParentCandidates::new(source).unwrap().next().unwrap().unwrap();
        let actual = first.metadata().unwrap();
        assert_eq!((actual.dev(), actual.ino()), (expected.dev(), expected.ino()));
        assert_eq!(fs::read(parent.join("foreign")).unwrap(), b"untouched");
        assert!(fs::read_dir(fixture.path().join("original-parent")).unwrap().all(|entry| entry.unwrap().file_name() == "source"));
    }

    #[test]
    fn candidate_discovery_skips_shared_parent_and_bounds_retained_candidates() {
        let fixture = tempfile::tempdir().unwrap(); let mut path = fixture.path().to_path_buf();
        for index in 0..12 { path.push(format!("level-{index}")); fs::create_dir(&path).unwrap(); }
        let source_path = path.join("source"); fs::create_dir(&source_path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o777)).unwrap();
        let excluded = recovery_directory_stamp(&Root::open(&path).unwrap()).unwrap();
        let candidates: Vec<_> = RecoveryParentCandidates::new(Arc::new(Root::open(&source_path).unwrap())).unwrap()
            .collect::<io::Result<_>>().unwrap();
        assert_eq!(candidates.len(), MAX_CANDIDATES);
        assert!(candidates.iter().all(|root| recovery_directory_stamp(root).unwrap() != excluded));
        assert!(fs::read_dir(source_path).unwrap().next().is_none());
    }

    #[test]
    fn factory_returns_actual_published_holding_and_same_context_reservation() {
        let (_source, context) = context();
        let holding = prepare_recovery_directory(context.reserve_directory().unwrap(), OsStr::new("holding"), || false).unwrap();
        assert_eq!(holding.directory().metadata().unwrap().mode() & 0o7777, 0o700);
        assert!(Arc::ptr_eq(holding.context().placement(), context.placement()));
        holding.context().validate_payload_root(holding.directory()).unwrap();
        assert_eq!(names(context.placement().directory()), [OsString::from("holding")]);
        let (mut publication, retained, slot) = holding.into_parts();
        assert_eq!(publication.identity(), {
            let held = publication.directory().metadata().unwrap(); (held.dev(), held.ino())
        });
        publication.cleanup_empty().unwrap(); sync_directory(retained.placement().directory()).unwrap();
        assert!(publication.is_removed());
        drop((publication, retained, slot));
    }

    #[test]
    fn existing_holding_and_pre_effect_cancellation_leave_names_untouched() {
        let (_source, context) = context(); let parent = context.placement().directory();
        fs::create_dir(parent.path().join("existing")).unwrap();
        fs::write(parent.path().join("existing/foreign"), b"preserved").unwrap();
        let before = names(parent);
        let error = prepare_recovery_directory(context.reserve_directory().unwrap(), OsStr::new("existing"), || false).unwrap_err();
        assert!(matches!(error, RecoveryDirectoryError::Rejected(ref e) if e.kind() == io::ErrorKind::AlreadyExists));
        let error = prepare_recovery_directory(context.reserve_directory().unwrap(), OsStr::new("cancelled"), || true).unwrap_err();
        assert!(matches!(error, RecoveryDirectoryError::Rejected(ref e) if e.kind() == io::ErrorKind::Interrupted));
        assert_eq!(names(parent), before);
        assert_eq!(fs::read(parent.path().join("existing/foreign")).unwrap(), b"preserved");
    }

    #[test]
    fn cancellation_after_publication_retains_exact_replacement_safe_cleanup() {
        let (_source, context) = context(); let parent = context.placement().directory();
        let calls = Cell::new(0);
        let error = prepare_recovery_directory(context.reserve_directory().unwrap(), OsStr::new("holding"), || {
            calls.set(calls.get()+1);
            if calls.get() == 2 {
                fs::rename(parent.path().join("holding"), parent.path().join("original")).unwrap();
                fs::create_dir(parent.path().join("holding")).unwrap();
                fs::write(parent.path().join("holding/foreign"), b"preserved").unwrap();
                true
            } else { false }
        }).unwrap_err();
        let recovery = error.recovery().unwrap();
        assert!(!recovery.is_resolved()); assert!(recovery.retry().is_err());
        assert_eq!(fs::read(parent.path().join("holding/foreign")).unwrap(), b"preserved");
        fs::remove_file(parent.path().join("holding/foreign")).unwrap();
        fs::remove_dir(parent.path().join("holding")).unwrap();
        fs::rename(parent.path().join("original"), parent.path().join("holding")).unwrap();
        recovery.retry().unwrap(); assert!(recovery.is_resolved());
        assert!(names(parent).is_empty());
    }

    #[test]
    fn moved_actual_holding_refuses_retry_inside_checkout_then_resolves_after_repair() {
        let (source, context) = context(); let parent = context.placement().directory();
        let calls = Cell::new(0);
        let error = prepare_recovery_directory(context.reserve_directory().unwrap(), OsStr::new("holding"), || {
            calls.set(calls.get()+1);
            if calls.get() == 2 {
                fs::rename(parent.path().join("holding"), source.path().join("moved-holding")).unwrap(); true
            } else { false }
        }).unwrap_err();
        let recovery = error.recovery().unwrap();
        assert!(recovery.retry().is_err());
        assert!(source.path().join("moved-holding").is_dir());
        fs::rename(source.path().join("moved-holding"), parent.path().join("holding")).unwrap();
        recovery.retry().unwrap(); assert!(recovery.is_resolved()); assert!(names(parent).is_empty());
    }
}
