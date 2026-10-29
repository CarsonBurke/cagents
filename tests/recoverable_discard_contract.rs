#![cfg(all(target_os = "linux", feature = "host"))]
//! Actual current service contracts. Each child owns its private runtime,
//! desktop Trash and checkout; no developer Trash or global environment changes.
use cagents::{
    process::{CancellationToken, CommandSpec, ProcessExecutor, SystemProcessExecutor},
    repository::{
        DiscardRequest, MutationOutcome, PathSelection, RepoPath, RepositoryService,
        WorktreeAuthority,
    },
};
use std::{
    fs,
    io::Read,
    os::unix::fs::{MetadataExt, PermissionsExt, symlink},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::Arc,
    time::{Duration, Instant},
};
fn isolated(name: &str, inside_trash: bool) -> bool {
    if std::env::var("CAGENTS_DISCARD_CASE").as_deref() == Ok(name) {
        return false;
    }
    let owner = tempfile::Builder::new()
        .permissions(fs::Permissions::from_mode(0o700))
        .tempdir()
        .unwrap();
    let runtime = owner.path().join("runtime");
    let data = owner.path().join("data");
    let root = owner.path().join("checkout");
    for dir in [&runtime, &data, &root] {
        fs::create_dir(dir).unwrap();
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700)).unwrap();
    }
    let stdout = owner.path().join("stdout");
    let stderr = owner.path().join("stderr");
    let mut command = Command::new(std::env::current_exe().unwrap());
    for (name, _) in std::env::vars_os() {
        if name.as_encoded_bytes().starts_with(b"GIT_") {
            command.env_remove(name);
        }
    }
    command
        .args([name, "--exact", "--nocapture", "--test-threads=1"])
        .env("CAGENTS_DISCARD_CASE", name)
        .env("CAGENTS_DISCARD_ROOT", &root)
        .env("XDG_RUNTIME_DIR", &runtime)
        .env(
            "XDG_DATA_HOME",
            if inside_trash {
                root.join("inside-data")
            } else {
                data
            },
        )
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "Fixture")
        .env("GIT_AUTHOR_EMAIL", "fixture@example.invalid")
        .env("GIT_COMMITTER_NAME", "Fixture")
        .env("GIT_COMMITTER_EMAIL", "fixture@example.invalid")
        .stdin(Stdio::null())
        .stdout(Stdio::from(fs::File::create(&stdout).unwrap()))
        .stderr(Stdio::from(fs::File::create(&stderr).unwrap()));
    struct Child(std::process::Child);
    impl Drop for Child {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let mut child = Child(command.spawn().unwrap());
    let deadline = Instant::now() + Duration::from_secs(90);
    let status = loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            break status;
        }
        assert!(Instant::now() < deadline, "discard child timed out");
        std::thread::sleep(Duration::from_millis(10));
    };
    fn read(path: &Path) -> String {
        let mut bytes = Vec::new();
        fs::File::open(path)
            .unwrap()
            .take(1024 * 1024 + 1)
            .read_to_end(&mut bytes)
            .unwrap();
        assert!(bytes.len() <= 1024 * 1024);
        String::from_utf8_lossy(&bytes).into_owned()
    }
    let out = read(&stdout);
    let err = read(&stderr);
    assert!(status.success(), "{name}: {status}\n{out}\n{err}");
    assert!(
        out.contains(&format!("test {name} ... ok")),
        "missing exact positive test: {out}\n{err}"
    );
    assert!(
        out.contains("test result: ok. 1 passed; 0 failed;"),
        "zero/incorrect selector: {out}"
    );
    true
}
fn root() -> PathBuf {
    PathBuf::from(std::env::var_os("CAGENTS_DISCARD_ROOT").unwrap())
}
fn path(name: &str) -> RepoPath {
    RepoPath::parse(name).unwrap()
}
fn git(root: &Path, args: &[&str]) {
    let spec = CommandSpec::new("git")
        .cwd(root)
        .args(args.iter().copied())
        .timeout(Duration::from_secs(10));
    let out = SystemProcessExecutor
        .execute(&spec, &CancellationToken::new())
        .unwrap();
    out.require_success(&spec.program).unwrap();
}
fn setup() -> PathBuf {
    let root = root();
    git(&root, &["init", "-q"]);
    fs::write(root.join("modified"), b"base modified\n").unwrap();
    fs::write(root.join("deleted"), b"base deleted\n").unwrap();
    fs::write(root.join("executable"), b"#!/bin/sh\nexit 0\n").unwrap();
    fs::set_permissions(root.join("executable"), fs::Permissions::from_mode(0o755)).unwrap();
    git(&root, &["add", "--all"]);
    git(&root, &["commit", "-qm", "base"]);
    fs::write(root.join("modified"), b"precious modified\n").unwrap();
    fs::remove_file(root.join("deleted")).unwrap();
    fs::write(root.join("executable"), b"#!/bin/sh\nexit 7\n").unwrap();
    root
}
fn request(root: &Path, tracked: &[&str], untracked: &[&str]) -> DiscardRequest {
    DiscardRequest {
        worktree: root.to_owned(),
        tracked: tracked
            .iter()
            .map(|s| PathSelection::new(path(s)))
            .collect(),
        untracked: untracked.iter().map(|s| path(s)).collect(),
        confirmed: true,
    }
}
fn service(
    root: &Path,
) -> RepositoryService<cagents::repository::WorktreeExecutor<SystemProcessExecutor>> {
    RepositoryService::system("git")
        .with_worktree_authority(WorktreeAuthority::capture(root).unwrap())
        .unwrap()
}
fn applied(out: &MutationOutcome) {
    assert!(matches!(out, MutationOutcome::Applied { .. }), "{out:?}");
}
fn trash() -> PathBuf {
    PathBuf::from(std::env::var_os("XDG_DATA_HOME").unwrap()).join("Trash")
}
#[test]
fn local_discard_native_mixed_roundtrip_and_empty_parent_pruning() {
    const NAME: &str = "local_discard_native_mixed_roundtrip_and_empty_parent_pruning";
    if isolated(NAME, false) {
        return;
    }
    let root = setup();
    fs::create_dir(root.join("new-directory")).unwrap();
    fs::write(root.join("new-directory/new file"), b"untracked original").unwrap();
    fs::set_permissions(
        root.join("new-directory/new file"),
        fs::Permissions::from_mode(0o640),
    )
    .unwrap();
    symlink("../modified", root.join("new-link")).unwrap();
    let original = fs::symlink_metadata(root.join("new-directory/new file")).unwrap();
    let source = service(&root);
    let result = source
        .discard_recoverable(
            &request(
                &root,
                &["modified", "deleted", "executable"],
                &["new-directory/new file", "new-link"],
            ),
            &CancellationToken::new(),
            &[root.clone()],
        )
        .unwrap();
    applied(&result.outcome);
    assert!(!root.join("new-directory").exists());
    assert!(!root.join("new-link").exists());
    let files = fs::read_dir(trash().join("files"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect::<Vec<_>>();
    assert_eq!(files.len(), 2);
    let actual = files
        .iter()
        .map(|p| fs::symlink_metadata(p).unwrap())
        .find(|m| m.is_file())
        .unwrap();
    assert_eq!(
        (original.dev(), original.ino()),
        (actual.dev(), actual.ino())
    );
    let bodies = fs::read_dir(trash().join("info"))
        .unwrap()
        .map(|e| fs::read_to_string(e.unwrap().path()).unwrap())
        .collect::<Vec<_>>();
    assert!(
        bodies
            .iter()
            .any(|s| s.contains("new-directory/new%20file\nDeletionDate=")),
        "{bodies:?}"
    );
    let undo = source
        .undo_discard(&result.recovery, &CancellationToken::new())
        .unwrap();
    applied(&undo);
    assert_eq!(
        fs::read(root.join("modified")).unwrap(),
        b"precious modified\n"
    );
    assert!(!root.join("deleted").exists());
    assert_eq!(
        fs::read(root.join("new-directory/new file")).unwrap(),
        b"untracked original"
    );
    assert_eq!(
        fs::metadata(root.join("new-directory/new file"))
            .unwrap()
            .mode()
            & 0o7777,
        0o640
    );
    assert_eq!(
        fs::read_link(root.join("new-link")).unwrap(),
        Path::new("../modified")
    );
    let exit = Command::new(root.join("executable")).status().unwrap();
    assert_eq!(exit.code(), Some(7));
    assert_eq!(fs::read_dir(trash().join("files")).unwrap().count(), 0);
    assert_eq!(fs::read_dir(trash().join("info")).unwrap().count(), 0);
}
#[test]
fn local_discard_128_native_and_tracked_roundtrip_keeps_newer_entries_per_path() {
    const NAME: &str =
        "local_discard_128_native_and_tracked_roundtrip_keeps_newer_entries_per_path";
    if isolated(NAME, false) {
        return;
    }
    let root = setup();
    fs::create_dir(root.join("many")).unwrap();
    let names = (0..128)
        .map(|index| format!("many/entry-{index}"))
        .collect::<Vec<_>>();
    let originals = names
        .iter()
        .enumerate()
        .map(|(index, name)| {
            let file = root.join(name);
            if index % 7 == 0 {
                symlink("../modified", &file).unwrap();
            } else {
                fs::write(&file, format!("original native {index}\n")).unwrap();
                fs::set_permissions(&file, fs::Permissions::from_mode(0o640)).unwrap();
            }
            fs::symlink_metadata(&file).unwrap()
        })
        .collect::<Vec<_>>();
    let source = service(&root);
    let requested = names.iter().map(String::as_str).collect::<Vec<_>>();
    let result = source
        .discard_recoverable(
            &request(&root, &["modified", "deleted", "executable"], &requested),
            &CancellationToken::new(),
            &[root.clone()],
        )
        .unwrap();
    applied(&result.outcome);
    assert!(!root.join("many").exists());
    assert_eq!(fs::read_dir(trash().join("files")).unwrap().count(), 128);
    assert_eq!(fs::read_dir(trash().join("info")).unwrap().count(), 128);
    // A newer destination refuses independently while the other127 original
    // Trash inodes and all tracked preimages are restored by the real service.
    fs::create_dir(root.join("many")).unwrap();
    fs::write(root.join(&names[1]), b"newer user work\n").unwrap();
    let undo = source
        .undo_discard(&result.recovery, &CancellationToken::new())
        .unwrap();
    assert!(
        matches!(undo, MutationOutcome::PartiallyApplied { .. }),
        "{undo:?}"
    );
    assert_eq!(result.recovery.remaining_paths(), 1);
    for (index, (name, original)) in names.iter().zip(&originals).enumerate() {
        let file = root.join(name);
        if index == 1 {
            assert_eq!(fs::read(&file).unwrap(), b"newer user work\n");
            continue;
        }
        let actual = fs::symlink_metadata(&file).unwrap();
        assert_eq!(
            (actual.dev(), actual.ino()),
            (original.dev(), original.ino())
        );
        assert_eq!(actual.mode() & 0o7777, original.mode() & 0o7777);
        if index % 7 == 0 {
            assert_eq!(fs::read_link(file).unwrap(), Path::new("../modified"));
        } else {
            assert_eq!(
                fs::read(file).unwrap(),
                format!("original native {index}\n").as_bytes()
            );
        }
    }
    assert_eq!(
        fs::read(root.join("modified")).unwrap(),
        b"precious modified\n"
    );
    assert!(!root.join("deleted").exists());
    assert_eq!(
        Command::new(root.join("executable"))
            .status()
            .unwrap()
            .code(),
        Some(7)
    );
    assert_eq!(fs::read_dir(trash().join("files")).unwrap().count(), 1);
    assert_eq!(fs::read_dir(trash().join("info")).unwrap().count(), 1);
    fs::remove_file(root.join(&names[1])).unwrap();
    applied(
        &source
            .undo_discard(&result.recovery, &CancellationToken::new())
            .unwrap(),
    );
    assert_eq!(result.recovery.remaining_paths(), 0);
    let restored = fs::metadata(root.join(&names[1])).unwrap();
    assert_eq!(
        (restored.dev(), restored.ino()),
        (originals[1].dev(), originals[1].ino())
    );
    assert_eq!(
        fs::read(root.join(&names[1])).unwrap(),
        b"original native 1\n"
    );
    assert_eq!(fs::read_dir(trash().join("files")).unwrap().count(), 0);
    assert_eq!(fs::read_dir(trash().join("info")).unwrap().count(), 0);
    assert!(!root.join(".git/index.lock").exists());
}

#[test]
fn local_discard_undo_preserves_newer_file_per_path() {
    const NAME: &str = "local_discard_undo_preserves_newer_file_per_path";
    if isolated(NAME, false) {
        return;
    }
    let root = setup();
    fs::write(root.join("new"), b"original untracked").unwrap();
    let service = service(&root);
    let result = service
        .discard_recoverable(
            &request(&root, &["modified"], &["new"]),
            &CancellationToken::new(),
            &[root.clone()],
        )
        .unwrap();
    applied(&result.outcome);
    fs::write(root.join("new"), b"newer work").unwrap();
    let undo = service
        .undo_discard(&result.recovery, &CancellationToken::new())
        .unwrap();
    assert!(
        matches!(undo, MutationOutcome::PartiallyApplied { .. }),
        "{undo:?}"
    );
    assert_eq!(fs::read(root.join("new")).unwrap(), b"newer work");
    assert_eq!(
        fs::read(root.join("modified")).unwrap(),
        b"precious modified\n"
    );
    assert_eq!(result.recovery.remaining_paths(), 1);
    assert_eq!(fs::read_dir(trash().join("files")).unwrap().count(), 1);
}
#[test]
fn local_discard_receipt_rejects_replaced_source_and_git_metadata() {
    const NAME: &str = "local_discard_receipt_rejects_replaced_source_and_git_metadata";
    if isolated(NAME, false) {
        return;
    }
    let root = setup();
    let service = service(&root);
    let result = service
        .discard_recoverable(
            &request(&root, &["modified"], &[]),
            &CancellationToken::new(),
            &[root.clone()],
        )
        .unwrap();
    applied(&result.outcome);
    fs::rename(root.join(".git"), root.join("old-git")).unwrap();
    fs::create_dir(root.join(".git")).unwrap();
    assert!(
        service
            .undo_discard(&result.recovery, &CancellationToken::new())
            .is_err()
    );
    assert_eq!(fs::read(root.join("modified")).unwrap(), b"base modified\n");
    fs::remove_dir(root.join(".git")).unwrap();
    fs::rename(root.join("old-git"), root.join(".git")).unwrap();
    let old = root.with_extension("original");
    fs::rename(&root, &old).unwrap();
    fs::create_dir(&root).unwrap();
    fs::write(root.join("modified"), b"foreign root").unwrap();
    assert!(
        service
            .undo_discard(&result.recovery, &CancellationToken::new())
            .is_err()
    );
    assert_eq!(fs::read(root.join("modified")).unwrap(), b"foreign root");
}
#[test]
fn local_discard_refuses_modified_public_trash_entry() {
    const NAME: &str = "local_discard_refuses_modified_public_trash_entry";
    if isolated(NAME, false) {
        return;
    }
    let root = setup();
    fs::write(root.join("new"), b"original untracked").unwrap();
    let service = service(&root);
    let result = service
        .discard_recoverable(
            &request(&root, &[], &["new"]),
            &CancellationToken::new(),
            &[root.clone()],
        )
        .unwrap();
    applied(&result.outcome);
    let moved = fs::read_dir(trash().join("files"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    fs::write(&moved, b"foreign changed data").unwrap();
    let undo = service
        .undo_discard(&result.recovery, &CancellationToken::new())
        .unwrap();
    assert!(
        matches!(undo, MutationOutcome::NotApplied { .. }),
        "{undo:?}"
    );
    assert!(!root.join("new").exists());
    assert_eq!(fs::read(moved).unwrap(), b"foreign changed data");
}
#[test]
fn local_discard_refuses_native_trash_inside_checkout_before_any_effect() {
    const NAME: &str = "local_discard_refuses_native_trash_inside_checkout_before_any_effect";
    if isolated(NAME, true) {
        return;
    }
    let root = setup();
    fs::write(root.join("new"), b"original untracked").unwrap();
    let service = service(&root);
    let result = service
        .discard_recoverable(
            &request(&root, &["modified"], &["new"]),
            &CancellationToken::new(),
            &[root.clone()],
        )
        .unwrap();
    assert!(
        matches!(result.outcome, MutationOutcome::NotApplied { .. }),
        "{:?}",
        result.outcome
    );
    assert_eq!(fs::read(root.join("new")).unwrap(), b"original untracked");
    assert_eq!(
        fs::read(root.join("modified")).unwrap(),
        b"precious modified\n"
    );
    assert!(!root.join("inside-data").exists());
}

#[test]
fn local_discard_undo_preserves_replaced_desktop_trash_directory() {
    const NAME: &str = "local_discard_undo_preserves_replaced_desktop_trash_directory";
    if isolated(NAME, false) {
        return;
    }
    let root = setup();
    fs::write(root.join("new"), b"original untracked").unwrap();
    let service = service(&root);
    let result = service
        .discard_recoverable(
            &request(&root, &[], &["new"]),
            &CancellationToken::new(),
            &[root.clone()],
        )
        .unwrap();
    applied(&result.outcome);
    let files = trash().join("files");
    let old = trash().join("retired-files");
    fs::rename(&files, &old).unwrap();
    fs::create_dir(&files).unwrap();
    fs::set_permissions(&files, fs::Permissions::from_mode(0o700)).unwrap();
    fs::write(files.join("foreign"), b"foreign native entry").unwrap();
    let undo = service
        .undo_discard(&result.recovery, &CancellationToken::new())
        .unwrap();
    assert!(
        matches!(undo, MutationOutcome::NotApplied { .. }),
        "{undo:?}"
    );
    assert!(!root.join("new").exists());
    assert_eq!(
        fs::read(files.join("foreign")).unwrap(),
        b"foreign native entry"
    );
    assert_eq!(fs::read_dir(&old).unwrap().count(), 1);
    fs::remove_file(files.join("foreign")).unwrap();
    fs::remove_dir(&files).unwrap();
    fs::rename(&old, &files).unwrap();
    applied(
        &service
            .undo_discard(&result.recovery, &CancellationToken::new())
            .unwrap(),
    );
    assert_eq!(fs::read(root.join("new")).unwrap(), b"original untracked");
}

#[test]
fn local_tracked_undo_refuses_parent_moved_during_actual_blob_materialization() {
    const NAME: &str = "local_tracked_undo_refuses_parent_moved_during_actual_blob_materialization";
    if isolated(NAME, false) {
        return;
    }
    use cagents::process::{ProcessFailure, ProcessOutput};
    use std::sync::atomic::{AtomicBool, Ordering};
    struct MoveParent {
        source: PathBuf,
        moved: PathBuf,
        armed: AtomicBool,
    }
    impl ProcessExecutor for MoveParent {
        fn execute(
            &self,
            command: &CommandSpec,
            cancel: &CancellationToken,
        ) -> Result<ProcessOutput, ProcessFailure> {
            if command.arguments.iter().any(|arg| arg == "cat-file")
                && self.armed.swap(false, Ordering::SeqCst)
            {
                fs::rename(self.source.join("nested"), &self.moved).unwrap();
                fs::create_dir(self.source.join("nested")).unwrap();
                fs::write(
                    self.source.join("nested/file"),
                    b"foreign replacement parent\n",
                )
                .unwrap();
            }
            SystemProcessExecutor.execute(command, cancel)
        }
    }
    let root = setup();
    fs::create_dir(root.join("nested")).unwrap();
    fs::write(root.join("nested/file"), b"index base\n").unwrap();
    git(&root, &["add", "nested/file"]);
    git(&root, &["commit", "-qm", "nested base"]);
    fs::write(root.join("nested/file"), b"precious nested original\n").unwrap();
    let moved = root.with_extension("moved-parent");
    let executor = Arc::new(MoveParent {
        source: root.clone(),
        moved: moved.clone(),
        armed: AtomicBool::new(false),
    });
    let service = RepositoryService::new("git", executor.clone())
        .with_worktree_authority(WorktreeAuthority::capture(&root).unwrap())
        .unwrap();
    let result = service
        .discard_recoverable(
            &request(&root, &["nested/file"], &[]),
            &CancellationToken::new(),
            &[root.clone()],
        )
        .unwrap();
    applied(&result.outcome);
    executor.armed.store(true, Ordering::SeqCst);
    let undo = service
        .undo_discard(&result.recovery, &CancellationToken::new())
        .unwrap();
    assert!(
        matches!(undo, MutationOutcome::NotApplied { .. }),
        "{undo:?}"
    );
    assert_eq!(
        fs::read(root.join("nested/file")).unwrap(),
        b"foreign replacement parent\n"
    );
    assert_eq!(fs::read(moved.join("file")).unwrap(), b"index base\n");
    assert_eq!(result.recovery.remaining_paths(), 1);
    assert!(result.recovery.live_recovery.needs_resolution());
    let choices = result.recovery.live_recovery.recoverable_originals();
    assert_eq!(
        choices.len(),
        1,
        "only the authenticated complete blob is offered"
    );
    let choice = &choices[0];
    assert_eq!(choice.original_path, root.join("nested/file"));
    assert_eq!(choice.bytes, b"precious nested original\n".len() as u64);
    let payload = fs::metadata(&choice.retained_path).unwrap();
    let payload_identity = (payload.dev(), payload.ino());
    assert_eq!(
        fs::read(&choice.retained_path).unwrap(),
        b"precious nested original\n"
    );
    assert!(
        service
            .resolve_discard_recovery(&result.recovery, &CancellationToken::new())
            .is_err(),
        "Resolve must not imply restoration or release precious custody"
    );
    assert!(
        service
            .recover_original_file(
                &result.recovery,
                uuid::Uuid::new_v4(),
                &CancellationToken::new()
            )
            .is_err()
    );
    assert!(
        service
            .recover_original_file(&result.recovery, choice.nonce, &CancellationToken::new())
            .is_err(),
        "the foreign replacement parent is not adopted"
    );
    let foreign = root.join("nested");
    let foreign_identity = fs::metadata(foreign.join("file")).unwrap();
    let saved_foreign = root.join("foreign-preserved");
    fs::rename(&foreign, &saved_foreign).unwrap();
    fs::rename(&moved, &foreign).unwrap();
    let baseline = fs::metadata(foreign.join("file")).unwrap();
    assert!(
        service
            .recover_original_file(&result.recovery, choice.nonce, &CancellationToken::new())
            .is_err(),
        "even the exact original baseline must never be overwritten"
    );
    assert_eq!(fs::read(foreign.join("file")).unwrap(), b"index base\n");
    fs::rename(foreign.join("file"), foreign.join("preserved-baseline")).unwrap();
    // A genuine current-source fence, not a convenient recapture after manual
    // namespace repair. The precious inode remains private on refusal.
    fs::rename(root.join(".git"), root.join("preserved-git")).unwrap();
    fs::create_dir(root.join(".git")).unwrap();
    assert!(
        service
            .recover_original_file(&result.recovery, choice.nonce, &CancellationToken::new())
            .is_err()
    );
    assert!(!foreign.join("file").exists());
    fs::remove_dir(root.join(".git")).unwrap();
    fs::rename(root.join("preserved-git"), root.join(".git")).unwrap();
    let resolved = service
        .recover_original_file(&result.recovery, choice.nonce, &CancellationToken::new())
        .unwrap();
    assert_eq!(resolved.remaining_paths, 0);
    let recovered = fs::metadata(foreign.join("file")).unwrap();
    assert_eq!(
        (recovered.dev(), recovered.ino()),
        payload_identity,
        "Recover publishes the actual precious inode, never a replacement copy"
    );
    assert_eq!(
        fs::read(foreign.join("file")).unwrap(),
        b"precious nested original\n"
    );
    assert_eq!(recovered.mode() & 0o7777, payload.mode() & 0o7777);
    let saved_baseline = fs::metadata(foreign.join("preserved-baseline")).unwrap();
    assert_eq!(
        (saved_baseline.dev(), saved_baseline.ino()),
        (baseline.dev(), baseline.ino())
    );
    assert_eq!(
        fs::read(foreign.join("preserved-baseline")).unwrap(),
        b"index base\n"
    );
    let preserved = fs::metadata(saved_foreign.join("file")).unwrap();
    assert_eq!(
        (preserved.dev(), preserved.ino()),
        (foreign_identity.dev(), foreign_identity.ino())
    );
    assert_eq!(
        fs::read(saved_foreign.join("file")).unwrap(),
        b"foreign replacement parent\n"
    );
    assert!(!choice.retained_path.exists());
    assert!(!result.recovery.live_recovery.is_pending());
    assert!(!result.recovery.live_recovery.needs_resolution());
    assert!(
        result
            .recovery
            .live_recovery
            .recoverable_originals()
            .is_empty()
    );
    assert!(
        service
            .recover_original_file(&result.recovery, choice.nonce, &CancellationToken::new())
            .is_err(),
        "the stale rendered action cannot publish twice"
    );
    assert_eq!(result.recovery.remaining_paths(), 0);
}

#[test]
fn local_linked_discard_undo_keeps_original_private_index_and_common_objects() {
    const NAME: &str = "local_linked_discard_undo_keeps_original_private_index_and_common_objects";
    if isolated(NAME, false) {
        return;
    }
    let primary = setup();
    let linked = primary.with_file_name("linked-checkout");
    git(
        &primary,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "linked-fixture",
            linked.to_str().unwrap(),
        ],
    );
    fs::write(linked.join("modified"), b"linked precious work\n").unwrap();
    fs::write(
        linked.join("linked new file"),
        b"linked untracked original\n",
    )
    .unwrap();
    let native_original = fs::metadata(linked.join("linked new file")).unwrap();
    let original = service(&linked);
    let discarded = original
        .discard_recoverable(
            &request(&linked, &["modified"], &["linked new file"]),
            &CancellationToken::new(),
            &[primary.clone(), linked.clone()],
        )
        .unwrap();
    assert!(
        matches!(discarded.outcome, MutationOutcome::Applied { .. }),
        "{:?}",
        discarded.outcome
    );
    assert_eq!(
        fs::read(linked.join("modified")).unwrap(),
        b"base modified\n"
    );
    assert!(!linked.join("linked new file").exists());
    assert_eq!(
        fs::read(primary.join("modified")).unwrap(),
        b"precious modified\n"
    );
    // The current caller may point at a sibling: only the receipt's original
    // private index/common-object authority can select the Undo destination.
    let undone = service(&primary)
        .undo_discard(&discarded.recovery, &CancellationToken::new())
        .unwrap();
    assert!(
        matches!(undone, MutationOutcome::Applied { .. }),
        "{undone:?}"
    );
    assert_eq!(discarded.recovery.remaining_paths(), 0);
    assert_eq!(
        fs::read(linked.join("modified")).unwrap(),
        b"linked precious work\n"
    );
    assert_eq!(
        fs::read(linked.join("linked new file")).unwrap(),
        b"linked untracked original\n"
    );
    let restored = fs::metadata(linked.join("linked new file")).unwrap();
    assert_eq!(
        (restored.dev(), restored.ino()),
        (native_original.dev(), native_original.ino())
    );
    assert_eq!(
        fs::read(primary.join("modified")).unwrap(),
        b"precious modified\n"
    );
    let gitfile = fs::read_to_string(linked.join(".git")).unwrap();
    let private = PathBuf::from(gitfile.trim().strip_prefix("gitdir: ").unwrap());
    assert!(!private.join("index.lock").exists());
    assert!(!primary.join(".git/index.lock").exists());
    assert_eq!(fs::read_dir(trash().join("files")).unwrap().count(), 0);
    assert_eq!(fs::read_dir(trash().join("info")).unwrap().count(), 0);
}

#[test]
fn local_tracked_undo_acquired_previous_never_offers_single_blob_recovery() {
    const NAME: &str = "local_tracked_undo_acquired_previous_never_offers_single_blob_recovery";
    if isolated(NAME, false) {
        return;
    }
    use cagents::process::{ProcessFailure, ProcessOutput};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    struct MoveAfterAcquisition {
        root: PathBuf,
        parked: PathBuf,
        armed: AtomicBool,
        hashes: AtomicUsize,
    }
    impl ProcessExecutor for MoveAfterAcquisition {
        fn execute(
            &self,
            command: &CommandSpec,
            cancel: &CancellationToken,
        ) -> Result<ProcessOutput, ProcessFailure> {
            if self.armed.load(Ordering::SeqCst)
                && command.arguments.iter().any(|arg| arg == "hash-object")
                && self.hashes.fetch_add(1, Ordering::SeqCst) == 1
            {
                // First hash authenticated the precious blob; the second is
                // the actual acquired baseline at Holding/previous.
                fs::rename(self.root.join("nested"), &self.parked).unwrap();
                fs::create_dir(self.root.join("nested")).unwrap();
                fs::write(self.root.join("nested/file"), b"foreign replacement\n").unwrap();
            }
            SystemProcessExecutor.execute(command, cancel)
        }
    }
    let root = setup();
    fs::create_dir(root.join("nested")).unwrap();
    fs::write(root.join("nested/file"), b"index baseline\n").unwrap();
    git(&root, &["add", "nested/file"]);
    git(&root, &["commit", "-qm", "nested base"]);
    fs::write(root.join("nested/file"), b"precious acquired original\n").unwrap();
    let executor = Arc::new(MoveAfterAcquisition {
        root: root.clone(),
        parked: root.with_extension("parked"),
        armed: AtomicBool::new(false),
        hashes: AtomicUsize::new(0),
    });
    let service = RepositoryService::new("git", executor.clone())
        .with_worktree_authority(WorktreeAuthority::capture(&root).unwrap())
        .unwrap();
    let discarded = service
        .discard_recoverable(
            &request(&root, &["nested/file"], &[]),
            &CancellationToken::new(),
            &[root.clone()],
        )
        .unwrap();
    applied(&discarded.outcome);
    executor.armed.store(true, Ordering::SeqCst);
    let undo = service
        .undo_discard(&discarded.recovery, &CancellationToken::new())
        .unwrap();
    assert!(
        matches!(undo, MutationOutcome::OutcomeUnknown { .. }),
        "{undo:?}"
    );
    assert_eq!(executor.hashes.load(Ordering::SeqCst), 2);
    assert!(discarded.recovery.live_recovery.needs_resolution());
    assert!(
        discarded
            .recovery
            .live_recovery
            .recoverable_originals()
            .is_empty(),
        "complete precious bytes do not authorize stranding an acquired baseline"
    );
    let holders: Vec<_> = fs::read_dir(root.parent().unwrap())
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| {
            p.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with(".cagents-undo-")
        })
        .collect();
    assert_eq!(holders.len(), 1);
    assert_eq!(
        fs::read(holders[0].join("restored-blob")).unwrap(),
        b"precious acquired original\n"
    );
    assert_eq!(
        fs::read(holders[0].join("previous")).unwrap(),
        b"index baseline\n"
    );
    assert_eq!(
        fs::read(root.join("nested/file")).unwrap(),
        b"foreign replacement\n"
    );
    assert!(
        service
            .resolve_discard_recovery(&discarded.recovery, &CancellationToken::new())
            .is_err()
    );
    assert_eq!(discarded.recovery.remaining_paths(), 1);
}

#[path = "recoverable_discard_contract/access.rs"]
mod access;
