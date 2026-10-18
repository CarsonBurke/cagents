//! Local blocking-worker ownership for recoverable filesystem operations.
//!
//! The supplied paths are a captured negative checkout census, not global root
//! registration. Original source/parent descriptors and cleanup protection
//! remain held through the transaction and unresolved recovery. New external
//! repositories and topology changes after the last check are explicit limits.
use crate::{
    fs_safety::{RecoveryParentCandidates, RecoveryPlacement, Root},
    recovery_authority::{RecoveryContext, RecoverySlot},
    worktree_cleanup::Protection,
};
use std::{io, path::PathBuf, sync::Arc};

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
    pub(crate) fn acquire(source: &Root, known: &[PathBuf]) -> io::Result<Self> {
        if known.len() > 256 {
            return Err(io::Error::other(
                "The known checkout census exceeds the bounded recovery inspection; no file was changed",
            ));
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
                Err(error) => return Err(error),
            }
        }
        let excluded: Arc<[Arc<Root>]> = excluded.into();
        let mut last_error = None;
        for parent in RecoveryParentCandidates::new(source.clone())? {
            let parent = parent?;
            match RecoveryPlacement::acquire(parent, source.clone(), excluded.clone()) {
                Ok(placement) => return Self::from_placement(&source, protection, placement, ()),
                Err(error) => last_error = Some(error),
            }
        }
        Err(last_error.unwrap_or_else(|| io::Error::other(
            "No protected recovery parent outside the captured checkout roots exists on the source filesystem; no file was changed",
        )))
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
        Ok(Self { placement, context })
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
