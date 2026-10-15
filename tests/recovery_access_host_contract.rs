#![cfg(all(feature = "host", target_os = "linux"))]

//! Real Git service contracts on the isolated HOST boundary. These exercise
//! the same public recovery API as the UI integration suite without UI imports.
//! Test-only owned placement topology is not production issuer qualification.
use cagents::{filesystem_recovery_context::FilesystemRecoveryContext, repository::{
    CancellationToken, CommandSpec, DiscardRequest, MutationOutcome, PathSelection,
    ProcessExecutor, ProcessFailure, ProcessOutput, RepoPath, RepositoryService,
    SystemProcessExecutor, WorktreeAuthority,
}};
use std::{fs, os::{fd::AsRawFd, unix::fs::{MetadataExt, PermissionsExt}}, path::PathBuf, sync::Arc, time::Duration};

#[path = "support/recovery_context.rs"]
mod recovery_context;

struct IsolatedExecutor;
fn isolate(mut command: CommandSpec) -> CommandSpec {
    // Remove inherited redirection while preserving explicit retained scopes:
    // CommandSpec applies its controlled environment after removed_environment.
    for (key, _) in std::env::vars_os() {
        if key.as_encoded_bytes().starts_with(b"GIT_") { command = command.env_remove(key); }
    }
    command.arguments.splice(0..0, [
        "-c", "core.hooksPath=/dev/null", "-c", "core.fsmonitor=false",
        "-c", "init.templateDir=", "-c", "commit.gpgsign=false",
    ].map(Into::into));
    command.env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null").env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "Recovery Contract").env("GIT_AUTHOR_EMAIL", "contract@example.invalid")
        .env("GIT_COMMITTER_NAME", "Recovery Contract").env("GIT_COMMITTER_EMAIL", "contract@example.invalid")
        .env("GIT_AUTHOR_DATE", "1700000000 +0000").env("GIT_COMMITTER_DATE", "1700000000 +0000")
        .timeout(Duration::from_secs(30))
}
impl ProcessExecutor for IsolatedExecutor {
    fn execute(&self, command: &CommandSpec, cancellation: &CancellationToken) -> Result<ProcessOutput, ProcessFailure> {
        SystemProcessExecutor.execute(&isolate(command.clone()), cancellation)
    }
}
struct Fixture { directory: tempfile::TempDir }
impl Fixture {
    fn new() -> Option<Self> {
        let fixture = Self { directory: tempfile::Builder::new().prefix("cagents-host-recovery-")
            .permissions(fs::Permissions::from_mode(0o700)).tempdir().unwrap() };
        fixture.git_in_root(&["init", "-b", "main", "work"]);
        Some(fixture)
    }
    fn work(&self) -> PathBuf { self.directory.path().join("work") }
    fn git_in_root(&self, args: &[&str]) {
        let output = IsolatedExecutor.execute(&CommandSpec::new("git").cwd(self.directory.path()).args(args.iter().copied()), &CancellationToken::new()).unwrap();
        assert!(output.success, "Git {args:?}: {output:?}");
    }
    fn git_in_work(&self, args: &[&str]) {
        let output = IsolatedExecutor.execute(&CommandSpec::new("git").cwd(self.work()).args(args.iter().copied()), &CancellationToken::new()).unwrap();
        assert!(output.success, "Git {args:?}: {output:?}");
    }
    fn commit(&self, message: &str) { self.git_in_work(&["commit", "-m", message]); }
    fn write(&self, path: &str, bytes: &[u8]) { fs::write(self.work().join(path), bytes).unwrap(); }
    fn service(&self) -> RepositoryService<IsolatedExecutor> { RepositoryService::new("git", Arc::new(IsolatedExecutor)) }
    fn recovery_context(&self) -> FilesystemRecoveryContext { recovery_context::context(&self.work()) }
    fn recovery_service(&self) -> RepositoryService<IsolatedExecutor> {
        self.service().with_worktree_authority(WorktreeAuthority::capture(&self.work()).unwrap()).unwrap()
            .with_recovery_context(self.recovery_context())
    }
}
fn path(path: &str) -> RepoPath { RepoPath::parse(path).unwrap() }

/// Root UID does not prove CAP_CHOWN. Count a different-GID positive only
/// after the real syscall and same-FD observation both establish it.
fn try_distinct_original_group(file: &fs::File) {
    let initial = file.metadata().unwrap().gid();
    let count = unsafe { libc::getgroups(0, std::ptr::null_mut()) };
    assert!((0..=65_536).contains(&count));
    let mut groups = vec![0; count as usize];
    assert_eq!(unsafe { libc::getgroups(count, groups.as_mut_ptr()) }, count);
    let candidate = groups.into_iter().find(|gid| *gid != initial).or_else(||
        (unsafe { libc::geteuid() } == 0).then_some(initial.wrapping_add(12_345)));
    if let Some(gid) = candidate {
        if unsafe { libc::fchown(file.as_raw_fd(), u32::MAX, gid) } == 0 {
            assert_eq!(file.metadata().unwrap().gid(), gid);
            eprintln!("GID_DIFFERENCE_EXERCISED {initial}->{gid}");
            return;
        }
    }
    eprintln!("NO_RESTORABLE_SECONDARY_GID: original-GID equality still asserted; no cross-GID positive credited");
}

#[cfg(target_os = "linux")]
#[test]
fn recoverable_discard_preserves_original_access_despite_outside_default_acl() {
    const NAME: &str = "recoverable_discard_preserves_original_access_despite_outside_default_acl";
    if recovery_context::run_isolated_case(NAME) {
        return;
    }
    use std::os::{
        fd::AsRawFd,
        unix::fs::{MetadataExt, PermissionsExt},
    };
    fn acl(uid: u32) -> Vec<u8> {
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
    fn set(file: &fs::File, name: &std::ffi::CStr, bytes: &[u8]) {
        let result = unsafe {
            libc::fsetxattr(
                file.as_raw_fd(),
                name.as_ptr(),
                bytes.as_ptr().cast(),
                bytes.len(),
                0,
            )
        };
        assert_eq!(result, 0, "{}", std::io::Error::last_os_error());
    }
    fn get(file: &fs::File) -> Option<Vec<u8>> {
        let size = unsafe {
            libc::fgetxattr(
                file.as_raw_fd(),
                c"system.posix_acl_access".as_ptr(),
                std::ptr::null_mut(),
                0,
            )
        };
        if size < 0 {
            assert_eq!(
                std::io::Error::last_os_error().raw_os_error(),
                Some(libc::ENODATA)
            );
            return None;
        }
        assert!(size <= 64 * 1024);
        let mut bytes = vec![0u8; size as usize];
        assert_eq!(
            unsafe {
                libc::fgetxattr(
                    file.as_raw_fd(),
                    c"system.posix_acl_access".as_ptr(),
                    bytes.as_mut_ptr().cast(),
                    bytes.len(),
                )
            },
            size
        );
        Some(bytes)
    }
    for original_has_acl in [false, true] {
        let fixture = Fixture::new().expect("this isolated recovery contract requires Git");
        fixture.write(
            "tracked.txt",
            b"base
",
        );
        fixture.git_in_work(&["add", "--", "tracked.txt"]);
        fixture.commit("base");
        fixture.write(
            "tracked.txt",
            b"uncommitted secret
",
        );
        let original = fs::File::open(fixture.work().join("tracked.txt")).unwrap();
        try_distinct_original_group(&original);
        original
            .set_permissions(fs::Permissions::from_mode(0o640))
            .unwrap();
        if original_has_acl {
            set(&original, c"system.posix_acl_access", &acl(12_345));
        }
        let policy = get(&original);
        assert_eq!(policy.is_some(), original_has_acl);
        let metadata = original.metadata().unwrap();
        let context = fixture.recovery_context();
        let outside = fs::File::open(context.placement().directory().path()).unwrap();
        set(&outside, c"system.posix_acl_default", &acl(23_456));
        let service = fixture
            .service()
            .with_worktree_authority(
                cagents::repository::WorktreeAuthority::capture(&fixture.work()).unwrap(),
            )
            .unwrap()
            .with_recovery_context(context);
        let cancel = CancellationToken::new();
        let discarded = service
            .discard_recoverable(
                &DiscardRequest {
                    worktree: fixture.work(),
                    tracked: vec![PathSelection::new(path("tracked.txt"))],
                    untracked: vec![],
                    confirmed: true,
                },
                &cancel,
            )
            .unwrap();
        assert!(matches!(discarded.outcome, MutationOutcome::Applied { .. }));
        assert_eq!(
            fs::read(fixture.work().join("tracked.txt")).unwrap(),
            b"base
"
        );
        let outcome = service.undo_discard(&discarded.recovery, &cancel).unwrap();
        assert!(
            matches!(outcome, MutationOutcome::Applied { .. }),
            "{outcome:?}"
        );
        let restored = fs::File::open(fixture.work().join("tracked.txt")).unwrap();
        assert_eq!(
            get(&restored),
            policy,
            "outside named readers must not replace original ACL absence/presence"
        );
        let actual = restored.metadata().unwrap();
        assert_eq!(
            (actual.uid(), actual.gid(), actual.mode() & 0o7777),
            (metadata.uid(), metadata.gid(), metadata.mode() & 0o7777)
        );
        assert_eq!(
            fs::read(fixture.work().join("tracked.txt")).unwrap(),
            b"uncommitted secret
"
        );
        assert!(!discarded.recovery.live_recovery.needs_resolution());
    }
}

#[cfg(target_os = "linux")]
#[test]
fn recoverable_discard_restores_blob_under_restrictive_umask() {
    const NAME: &str = "recoverable_discard_restores_blob_under_restrictive_umask";
    if recovery_context::run_isolated_case(NAME) {
        return;
    }
    use std::os::unix::fs::PermissionsExt;
    let fixture = Fixture::new().expect("the isolated recovery contract requires Git");
    fixture.write("tracked.txt", b"base\n");
    fixture.git_in_work(&["add", "--", "tracked.txt"]);
    fixture.commit("base");
    fixture.write("tracked.txt", b"uncommitted exact\n");
    fs::set_permissions(
        fixture.work().join("tracked.txt"),
        fs::Permissions::from_mode(0o751),
    )
    .unwrap();
    let cancellation = CancellationToken::new();
    let service = fixture.recovery_service();
    let discarded = service
        .discard_recoverable(
            &DiscardRequest {
                worktree: fixture.work(),
                tracked: vec![PathSelection::new(path("tracked.txt"))],
                untracked: vec![],
                confirmed: true,
            },
            &cancellation,
        )
        .unwrap();
    assert!(matches!(discarded.outcome, MutationOutcome::Applied { .. }));
    assert_eq!(
        fs::read(fixture.work().join("tracked.txt")).unwrap(),
        b"base\n"
    );
    // Only this exact single-test child changes umask, AFTER real Git/context
    // setup. Restore it before fixture teardown even when the assertion fails.
    struct Umask(libc::mode_t);
    impl Drop for Umask {
        fn drop(&mut self) {
            unsafe {
                libc::umask(self.0);
            }
        }
    }
    let _restore = Umask(unsafe { libc::umask(0o777) });
    let outcome = service
        .undo_discard(&discarded.recovery, &cancellation)
        .unwrap();
    assert!(
        matches!(outcome, MutationOutcome::Applied { .. }),
        "{outcome:?}"
    );
    assert_eq!(
        fs::read(fixture.work().join("tracked.txt")).unwrap(),
        b"uncommitted exact\n"
    );
    assert_eq!(
        fs::metadata(fixture.work().join("tracked.txt"))
            .unwrap()
            .permissions()
            .mode()
            & 0o7777,
        0o751
    );
    assert!(!discarded.recovery.live_recovery.needs_resolution());
}

