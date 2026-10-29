//! Ports of public #1331's three real-service access contracts. The original
//! context factory was superseded: these enter through current public Discard,
//! then keep the actual original receipt/holding authority through Undo/retry.
use super::*;
use cagents::process::{ProcessFailure, ProcessOutput};
use std::{
    os::fd::AsRawFd,
    sync::{
        Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};

fn access_acl(uid: u32) -> Vec<u8> {
    let mut bytes = 2u32.to_le_bytes().to_vec();
    for (tag, permissions, id) in [
        (0x01u16, 6u16, u32::MAX),
        (0x02, 4, uid),
        (0x04, 0, u32::MAX),
        (0x10, 4, u32::MAX),
        (0x20, 0, u32::MAX),
    ] {
        bytes.extend_from_slice(&tag.to_le_bytes());
        bytes.extend_from_slice(&permissions.to_le_bytes());
        bytes.extend_from_slice(&id.to_le_bytes());
    }
    bytes
}
fn set_acl(file: &fs::File, name: &std::ffi::CStr, bytes: &[u8]) {
    // All descriptors belong to the isolated child fixture; never a user path.
    assert_eq!(
        unsafe {
            libc::fsetxattr(
                file.as_raw_fd(),
                name.as_ptr(),
                bytes.as_ptr().cast(),
                bytes.len(),
                0,
            )
        },
        0,
        "{}",
        std::io::Error::last_os_error()
    );
}
fn get_acl(file: &fs::File, name: &std::ffi::CStr) -> Option<Vec<u8>> {
    let size = unsafe { libc::fgetxattr(file.as_raw_fd(), name.as_ptr(), std::ptr::null_mut(), 0) };
    if size < 0 {
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ENODATA)
        );
        return None;
    }
    assert!(size <= 64 * 1024);
    let mut bytes = vec![0; size as usize];
    assert_eq!(
        unsafe {
            libc::fgetxattr(
                file.as_raw_fd(),
                name.as_ptr(),
                bytes.as_mut_ptr().cast(),
                bytes.len(),
            )
        },
        size
    );
    Some(bytes)
}
fn try_distinct_original_group(file: &fs::File) {
    let initial = file.metadata().unwrap().gid();
    let count = unsafe { libc::getgroups(0, std::ptr::null_mut()) };
    assert!((0..=65_536).contains(&count));
    let mut groups = vec![0; count as usize];
    assert_eq!(
        unsafe { libc::getgroups(count, groups.as_mut_ptr()) },
        count
    );
    let other = groups
        .into_iter()
        .find(|gid| *gid != initial)
        .or_else(|| (unsafe { libc::geteuid() } == 0).then_some(initial.wrapping_add(12_345)));
    if let Some(gid) = other
        && unsafe { libc::fchown(file.as_raw_fd(), u32::MAX, gid) } == 0
    {
        assert_eq!(file.metadata().unwrap().gid(), gid);
        eprintln!("GID_DIFFERENCE_EXERCISED {initial}->{gid}");
        return;
    }
    eprintln!(
        "NO_RESTORABLE_SECONDARY_GID: original equality still required; no distinct-GID positive credited"
    );
}

/// Intercepts the actual fixed recovery-object command, never substitutes a
/// Git result. It pins the real outside holding before the service publishes.
struct ObserveHolding {
    checkout: PathBuf,
    outside: PathBuf,
    armed: AtomicBool,
    inject: bool,
    calls: AtomicUsize,
    holding: Mutex<Option<fs::File>>,
    observed_default_acl: Mutex<Option<Vec<u8>>>,
}
impl ObserveHolding {
    fn new(checkout: &Path, inject: bool) -> Self {
        Self {
            checkout: checkout.to_owned(),
            outside: checkout.parent().unwrap().to_owned(),
            armed: AtomicBool::new(false),
            inject,
            calls: AtomicUsize::new(0),
            holding: Mutex::new(None),
            observed_default_acl: Mutex::new(None),
        }
    }
}
impl ProcessExecutor for ObserveHolding {
    fn execute(
        &self,
        command: &CommandSpec,
        cancel: &CancellationToken,
    ) -> Result<ProcessOutput, ProcessFailure> {
        if self.armed.load(Ordering::SeqCst)
            && command.arguments.iter().any(|arg| arg == "hash-object")
            && command.arguments.iter().any(|arg| arg == "--no-filters")
        {
            let scope = command
                .arguments
                .windows(2)
                .filter(|pair| pair[0] == "-C")
                .last()
                .expect("fixed original recovery scope");
            let actual = fs::canonicalize(&scope[1]).unwrap();
            assert!(actual.starts_with(&self.outside));
            assert!(
                !actual.starts_with(&self.checkout),
                "Undo holding must be outside source checkout"
            );
            assert!(
                actual
                    .file_name()
                    .unwrap()
                    .as_encoded_bytes()
                    .starts_with(b".cagents-undo-")
            );
            if self.armed.swap(false, Ordering::SeqCst) {
                let original = fs::File::open(&scope[1]).unwrap();
                assert_eq!(original.metadata().unwrap().mode() & 0o7777, 0o700);
                *self.observed_default_acl.lock().unwrap() =
                    get_acl(&original, c"system.posix_acl_default");
                if self.inject {
                    let anchor = PathBuf::from(format!("/proc/self/fd/{}", original.as_raw_fd()));
                    fs::write(
                        anchor.join("retain-unknown"),
                        b"fixture foreign cleanup entry",
                    )
                    .unwrap();
                }
                *self.holding.lock().unwrap() = Some(original);
                self.calls.fetch_add(1, Ordering::SeqCst);
            }
        }
        SystemProcessExecutor.execute(command, cancel)
    }
}
struct Cwd(PathBuf);
impl Cwd {
    fn change(path: &Path) -> Self {
        let old = std::env::current_dir().unwrap();
        std::env::set_current_dir(path).unwrap();
        Self(old)
    }
}
impl Drop for Cwd {
    fn drop(&mut self) {
        std::env::set_current_dir(&self.0).unwrap();
    }
}
fn foreign_checkout(root: &Path) -> PathBuf {
    let foreign = root.parent().unwrap().join("foreign-checkout");
    fs::create_dir(&foreign).unwrap();
    git(&foreign, &["init", "-q"]);
    fs::write(foreign.join("tracked.txt"), b"foreign user work\n").unwrap();
    git(&foreign, &["add", "tracked.txt"]);
    git(&foreign, &["commit", "-qm", "foreign base"]);
    foreign
}

#[test]
fn recoverable_discard_preserves_original_access_despite_outside_default_acl() {
    const NAME: &str =
        "access::recoverable_discard_preserves_original_access_despite_outside_default_acl";
    if isolated(NAME, false) {
        return;
    }
    let root = setup();
    let foreign = foreign_checkout(&root);
    let outside = fs::File::open(root.parent().unwrap()).unwrap();
    let default = access_acl(23_456);
    set_acl(&outside, c"system.posix_acl_default", &default);
    for original_has_acl in [false, true] {
        fs::write(root.join("tracked.txt"), b"base\n").unwrap();
        git(&root, &["add", "tracked.txt"]);
        git(&root, &["commit", "--allow-empty", "-qm", "access base"]);
        fs::write(root.join("tracked.txt"), b"uncommitted exact\n").unwrap();
        let original = fs::File::open(root.join("tracked.txt")).unwrap();
        // The previous round may have restored its original ACL; explicitly
        // remove it before the absence variant and recapture the actual policy.
        if get_acl(&original, c"system.posix_acl_access").is_some() {
            assert_eq!(
                unsafe {
                    libc::fremovexattr(original.as_raw_fd(), c"system.posix_acl_access".as_ptr())
                },
                0
            );
        }
        try_distinct_original_group(&original);
        original
            .set_permissions(fs::Permissions::from_mode(0o640))
            .unwrap();
        if original_has_acl {
            set_acl(&original, c"system.posix_acl_access", &access_acl(12_345));
        }
        let policy = get_acl(&original, c"system.posix_acl_access");
        assert_eq!(policy.is_some(), original_has_acl);
        let metadata = original.metadata().unwrap();
        let executor = Arc::new(ObserveHolding::new(&root, false));
        let source = RepositoryService::new("git", executor.clone())
            .with_worktree_authority(WorktreeAuthority::capture(&root).unwrap())
            .unwrap();
        // Missing retained metadata authority must not be recaptured from a
        // currently valid checkout pathname (the omitted original1331 parent).
        assert!(
            RepositoryService::system("git")
                .discard_recoverable(
                    &request(&root, &["tracked.txt"], &[]),
                    &CancellationToken::new(),
                    &[root.clone(), foreign.clone()]
                )
                .is_err()
        );
        assert_eq!(
            fs::read(root.join("tracked.txt")).unwrap(),
            b"uncommitted exact\n"
        );
        // A foreign original authority cannot authorize this discard request.
        assert!(
            service(&foreign)
                .discard_recoverable(
                    &request(&root, &["tracked.txt"], &[]),
                    &CancellationToken::new(),
                    &[root.clone(), foreign.clone()]
                )
                .is_err()
        );
        assert_eq!(
            fs::read(root.join("tracked.txt")).unwrap(),
            b"uncommitted exact\n"
        );
        let unchanged = fs::File::open(root.join("tracked.txt")).unwrap();
        let current = unchanged.metadata().unwrap();
        assert_eq!(
            (
                current.dev(),
                current.ino(),
                current.uid(),
                current.gid(),
                current.mode() & 0o7777
            ),
            (
                metadata.dev(),
                metadata.ino(),
                metadata.uid(),
                metadata.gid(),
                metadata.mode() & 0o7777
            )
        );
        assert_eq!(get_acl(&unchanged, c"system.posix_acl_access"), policy);
        let discarded = source
            .discard_recoverable(
                &request(&root, &["tracked.txt"], &[]),
                &CancellationToken::new(),
                &[root.clone(), foreign.clone()],
            )
            .unwrap();
        applied(&discarded.outcome);
        assert_eq!(fs::read(root.join("tracked.txt")).unwrap(), b"base\n");
        executor.armed.store(true, Ordering::SeqCst);
        // Neither a changed process cwd nor a different caller service may
        // redirect the receipt's original Git metadata/object/destination scope.
        let _cwd = Cwd::change(&foreign);
        let outcome = service(&foreign)
            .undo_discard(&discarded.recovery, &CancellationToken::new())
            .unwrap();
        applied(&outcome);
        assert_eq!(executor.calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            *executor.observed_default_acl.lock().unwrap(),
            Some(default.clone())
        );
        let restored = fs::File::open(root.join("tracked.txt")).unwrap();
        assert_eq!(get_acl(&restored, c"system.posix_acl_access"), policy);
        let actual = restored.metadata().unwrap();
        assert_eq!(
            (actual.uid(), actual.gid(), actual.mode() & 0o7777),
            (metadata.uid(), metadata.gid(), metadata.mode() & 0o7777)
        );
        assert_eq!(
            fs::read(root.join("tracked.txt")).unwrap(),
            b"uncommitted exact\n"
        );
        assert_eq!(
            fs::read(foreign.join("tracked.txt")).unwrap(),
            b"foreign user work\n"
        );
        assert!(!discarded.recovery.live_recovery.needs_resolution());
    }
}

#[test]
fn recoverable_discard_restores_blob_under_restrictive_umask() {
    const NAME: &str = "access::recoverable_discard_restores_blob_under_restrictive_umask";
    if isolated(NAME, false) {
        return;
    }
    let root = setup();
    fs::write(root.join("tracked.txt"), b"base\n").unwrap();
    git(&root, &["add", "tracked.txt"]);
    git(&root, &["commit", "-qm", "umask base"]);
    fs::write(root.join("tracked.txt"), b"uncommitted exact\n").unwrap();
    fs::set_permissions(root.join("tracked.txt"), fs::Permissions::from_mode(0o751)).unwrap();
    let source = service(&root);
    let cancel = CancellationToken::new();
    let discarded = source
        .discard_recoverable(
            &request(&root, &["tracked.txt"], &[]),
            &cancel,
            &[root.clone()],
        )
        .unwrap();
    applied(&discarded.outcome);
    assert_eq!(fs::read(root.join("tracked.txt")).unwrap(), b"base\n");
    struct Umask(libc::mode_t);
    impl Drop for Umask {
        fn drop(&mut self) {
            unsafe {
                libc::umask(self.0);
            }
        }
    }
    // Only the finite single-test child changes process policy, after original
    // source/index/context capture and the real discard completed.
    let _restore = Umask(unsafe { libc::umask(0o777) });
    applied(&source.undo_discard(&discarded.recovery, &cancel).unwrap());
    assert_eq!(
        fs::read(root.join("tracked.txt")).unwrap(),
        b"uncommitted exact\n"
    );
    assert_eq!(
        fs::metadata(root.join("tracked.txt")).unwrap().mode() & 0o7777,
        0o751
    );
    assert!(!discarded.recovery.live_recovery.needs_resolution());
}

#[test]
fn recoverable_discard_executable_runs_while_original_cleanup_is_retained() {
    const NAME: &str =
        "access::recoverable_discard_executable_runs_while_original_cleanup_is_retained";
    if isolated(NAME, false) {
        return;
    }
    let root = setup();
    fs::write(root.join("tracked.sh"), b"#!/bin/sh\nprintf base").unwrap();
    fs::set_permissions(root.join("tracked.sh"), fs::Permissions::from_mode(0o700)).unwrap();
    git(&root, &["add", "tracked.sh"]);
    git(&root, &["commit", "-qm", "executable base"]);
    fs::write(root.join("tracked.sh"), b"#!/bin/sh\nprintf restored-exec").unwrap();
    let executor = Arc::new(ObserveHolding::new(&root, true));
    let source = RepositoryService::new("git", executor.clone())
        .with_worktree_authority(WorktreeAuthority::capture(&root).unwrap())
        .unwrap();
    let cancel = CancellationToken::new();
    let discarded = source
        .discard_recoverable(
            &request(&root, &["tracked.sh"], &[]),
            &cancel,
            &[root.clone()],
        )
        .unwrap();
    applied(&discarded.outcome);
    executor.armed.store(true, Ordering::SeqCst);
    let outcome = source.undo_discard(&discarded.recovery, &cancel).unwrap();
    let MutationOutcome::PartiallyApplied { paths, failures } = outcome else {
        panic!("restored plus unresolved cleanup: {outcome:?}")
    };
    assert_eq!(paths, vec![path("tracked.sh")]);
    assert_eq!(failures.len(), 1);
    assert!(
        failures[0]
            .to_string()
            .contains("cleanup or durability is unconfirmed")
    );
    assert_eq!(executor.calls.load(Ordering::SeqCst), 1);
    let authority = discarded
        .recovery
        .live_recovery
        .authority()
        .expect("exact original cleanup custody");
    assert!(discarded.recovery.live_recovery.needs_resolution());
    assert!(!authority.is_resolved());
    let output = SystemProcessExecutor
        .execute(
            &CommandSpec::new(root.join("tracked.sh")).timeout(Duration::from_secs(5)),
            &cancel,
        )
        .unwrap();
    assert!(
        output.success,
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, b"restored-exec");
    assert!(
        !authority.is_resolved(),
        "execution precedes cleanup resolution"
    );
    assert!(
        source
            .resolve_discard_recovery(&discarded.recovery, &cancel)
            .is_err(),
        "foreign entry still refuses cleanup"
    );
    let original = executor.holding.lock().unwrap();
    let original = original.as_ref().unwrap();
    assert_eq!(
        unsafe { libc::unlinkat(original.as_raw_fd(), c"retain-unknown".as_ptr(), 0) },
        0
    );
    source
        .resolve_discard_recovery(&discarded.recovery, &cancel)
        .unwrap();
    assert!(!discarded.recovery.live_recovery.needs_resolution());
    assert!(authority.is_resolved());
    assert_eq!(
        fs::read(root.join("tracked.sh")).unwrap(),
        b"#!/bin/sh\nprintf restored-exec"
    );
}
