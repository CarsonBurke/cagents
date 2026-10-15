//! Test-only real domain fixture; no production registration policy.
use std::{io, os::unix::fs::MetadataExt, path::Path, process::{Command, Stdio}, sync::{Arc, Mutex}, time::{Duration, Instant}};
use cagents::{filesystem_recovery_context::FilesystemRecoveryContext, fs_safety::{RecoveryPlacement, RecoveryRegistrationLease, RecoveryRegistryCheck, Root, recovery_directory_stamp}};

/// The process environment is isolated by the owning test process BEFORE any
/// cleanup lease or Trash operation. No Rust process-global env mutation or
/// initialized-window bypass grants the operation its real cleanup Protection.
pub(super) fn run_isolated_case(case: &str) -> bool {
    if std::env::var("CAGENTS_RECOVERY_CASE").as_deref() == Ok(case) {
        let runtime = std::env::var_os("CAGENTS_RECOVERY_RUNTIME").expect("owned runtime marker");
        assert_eq!(std::env::var_os("XDG_RUNTIME_DIR"), Some(runtime.clone()));
        let metadata = std::fs::metadata(&runtime).unwrap();
        assert_eq!(metadata.uid(), unsafe { libc::geteuid() });
        assert_eq!(metadata.mode() & 0o777, 0o700);
        return false;
    }
    let runtime = tempfile::Builder::new().prefix("cagents-recovery-runtime-").tempdir().unwrap();
    let data = tempfile::Builder::new().prefix("cagents-recovery-data-").tempdir().unwrap();
    let mut command = Command::new(std::env::current_exe().unwrap());
    for (key, _) in std::env::vars_os() { if key.as_encoded_bytes().starts_with(b"GIT_") { command.env_remove(key); } }
    command.args([case, "--exact", "--nocapture", "--test-threads=1"])
        .env("CAGENTS_RECOVERY_CASE", case).env("CAGENTS_RECOVERY_RUNTIME", runtime.path())
        .env("XDG_RUNTIME_DIR", runtime.path()).env("XDG_DATA_HOME", data.path())
        .stdin(Stdio::null()).stdout(Stdio::inherit()).stderr(Stdio::inherit());
    struct Child(std::process::Child);
    impl Drop for Child { fn drop(&mut self) { let _ = self.0.kill(); let _ = self.0.wait(); } }
    let mut child = Child(command.spawn().unwrap());
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        if let Some(status) = child.0.try_wait().unwrap() { assert!(status.success(), "isolated recovery case {case} failed: {status}"); break; }
        assert!(Instant::now() < deadline, "isolated recovery case {case} timed out");
        std::thread::sleep(Duration::from_millis(10));
    }
    true
}

struct OwnedLease {
    _outside: tempfile::TempDir,
    payloads: Mutex<Vec<Root>>,
}
const MAX_PAYLOADS: usize = 16;
impl RecoveryRegistrationLease for OwnedLease {
    fn validate(&self) -> io::Result<()> {
        for root in self.payloads.lock().map_err(|_| io::Error::other("fixture lease poisoned"))?.iter() { root.metadata()?; }
        Ok(())
    }
    fn descriptor_budget(&self) -> usize { MAX_PAYLOADS }
    fn register_payload(&self, payload: &Root) -> io::Result<()> {
        let stamp = recovery_directory_stamp(payload)?;
        let mut roots = self.payloads.lock().map_err(|_| io::Error::other("fixture lease poisoned"))?;
        let mut index = 0;
        while index < roots.len() {
            if roots[index].metadata()?.nlink() == 0 { roots.swap_remove(index); } else { index += 1; }
        }
        for root in roots.iter() { if recovery_directory_stamp(root)? == stamp { return Ok(()); } }
        if roots.len() == MAX_PAYLOADS { return Err(io::Error::other("fixture live payload budget exhausted")); }
        roots.push(payload.try_clone()?);
        Ok(())
    }
}

pub(super) fn context(path: &Path) -> FilesystemRecoveryContext {
    assert!(std::env::var_os("CAGENTS_RECOVERY_CASE").is_some(), "recovery fixture requires its isolated process");
    let source = Arc::new(Root::open(path).unwrap());
    let protection = Arc::new(Some(cagents::worktree_cleanup::protect_operation(&source).unwrap()));
    let outside = tempfile::Builder::new().prefix("cagents-integration-payload-").tempdir_in(path.parent().unwrap()).unwrap();
    let outside_root = Arc::new(Root::open(outside.path()).unwrap());
    let lease = Arc::new(OwnedLease { _outside: outside, payloads: Mutex::new(Vec::new()) });
    // The fixture owns the entire fresh source topology. This test-only owned
    // lease does not claim daemon registration/adoption exclusion coverage.
    let placement = RecoveryPlacement::acquire(outside_root, source.clone(), Arc::from([source.clone()]), RecoveryRegistryCheck::from_lease(lease)).unwrap();
    FilesystemRecoveryContext::new(&source, protection, placement).unwrap()
}
