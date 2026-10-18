//! Local blocking-worker ownership for recoverable filesystem operations.
//!
//! The supplied paths are a captured negative checkout census, not global root
//! registration. Original source/parent descriptors and cleanup protection
//! remain held through the transaction and unresolved recovery. New external
//! repositories and topology changes after the last check are explicit limits.
use crate::{
    fs_safety::{
        OwnedPrivateSibling, PrivateSiblingCreateError, RecoveryParentCandidates,
        RecoveryPlacement, Root, StickyParent,
    },
    recovery_authority::{RecoveryAuthority, RecoveryContext, RecoverySlot},
    worktree_cleanup::Protection,
};
use std::{
    io,
    path::PathBuf,
    sync::{Arc, Mutex},
};

#[derive(Debug)]
pub(crate) struct RecoveryAcquireError {
    pub(crate) message: String,
    pub(crate) authority: Option<RecoveryAuthority>,
}
impl From<io::Error> for RecoveryAcquireError {
    fn from(error: io::Error) -> Self {
        Self {
            message: error.to_string(),
            authority: None,
        }
    }
}
impl std::fmt::Display for RecoveryAcquireError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for RecoveryAcquireError {}

#[derive(Default)]
struct BootstrapOwner {
    sibling: Option<OwnedPrivateSibling>,
}
impl BootstrapOwner {
    fn cleanup(&mut self) -> Result<(), String> {
        if let Some(sibling) = &mut self.sibling {
            sibling.cleanup_empty().map_err(|error| error.to_string())?;
        }
        Ok(())
    }
}
struct BootstrapCleanup {
    owner: Arc<Mutex<BootstrapOwner>>,
    slot: Option<RecoverySlot>,
}

pub(crate) struct FilesystemRecoveryReservation {
    context: FilesystemRecoveryContext,
    slot: RecoverySlot,
}
impl FilesystemRecoveryReservation {
    pub(crate) fn into_parts(self) -> (FilesystemRecoveryContext, RecoverySlot) {
        (self.context, self.slot)
    }
}

#[derive(Clone)]
pub(crate) struct FilesystemRecoveryContext {
    placement: Arc<RecoveryPlacement>,
    context: RecoveryContext,
    bootstrap: Option<Arc<Mutex<BootstrapCleanup>>>,
}
impl std::fmt::Debug for FilesystemRecoveryContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FilesystemRecoveryContext")
            .field("placement", &self.placement)
            .finish_non_exhaustive()
    }
}
impl FilesystemRecoveryContext {
    /// No recovery directory is created until a protected, same-filesystem
    /// parent outside the source and captured known checkout roots is held.
    pub(crate) fn acquire(source: &Root, known: &[PathBuf]) -> Result<Self, RecoveryAcquireError> {
        if known.len() > 256 {
            return Err(io::Error::other(
                "The known checkout census exceeds the bounded recovery inspection; no file was changed",
            ).into());
        }
        let protection = crate::worktree_cleanup::protect_root_operation(source)?;
        let source = Arc::new(source.try_clone()?);
        let mut excluded = Vec::with_capacity(known.len());
        for path in known {
            match Root::open(path) {
                Ok(root) => excluded.push(Arc::new(root)),
                // An absent negative-census spelling cannot contain the
                // current holding. The actual source is separately pinned.
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        let excluded: Arc<[Arc<Root>]> = excluded.into();
        let mut last_error = None;
        let mut sticky_candidates = Vec::new();
        for parent in RecoveryParentCandidates::new(source.clone())? {
            let parent = parent?;
            match RecoveryPlacement::acquire(parent.clone(), source.clone(), excluded.clone()) {
                Ok(placement) => {
                    return Self::from_placement(&source, protection, placement, ())
                        .map_err(Into::into);
                }
                Err(error) => last_error = Some(error),
            }
            if let Ok(sticky) = StickyParent::acquire(parent.clone()) {
                sticky_candidates.push((parent, sticky));
            }
        }
        // Preserve ordinary protected-parent preference. A usable higher
        // parent needs no new bootstrap and must not be hidden by a lower
        // sticky candidate. Both candidate sets and the walk are bounded.
        let mut selected_sticky = None;
        for (parent, sticky) in sticky_candidates {
            match crate::fs_safety::validate_recovery_parent(&parent, &source, &excluded) {
                Ok(()) => {
                    selected_sticky = Some(sticky);
                    break;
                }
                Err(error) => last_error = Some(error),
            }
        }
        // Release every unselected candidate before reserving the measured
        // bootstrap capacity and performing its first filesystem effect.
        if let Some(sticky) = selected_sticky {
            return Self::bootstrap(source, excluded, protection, sticky);
        }
        Err(last_error.unwrap_or_else(|| io::Error::other(
            "No protected recovery parent outside the captured checkout roots exists on the source filesystem; no file was changed",
        )).into())
    }

    fn bootstrap(
        source: Arc<Root>,
        excluded: Arc<[Arc<Root>]>,
        protection: Protection,
        sticky: Arc<StickyParent>,
    ) -> Result<Self, RecoveryAcquireError> {
        let owner = Arc::new(Mutex::new(BootstrapOwner::default()));
        let mut descriptors = std::collections::HashSet::from([source.descriptor()]);
        descriptors.extend(excluded.iter().map(|root| root.descriptor()));
        // Reserve capacity BEFORE mkdir: one sticky-parent handle and at most
        // five additional handles cover child, placement namespace, and the
        // bounded transient ancestry/cleanup walk. This conservative local
        // reservation remains charged through unresolved cleanup.
        let capacity = descriptors.len()
            + protection.retained_descriptor_count()
            + sticky.retained_descriptor_count()
            + 5;
        let context = RecoveryContext::with_guard(
            (
                source.clone(),
                excluded.clone(),
                protection,
                sticky.clone(),
                owner.clone(),
            ),
            capacity,
        )
        .map_err(|message| RecoveryAcquireError {
            message,
            authority: None,
        })?;
        let slot = context.reserve().map_err(|message| RecoveryAcquireError {
            message,
            authority: None,
        })?;
        let sibling = match sticky.create_private_sibling() {
            Ok(sibling) => sibling,
            Err(PrivateSiblingCreateError::Rejected(error)) => return Err(error.into()),
            Err(PrivateSiblingCreateError::Retained { cause, sibling }) => {
                owner
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .sibling = Some(sibling);
                let retry_owner = owner.clone();
                let authority = slot.retain(
                    "Private recovery parent preparation needs cleanup".into(),
                    move || {
                        retry_owner
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .cleanup()
                    },
                );
                return Err(RecoveryAcquireError {
                    message: cause.to_string(),
                    authority: Some(authority),
                });
            }
        };
        let directory = sibling.directory_arc();
        owner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .sibling = Some(sibling);
        let placement =
            directory.and_then(|directory| RecoveryPlacement::acquire(directory, source, excluded));
        let cleanup = Arc::new(Mutex::new(BootstrapCleanup {
            owner,
            slot: Some(slot),
        }));
        match placement {
            Ok(placement) => Ok(Self {
                placement,
                context,
                bootstrap: Some(cleanup),
            }),
            Err(error) => {
                let authority = Self::finish_bootstrap(&cleanup, None);
                Err(RecoveryAcquireError {
                    message: error.to_string(),
                    authority,
                })
            }
        }
    }

    /// Resolve the owned sibling only after all leaf payloads have resolved.
    /// A reserved retry owns both original guards and the exact creation
    /// receipt; Drop never mutates names and never recursively removes data.
    pub(crate) fn finish(&self, leaf: Option<RecoveryAuthority>) -> Option<RecoveryAuthority> {
        self.bootstrap
            .as_ref()
            .and_then(|cleanup| Self::finish_bootstrap(cleanup, leaf))
    }
    fn finish_bootstrap(
        cleanup: &Arc<Mutex<BootstrapCleanup>>,
        leaf: Option<RecoveryAuthority>,
    ) -> Option<RecoveryAuthority> {
        let mut cleanup = cleanup
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let slot = cleanup.slot.take()?;
        let owner = cleanup.owner.clone();
        let pending_leaf = leaf.filter(|leaf| !leaf.is_resolved());
        if pending_leaf.is_none()
            && owner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .cleanup()
                .is_ok()
        {
            return None;
        }
        Some(slot.retain(
            "Private recovery parent cleanup remains pending".into(),
            move || {
                if let Some(leaf) = &pending_leaf {
                    leaf.retry()?;
                }
                owner
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .cleanup()
            },
        ))
    }

    fn from_placement(
        source: &Root,
        protection: Protection,
        placement: Arc<RecoveryPlacement>,
        owner: impl Send + Sync + 'static,
    ) -> io::Result<Self> {
        placement.validate_for_source(source)?;
        if !protection.protects_root(source)? {
            return Err(io::Error::other(
                "Recovery cleanup protection belongs to another source root",
            ));
        }
        let descriptors = placement
            .retained_descriptor_count()
            .checked_add(protection.retained_descriptor_count())
            .ok_or_else(|| io::Error::other("Recovery descriptor accounting overflowed"))?;
        // The SAME context owns these actual native handles. Its charge stays
        // until the guard tuple has dropped, including unresolved leaf retries.
        let context =
            RecoveryContext::with_guard((owner, protection, placement.clone()), descriptors)
                .map_err(io::Error::other)?;
        Ok(Self {
            placement,
            context,
            bootstrap: None,
        })
    }

    pub(crate) fn validate_for_source(&self, source: &Root) -> io::Result<()> {
        self.placement.validate_for_source(source)
    }
    pub(crate) fn placement(&self) -> &Arc<RecoveryPlacement> {
        &self.placement
    }
    pub(crate) fn validate_payload_root(&self, payload: &Root) -> io::Result<()> {
        self.placement.validate_payload_root(payload)
    }
    pub(crate) fn reserve_directory(&self) -> Result<FilesystemRecoveryReservation, String> {
        let slot = self.reserve()?;
        Ok(FilesystemRecoveryReservation {
            context: self.clone(),
            slot,
        })
    }
    pub(crate) fn reserve(&self) -> Result<RecoverySlot, String> {
        self.placement
            .validate()
            .map_err(|error| error.to_string())?;
        self.context.reserve()
    }
}

#[cfg(all(test, target_os = "linux"))]
pub(crate) mod fixtures {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    pub(crate) fn fixture_context(source: &Root) -> io::Result<FilesystemRecoveryContext> {
        fixture_context_with_guard(source, ())
    }
    pub(crate) fn fixture_context_with_guard(
        source: &Root,
        guard: impl Send + Sync + 'static,
    ) -> io::Result<FilesystemRecoveryContext> {
        let runtime = tempfile::Builder::new()
            .permissions(std::fs::Permissions::from_mode(0o700))
            .tempdir()?;
        let outside = tempfile::Builder::new()
            .permissions(std::fs::Permissions::from_mode(0o700))
            .tempdir()?;
        let protection =
            crate::worktree_cleanup::fixture_protection(runtime.path(), source.path(), false)?;
        let source = Arc::new(source.try_clone()?);
        let placement = RecoveryPlacement::acquire(
            Arc::new(Root::open(outside.path())?),
            source.clone(),
            Arc::from([source.clone()]),
        )?;
        FilesystemRecoveryContext::from_placement(
            &source,
            protection,
            placement,
            (guard, runtime, outside),
        )
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    struct Charge(Arc<AtomicBool>);
    impl Drop for Charge {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }
    #[test]
    fn sticky_bootstrap_cleanup_removes_only_its_owned_empty_sibling() {
        let directory = tempfile::tempdir_in("/tmp").unwrap();
        let runtime = tempfile::tempdir().unwrap();
        let source = Arc::new(Root::open(directory.path()).unwrap());
        let sticky =
            StickyParent::acquire(Arc::new(Root::open(std::path::Path::new("/tmp")).unwrap()))
                .unwrap();
        let protection =
            crate::worktree_cleanup::fixture_protection(runtime.path(), source.path(), false)
                .unwrap();
        let context = FilesystemRecoveryContext::bootstrap(
            source.clone(),
            Arc::from([source]),
            protection,
            sticky,
        )
        .unwrap();
        let owned = context.placement().directory().path().to_owned();
        assert!(owned.is_dir());
        assert!(context.finish(None).is_none());
        assert!(!owned.exists());
        assert!(context.finish(None).is_none());
    }

    #[test]
    fn sticky_bootstrap_retry_resolves_leaf_before_parent_cleanup() {
        let directory = tempfile::tempdir_in("/tmp").unwrap();
        let runtime = tempfile::tempdir().unwrap();
        let source = Arc::new(Root::open(directory.path()).unwrap());
        let sticky =
            StickyParent::acquire(Arc::new(Root::open(std::path::Path::new("/tmp")).unwrap()))
                .unwrap();
        let protection =
            crate::worktree_cleanup::fixture_protection(runtime.path(), source.path(), false)
                .unwrap();
        let context = FilesystemRecoveryContext::bootstrap(
            source.clone(),
            Arc::from([source]),
            protection,
            sticky,
        )
        .unwrap();
        let owned = context.placement().directory().path().to_owned();
        let payload = owned.join("retained-leaf");
        std::fs::write(&payload, b"preserve until leaf retry").unwrap();
        let leaf = RecoverySlot::reserve()
            .unwrap()
            .retain("owning leaf fixture".into(), move || {
                std::fs::remove_file(&payload).map_err(|error| error.to_string())
            });
        let authority = context
            .finish(Some(leaf))
            .expect("pending leaf retains parent");
        drop(context);
        assert!(owned.join("retained-leaf").is_file());
        authority.retry().unwrap();
        assert!(authority.is_resolved());
        assert!(!owned.exists());
    }

    #[test]
    fn sticky_bootstrap_nonempty_cleanup_retains_original_for_retry() {
        let directory = tempfile::tempdir_in("/tmp").unwrap();
        let runtime = tempfile::tempdir().unwrap();
        let source = Arc::new(Root::open(directory.path()).unwrap());
        let sticky =
            StickyParent::acquire(Arc::new(Root::open(std::path::Path::new("/tmp")).unwrap()))
                .unwrap();
        let protection =
            crate::worktree_cleanup::fixture_protection(runtime.path(), source.path(), false)
                .unwrap();
        let context = FilesystemRecoveryContext::bootstrap(
            source.clone(),
            Arc::from([source]),
            protection,
            sticky,
        )
        .unwrap();
        let owned = context.placement().directory().path().to_owned();
        std::fs::write(owned.join("preserve"), b"newer").unwrap();
        let authority = context
            .finish(None)
            .expect("nonempty parent retains cleanup");
        drop(context);
        assert_eq!(std::fs::read(owned.join("preserve")).unwrap(), b"newer");
        assert!(authority.retry().is_err());
        std::fs::remove_file(owned.join("preserve")).unwrap();
        authority.retry().unwrap();
        assert!(authority.is_resolved());
        assert!(!owned.exists());
    }

    #[test]
    fn actual_preparation_failure_retains_original_file_cleanup_lease_and_charge() {
        let source = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let runtime = tempfile::tempdir().unwrap();
        let source_root = Arc::new(Root::open(source.path()).unwrap());
        let placement = RecoveryPlacement::acquire(
            Arc::new(Root::open(outside.path()).unwrap()),
            source_root.clone(),
            Arc::from([source_root.clone()]),
        )
        .unwrap();
        let protection =
            crate::worktree_cleanup::fixture_protection(runtime.path(), source.path(), false)
                .unwrap();
        let released = Arc::new(AtomicBool::new(false));
        let context = FilesystemRecoveryContext::from_placement(
            &source_root,
            protection,
            placement,
            Charge(released.clone()),
        )
        .unwrap();
        let original_file = Arc::new(tempfile::tempfile().unwrap());
        let witness = Arc::downgrade(&original_file);
        let error = crate::fs_safety::fixture_recovery_directory_preparation_failure(
            context.reserve_directory().unwrap(),
            original_file,
        );
        let authority = match error {
            crate::fs_safety::RecoveryDirectoryError::Retained { recovery, .. } => recovery,
            other => panic!("expected actual retained preparation, got {other:?}"),
        };
        drop(context);
        assert!(witness.upgrade().is_some());
        assert!(!released.load(Ordering::SeqCst));
        assert!(
            crate::worktree_cleanup::fixture_protection(runtime.path(), source.path(), true)
                .is_err()
        );
        assert!(authority.retry().is_err());
        assert!(witness.upgrade().is_some());
        let artifact = std::fs::read_dir(outside.path())
            .unwrap()
            .map(|entry| entry.unwrap().path().join("directory"))
            .find(|path| path.is_file())
            .expect("real producer's private partial artifact");
        assert_eq!(
            std::fs::read(&artifact).unwrap(),
            b"retained fixture artifact"
        );
        std::fs::remove_file(artifact).unwrap();
        authority.retry().unwrap();
        assert!(authority.is_resolved());
        assert!(
            witness.upgrade().is_none(),
            "original file closes before its charge releases"
        );
        assert!(released.load(Ordering::SeqCst));
        assert!(
            crate::worktree_cleanup::fixture_protection(runtime.path(), source.path(), true)
                .is_ok()
        );
        assert_eq!(std::fs::read_dir(outside.path()).unwrap().count(), 0);
    }
}
