//! Worker-side composition of exact cleanup and outside-checkout placement.
//!
//! This is deliberately separate from the std-only generic recovery carrier.
//! A lexical path or a newly successful callback cannot construct domain
//! registration authority. Actual operation intake supplies that proof.
use std::{io, sync::Arc};
use crate::{fs_safety::{RecoveryPlacement, Root}, recovery_authority::{RecoveryContext, RecoverySlot}, worktree_cleanup::Protection};

/// Preeffect factory admission bound to its exact issued context. The private
/// pair cannot substitute another placement after reserving scarce ownership.
/// A factory transfers this SAME reservation to the successful producer, or
/// retains it with the entire typed publication/preparation on failure.
pub(crate) struct FilesystemRecoveryReservation {
    context: FilesystemRecoveryContext,
    slot: RecoverySlot,
}
impl FilesystemRecoveryReservation {
    pub(crate) fn context(&self) -> &FilesystemRecoveryContext { &self.context }
    pub(crate) fn into_parts(self) -> (FilesystemRecoveryContext, RecoverySlot) { (self.context, self.slot) }
}

#[derive(Clone)]
pub struct FilesystemRecoveryContext {
    // Drop our accessor reference before the charged generic guard. Slots
    // retain that guard (including the SAME placement Arc) after callers drop.
    placement: Arc<RecoveryPlacement>,
    descriptors: usize,
    context: RecoveryContext,
}
impl std::fmt::Debug for FilesystemRecoveryContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FilesystemRecoveryContext").field("retained_descriptors", &self.descriptors).finish_non_exhaustive()
    }
}
impl FilesystemRecoveryContext {
    /// Worker-only, before effects. `protection` is the EXACT already-acquired
    /// source lease, not a later path lookup. Missing strong protection refuses;
    /// no unprotected compatibility policy is inferred from an Option.
    pub fn new(source: &Root, protection: Arc<Option<Protection>>, placement: Arc<RecoveryPlacement>) -> io::Result<Self> {
        placement.validate_for_source(source)?;
        let lease = protection.as_ref().as_ref().ok_or_else(|| io::Error::new(io::ErrorKind::Unsupported,
            "A protected recovery placement requires an active cleanup lease; no file was changed."))?;
        if !lease.protects_root(source)? {
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, "The cleanup lease belongs to a different source root; no file was changed."));
        }
        let descriptors = lease.retained_descriptor_count().checked_add(placement.retained_descriptor_count())
            .ok_or_else(|| io::Error::other("Recovery descriptor accounting overflow; no file was changed."))?;
        // Shared native placement/registry and Protection descriptors are
        // measured/bounded once in this context, including the registry lease
        // reservation for future local payload handles; never as a zero-cost Arc or
        // hidden in each producer's separate <=16 private-descriptor allowance.
        let context = RecoveryContext::with_guard((protection, Arc::clone(&placement)), descriptors).map_err(io::Error::other)?;
        Ok(Self { placement, descriptors, context })
    }
    /// Reprove the actual effect source on the blocking worker before public
    /// acquisition/publication. A context for A never authorizes another Root B.
    pub fn validate_for_source(&self, source: &Root) -> io::Result<()> { self.placement.validate_for_source(source) }
    /// Original retained outside parent and namespace. These are distinct from
    /// the source parent used for acquisition, putback and source fsync.
    pub fn placement(&self) -> &Arc<RecoveryPlacement> { &self.placement }
    /// Enroll the actual newly Published holding before the first payload write
    /// or source acquisition. Failure may follow a partial registry handoff:
    /// keep this SAME context and publication in the pre-reserved owner.
    pub fn register_payload_root(&self, payload: &Root) -> io::Result<()> {
        self.placement.register_payload_root(payload)
    }
    /// Validate the actual live holding, not just its original parent. A public
    /// move into any checkout refuses subsequent payload and cleanup effects.
    /// Authenticated removed holdings need only complete retained-parent sync;
    /// they must not try to rediscover ancestry after unlink.
    pub fn validate_payload_root(&self, payload: &Root) -> io::Result<()> {
        self.placement.validate_payload_root(payload)
    }
    /// Explicit Root handles and the lease's reserved maximum local handles.
    /// Remote daemon reservations are accounted by their owning issuer.
    pub fn retained_descriptor_count(&self) -> usize { self.descriptors }
    /// Reserve before producer effects. The unresolved leaf retains the exact
    /// native context through failed retry and actual eventual resolution.
    /// The candidate parent must ALREADY be issued and protected. This does
    /// not bootstrap the parent needed to construct this context itself.
    pub(crate) fn reserve_directory(&self) -> Result<FilesystemRecoveryReservation, String> {
        let slot = self.reserve()?;
        Ok(FilesystemRecoveryReservation { context: self.clone(), slot })
    }
    pub(crate) fn reserve(&self) -> Result<RecoverySlot, String> {
        // Cleanup validation remains available after retirement. NEW directory
        // creation needs a separate atomic admission retained by this SAMEslot.
        let guard = self.placement.begin_payload_creation().map_err(|error| error.to_string())?;
        self.context.reserve().map(|slot| slot.with_creation_guard(guard))
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use crate::fs_safety::RecoveryRegistryCheck;
    fn fixture() -> (tempfile::TempDir, tempfile::TempDir, Arc<Root>, Arc<RecoveryPlacement>, Arc<Option<Protection>>) {
        let directory = tempfile::tempdir().unwrap(); let runtime = tempfile::tempdir().unwrap();
        let source = directory.path().join("source"); let outside = directory.path().join("recovery");
        std::fs::create_dir(&source).unwrap(); std::fs::create_dir(&outside).unwrap();
        let source = Arc::new(Root::open(&source).unwrap()); let outside = Arc::new(Root::open(&outside).unwrap());
        // Static fixture enrollment is an owned bounded lease, never a
        // callback-only production compatibility policy.
        drop(outside);
        let fixture = super::fixtures::fixture_context(&source).unwrap();
        let placement = fixture.placement().clone();
        drop(fixture);
        let protection = Arc::new(Some(crate::worktree_cleanup::fixture_protection(runtime.path(), source.path(), false).unwrap()));
        (directory, runtime, source, placement, protection)
    }
    #[test]
    fn same_context_keeps_measured_placement_and_actual_cleanup_lease_until_resolution() {
        let (_directory, runtime, source, placement, protection) = fixture();
        let measured = placement.retained_descriptor_count() + protection.as_ref().as_ref().unwrap().retained_descriptor_count();
        let context = FilesystemRecoveryContext::new(&source, protection, placement).unwrap();
        assert_eq!(context.retained_descriptor_count(), measured);
        let authority = context.reserve().unwrap().retain("isolated exact context".into(), || Ok(()));
        drop(context);
        assert!(crate::worktree_cleanup::fixture_protection(runtime.path(), source.path(), true).is_err());
        authority.retry().unwrap(); assert!(authority.is_resolved());
        drop(crate::worktree_cleanup::fixture_protection(runtime.path(), source.path(), true).unwrap());
    }
    #[test]
    fn a_different_source_or_missing_actual_lease_refuses_before_any_effect() {
        let (directory, runtime, source, placement, protection) = fixture();
        assert!(FilesystemRecoveryContext::new(&source, Arc::new(None), placement.clone()).is_err());
        let other = directory.path().join("other"); std::fs::create_dir(&other).unwrap(); let other = Root::open(&other).unwrap();
        let foreign_lease = Arc::new(Some(crate::worktree_cleanup::fixture_protection(runtime.path(), other.path(), false).unwrap()));
        let error = FilesystemRecoveryContext::new(&source, foreign_lease, placement.clone()).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied, "matching source placement must still reject a different source's actual cleanup lease");
        assert!(FilesystemRecoveryContext::new(&other, protection, placement).is_err());
        assert!(std::fs::read_dir(source.path()).unwrap().next().is_none());
        assert!(std::fs::read_dir(other.path()).unwrap().next().is_none());
    }
}

#[cfg(test)]
#[path = "filesystem_recovery_context/fixtures.rs"]
pub(crate) mod fixtures;
