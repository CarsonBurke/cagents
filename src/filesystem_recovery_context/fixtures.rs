//! Isolated test-only domain issuance. Never a production registry substitute.
use super::*;
use crate::fs_safety::{RecoveryRegistrationLease, RecoveryRegistryCheck, recovery_directory_stamp};
use std::{os::unix::fs::MetadataExt, sync::Mutex};

const MAX_PAYLOADS: usize = 16;

struct OwnedLease<G> {
    // TempDir ownership preserves the complete isolated test topology. Neither
    // TempDir nor the optional test guard hides additional native descriptors.
    _outside: tempfile::TempDir,
    _runtime: tempfile::TempDir,
    _guard: G,
    payloads: Mutex<Vec<Root>>,
}
impl<G: Send + Sync> RecoveryRegistrationLease for OwnedLease<G> {
    fn validate(&self) -> io::Result<()> {
        let payloads = self.payloads.lock().map_err(|_| io::Error::other("fixture registration lock poisoned"))?;
        for payload in payloads.iter() { payload.metadata()?; }
        Ok(())
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
    let registry = RecoveryRegistryCheck::from_lease(Arc::new(OwnedLease { _outside: outside, _runtime: runtime, _guard: guard, payloads: Mutex::new(Vec::new()) }));
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
