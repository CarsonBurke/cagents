//! Isolated test-only domain issuance. Never a production registry substitute.
use super::*;
use crate::fs_safety::{RecoveryRegistrationLease, RecoveryRegistryCheck, recovery_directory_stamp};
use std::{os::unix::fs::MetadataExt, sync::Mutex};

const MAX_PAYLOADS: usize = 16;

#[derive(Default)]
struct CreationState { sealed: bool, in_flight: usize }
struct CreationGuard(Arc<Mutex<CreationState>>);
impl Drop for CreationGuard {
    fn drop(&mut self) {
        // Counter-only test admission: no I/O, hidden FDs or native
        // production issuer substitution. The SAME slot also retains context.
        let mut state = self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        state.in_flight = state.in_flight.checked_sub(1).expect("one admission per guard");
    }
}

struct OwnedLease<G> {
    // TempDir ownership preserves the complete isolated test topology. Neither
    // TempDir nor the optional test guard hides additional native descriptors.
    _outside: tempfile::TempDir,
    _runtime: tempfile::TempDir,
    _guard: G,
    payloads: Mutex<Vec<Root>>,
    creation: Arc<Mutex<CreationState>>,
}
impl<G: Send + Sync> RecoveryRegistrationLease for OwnedLease<G> {
    fn validate(&self) -> io::Result<()> {
        let payloads = self.payloads.lock().map_err(|_| io::Error::other("fixture registration lock poisoned"))?;
        for payload in payloads.iter() { payload.metadata()?; }
        Ok(())
    }
    fn begin_payload_creation(&self) -> io::Result<Arc<dyn Send + Sync>> {
        let mut state = self.creation.lock().map_err(|_| io::Error::other("fixture creation lock poisoned"))?;
        if state.sealed { return Err(io::Error::other("fixture creation admission is sealed")); }
        state.in_flight = state.in_flight.checked_add(1).ok_or_else(|| io::Error::other("fixture creation count exhausted"))?;
        Ok(Arc::new(CreationGuard(self.creation.clone())))
    }
    fn descriptor_budget(&self) -> usize { MAX_PAYLOADS }
    fn register_payload(&self, payload: &Root) -> io::Result<()> {
        let stamp = recovery_directory_stamp(payload)?;
        let mut payloads = self.payloads.lock().map_err(|_| io::Error::other("fixture registration lock poisoned"))?;
        // Positively unlinked original holdings cannot enclose future payloads.
        // Retire only those exact held identities; live/unknown obligations
        // survive and still consume their pre-reserved descriptor capacity.
        let mut index = 0;
        while index < payloads.len() {
            if payloads[index].metadata()?.nlink() == 0 { payloads.swap_remove(index); }
            else { index += 1; }
        }
        for held in payloads.iter() {
            if recovery_directory_stamp(held)? == stamp { return Ok(()); }
        }
        if payloads.len() == MAX_PAYLOADS { return Err(io::Error::other("fixture payload reservation exhausted")); }
        payloads.push(payload.try_clone()?);
        Ok(())
    }
}

/// Real isolated cleanup flock plus an owned, bounded registration lease. The
/// source must itself be an isolated test root; actual production intake is
/// deliberately unavailable through this cfg(test)-only helper.
pub(crate) fn fixture_context(source: &Root) -> io::Result<FilesystemRecoveryContext> {
    fixture_context_with_guard(source, ())
}

/// `guard` is test ownership/observation state with no native descriptors.
pub(crate) fn fixture_context_with_guard(source: &Root, guard: impl Send + Sync + 'static) -> io::Result<FilesystemRecoveryContext> {
    let parent = source.path().parent().ok_or_else(|| io::Error::other("fixture source has no outside parent"))?;
    let outside = tempfile::Builder::new().prefix("cagents-recovery-fixture-").tempdir_in(parent)?;
    let runtime = tempfile::tempdir()?;
    let protection = Arc::new(Some(crate::worktree_cleanup::fixture_protection(runtime.path(), source.path(), false)?));
    let source = Arc::new(source.try_clone()?);
    let directory = Arc::new(Root::open(outside.path()).map_err(|error| io::Error::other(error.to_string()))?);
    let registry = RecoveryRegistryCheck::from_lease(Arc::new(OwnedLease { _outside: outside, _runtime: runtime, _guard: guard, payloads: Mutex::new(Vec::new()), creation: Arc::default() }));
    let placement = RecoveryPlacement::acquire(directory, source.clone(), Arc::from([source.clone()]), registry)?;
    FilesystemRecoveryContext::new(&source, protection, placement)
}

#[test]
#[cfg(target_os = "linux")]
fn fixture_owns_real_enrollment_and_cleanup_lease() {
    let source = tempfile::tempdir().unwrap();
    let source = Root::open(source.path()).unwrap();
    let context = fixture_context(&source).unwrap();
    let placement = context.placement();
    let crate::fs_safety::DirectoryCreation::Published(publication) = crate::fs_safety::create_directory_in_namespace(placement.namespace(), placement.directory(), std::path::Path::new("payload"), 0o700).unwrap() else { panic!("fixture requires actual publication"); };
    placement.register_payload_root(publication.directory()).unwrap();
    placement.register_payload_root(publication.directory()).unwrap();
    assert!(context.retained_descriptor_count() >= MAX_PAYLOADS + 3);
}

#[test]
#[cfg(target_os = "linux")]
fn fixture_reuses_bounded_capacity_only_after_original_holdings_are_unlinked() {
    let source_directory = tempfile::tempdir().unwrap();
    let source = Root::open(source_directory.path()).unwrap();
    let context = fixture_context(&source).unwrap();
    let placement = context.placement();
    for index in 0..MAX_PAYLOADS + 2 {
        let name = format!("payload-{index}");
        let crate::fs_safety::DirectoryCreation::Published(mut publication) = crate::fs_safety::create_directory_in_namespace(placement.namespace(), placement.directory(), std::path::Path::new(&name), 0o700).unwrap() else { panic!("fixture requires actual publication"); };
        placement.register_payload_root(publication.directory()).unwrap();
        publication.cleanup_empty().unwrap();
    }
}

#[test]
fn fixture_creation_admission_rejects_after_seal_and_counts_last_guard_drop() {
    let lease = OwnedLease { _outside: tempfile::tempdir().unwrap(),
        _runtime: tempfile::tempdir().unwrap(), _guard: (),
        payloads: Mutex::new(Vec::new()), creation: Arc::default() };
    let original = lease.begin_payload_creation().unwrap();
    let retained = original.clone();
    lease.creation.lock().unwrap().sealed = true;
    assert!(lease.begin_payload_creation().is_err());
    assert_eq!(lease.creation.lock().unwrap().in_flight, 1);
    drop(original);
    assert_eq!(lease.creation.lock().unwrap().in_flight, 1, "a retained recovery slot still owns creation admission");
    lease.validate().unwrap();
    drop(retained);
    assert_eq!(lease.creation.lock().unwrap().in_flight, 0);
    assert!(lease.begin_payload_creation().is_err(), "retirement seals further creation even after old guards drain");
}
