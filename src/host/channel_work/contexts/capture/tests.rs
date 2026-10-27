use super::*;

fn git(directory: &Path, arguments: &[&str]) {
    let spec = CommandSpec::new("git")
        .args([
            "-c",
            "user.name=Marvin Burke",
            "-c",
            "user.email=marvinburke22@gmail.com",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "core.hooksPath=/dev/null",
        ])
        .arg("-C")
        .arg(directory)
        .args(arguments.iter().copied())
        .timeout(Duration::from_secs(10))
        .output_limits(1024 * 1024, 8192);
    let output = SystemProcessExecutor
        .execute(&spec, &CancellationToken::new())
        .unwrap();
    assert!(
        output.success,
        "fixture Git failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}
fn project(directory: &Path, name: &str) -> PathBuf {
    git(directory, &["init", "--template=", "-b", "main", name]);
    let root = directory.join(name).canonicalize().unwrap();
    git(&root, &["commit", "--allow-empty", "-m", "fixture"]);
    root
}
fn linked(root: &Path, target: &Path) {
    git(
        root,
        &["worktree", "add", "-b", "linked", target.to_str().unwrap()],
    );
}
fn token() -> CancellationToken {
    CancellationToken::new()
}

#[test]
fn explicit_main_and_outside_linked_worktrees_share_common_but_not_checkout_identity() {
    if crate::repository::isolated_git_test(concat!(
        module_path!(),
        "::explicit_main_and_outside_linked_worktrees_share_common_but_not_checkout_identity"
    )) {
        return;
    }
    let directory = tempfile::tempdir().unwrap();
    let root = project(directory.path(), "project");
    let outside = directory.path().join("outside");
    linked(&root, &outside);
    let main = CapturedCheckout::capture_user(&root, &root, &token()).unwrap();
    let captured = CapturedCheckout::capture_user(&root.join(".git"), &outside, &token()).unwrap();
    assert_eq!(captured.facts().repository, root.join(".git"));
    assert_eq!(captured.facts().path, outside);
    assert_eq!(captured.facts().branch.as_deref(), Some("linked"));
    assert_eq!(captured.facts().dirty, None);
    assert_eq!(main.identity().common, captured.identity().common);
    assert_ne!(main.identity(), captured.identity());
    let interval = captured.identity();
    let before_head = captured.facts().head;
    fs::write(outside.join("new-file"), "unpublished\n").unwrap();
    assert_eq!(captured.revalidate(&token()).unwrap().dirty, None);
    git(&outside, &["add", "new-file"]);
    git(&outside, &["commit", "-m", "local commit"]);
    let committed = captured.revalidate(&token()).unwrap();
    assert_eq!(committed.dirty, None);
    assert_ne!(committed.head, before_head);
    assert_eq!(
        captured.identity(),
        interval,
        "same symbolic branch interval survives commits and dirty observations"
    );
    git(&outside, &["switch", "--detach"]);
    assert!(captured.revalidate(&token()).is_err());
    let detached = CapturedCheckout::capture_user(&root, &outside, &token()).unwrap();
    assert_eq!(detached.facts().branch, None);
    assert_eq!(detached.facts().head, committed.head);
    assert_eq!(detached.facts().dirty, None);
}

#[test]
fn selected_root_rejects_foreign_common_nested_directory_and_final_symlink() {
    if crate::repository::isolated_git_test(concat!(
        module_path!(),
        "::selected_root_rejects_foreign_common_nested_directory_and_final_symlink"
    )) {
        return;
    }
    let directory = tempfile::tempdir().unwrap();
    let root = project(directory.path(), "project");
    let foreign = project(directory.path(), "foreign");
    assert!(CapturedCheckout::capture_user(&root, &foreign, &token()).is_err());
    fs::create_dir(root.join("nested")).unwrap();
    assert!(CapturedCheckout::capture_user(&root, &root.join("nested"), &token()).is_err());
    let alias = directory.path().join("alias");
    std::os::unix::fs::symlink(&root, &alias).unwrap();
    assert!(CapturedCheckout::capture_user(&root, &alias, &token()).is_err());
    assert!(CapturedCheckout::capture_user(&root, &root.join(".git"), &token()).is_err());
}

#[test]
fn renamed_worktree_and_replacement_path_cannot_retarget_the_retained_capture() {
    if crate::repository::isolated_git_test(concat!(
        module_path!(),
        "::renamed_worktree_and_replacement_path_cannot_retarget_the_retained_capture"
    )) {
        return;
    }
    let directory = tempfile::tempdir().unwrap();
    let root = project(directory.path(), "project");
    let captured = CapturedCheckout::capture_user(&root, &root, &token()).unwrap();
    let identity = captured.identity();
    fs::rename(&root, directory.path().join("moved")).unwrap();
    fs::create_dir(&root).unwrap();
    assert!(captured.revalidate(&token()).is_err());
    assert_eq!(captured.identity(), identity);
    assert_eq!(
        DirectoryIdentity::of(&captured.tree.root.metadata().unwrap()),
        identity.worktree
    );
    assert_ne!(
        DirectoryIdentity::of(&fs::metadata(&root).unwrap()),
        identity.worktree
    );
    let moved = directory.path().join("moved");
    let fresh = CapturedCheckout::capture_user(&moved, &moved, &token()).unwrap();
    assert_eq!(fresh.identity().worktree, identity.worktree);
    assert_ne!(
        fresh.identity(),
        identity,
        "fresh renamed paths cannot reuse an old context identity"
    );
    assert_eq!(captured.facts().path, root);
    assert_eq!(fresh.facts().path, moved);
}

#[test]
fn locator_admin_common_and_backlink_replacements_refuse_original_selection() {
    if crate::repository::isolated_git_test(concat!(
        module_path!(),
        "::locator_admin_common_and_backlink_replacements_refuse_original_selection"
    )) {
        return;
    }
    for case in 0..5 {
        let directory = tempfile::tempdir().unwrap();
        let root = project(directory.path(), "project");
        let outside = directory.path().join("outside");
        linked(&root, &outside);
        let captured = CapturedCheckout::capture_user(&root, &outside, &token()).unwrap();
        match case {
            0 => {
                let name = outside.join(".git");
                let bytes = fs::read(&name).unwrap();
                fs::rename(&name, outside.join("old-locator")).unwrap();
                fs::write(&name, bytes).unwrap();
            }
            1 => {
                let admin = captured.admin.canonical.clone();
                fs::rename(&admin, admin.with_extension("old")).unwrap();
                fs::create_dir(&admin).unwrap();
            }
            2 => {
                let foreign = project(directory.path(), "foreign");
                fs::write(
                    captured.admin.canonical.join("commondir"),
                    format!("{}\n", foreign.join(".git").display()),
                )
                .unwrap();
            }
            3 => {
                fs::write(
                    captured.admin.canonical.join("gitdir"),
                    format!("{}\n", root.join(".git").display()),
                )
                .unwrap();
            }
            4 => {
                fs::rename(root.join(".git"), root.join("old-git")).unwrap();
                fs::create_dir(root.join(".git")).unwrap();
            }
            _ => unreachable!(),
        }
        assert!(
            captured.revalidate(&token()).is_err(),
            "binding case {case}"
        );
    }
}

#[test]
fn observed_head_switch_and_return_to_same_branch_have_distinct_retained_intervals() {
    if crate::repository::isolated_git_test(concat!(
        module_path!(),
        "::observed_head_switch_and_return_to_same_branch_have_distinct_retained_intervals"
    )) {
        return;
    }
    let directory = tempfile::tempdir().unwrap();
    let root = project(directory.path(), "project");
    let original = CapturedCheckout::capture_user(&root, &root, &token()).unwrap();
    git(&root, &["switch", "-c", "other"]);
    assert!(original.revalidate(&token()).is_err());
    let other = CapturedCheckout::capture_user(&root, &root, &token()).unwrap();
    assert_eq!(other.facts().branch.as_deref(), Some("other"));
    git(&root, &["switch", "main"]);
    assert!(
        original.revalidate(&token()).is_err(),
        "A→B→A does not revive an earlier selection"
    );
    assert!(other.revalidate(&token()).is_err());
    let returned = CapturedCheckout::capture_user(&root, &root, &token()).unwrap();
    assert_eq!(original.facts().branch, returned.facts().branch);
    assert_ne!(original.identity(), returned.identity());
    assert_ne!(
        original.head.stamp.identity, returned.head.stamp.identity,
        "original HEAD descriptor keeps its unlinked inode from reuse"
    );
}

#[test]
fn metadata_must_be_bounded_regular_nofollow_before_any_git_metadata_read() {
    if crate::repository::isolated_git_test(concat!(
        module_path!(),
        "::metadata_must_be_bounded_regular_nofollow_before_any_git_metadata_read"
    )) {
        return;
    }
    for case in 0..3 {
        let directory = tempfile::tempdir().unwrap();
        let root = project(directory.path(), "project");
        let head = root.join(".git/HEAD");
        fs::remove_file(&head).unwrap();
        match case {
            0 => {
                fs::write(root.join("other-head"), "ref: refs/heads/main\n").unwrap();
                std::os::unix::fs::symlink(root.join("other-head"), &head).unwrap();
            }
            1 => {
                let name = std::ffi::CString::new(head.as_os_str().as_bytes()).unwrap();
                assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
            }
            2 => fs::write(&head, vec![b'x'; METADATA_BYTES as usize + 1]).unwrap(),
            _ => unreachable!(),
        }
        assert!(
            CapturedCheckout::capture_user(&root, &root, &token()).is_err(),
            "metadata case {case}"
        );
    }
}

#[test]
fn failed_metadata_and_cancelled_read_never_report_a_clean_checkout() {
    if crate::repository::isolated_git_test(concat!(
        module_path!(),
        "::failed_metadata_and_cancelled_read_never_report_a_clean_checkout"
    )) {
        return;
    }
    let directory = tempfile::tempdir().unwrap();
    let root = project(directory.path(), "project");
    fs::write(root.join("dirty"), "untracked\n").unwrap();
    let captured = CapturedCheckout::capture_user(&root, &root, &token()).unwrap();
    assert_eq!(captured.facts().dirty, None);
    let cancelled = token();
    cancelled.cancel();
    assert!(captured.revalidate(&cancelled).is_err());
    // The selected branch now names a missing object: it must not be called
    // unborn merely because commit resolution failed.
    fs::write(
        root.join(".git/refs/heads/main"),
        format!("{}\n", "f".repeat(40)),
    )
    .unwrap();
    assert!(captured.revalidate(&token()).is_err());
    assert_eq!(captured.facts().dirty, None);
    assert!(CapturedCheckout::capture_user(&root, &root, &token()).is_err());
}

#[test]
fn explicit_unborn_branch_has_no_oid_without_becoming_unknown_head() {
    if crate::repository::isolated_git_test(concat!(
        module_path!(),
        "::explicit_unborn_branch_has_no_oid_without_becoming_unknown_head"
    )) {
        return;
    }
    let directory = tempfile::tempdir().unwrap();
    git(
        directory.path(),
        &["init", "--template=", "-b", "main", "unborn"],
    );
    let root = directory.path().join("unborn").canonicalize().unwrap();
    let captured = CapturedCheckout::capture_user(&root, &root, &token()).unwrap();
    assert_eq!(captured.facts().branch.as_deref(), Some("main"));
    assert_eq!(captured.facts().head, None);
    assert_eq!(captured.facts().dirty, None);
}

#[test]
fn configured_clean_and_process_helpers_are_never_invoked_and_dirty_is_unknown() {
    if crate::repository::isolated_git_test(concat!(
        module_path!(),
        "::configured_clean_and_process_helpers_are_never_invoked_and_dirty_is_unknown"
    )) {
        return;
    }
    for driver in ["clean", "process"] {
        let directory = tempfile::tempdir().unwrap();
        let root = project(directory.path(), "project");
        fs::write(
            root.join(".gitattributes"),
            "payload filter=capture-sentinel\n",
        )
        .unwrap();
        fs::write(root.join("payload"), "aa\n").unwrap();
        git(&root, &["add", ".gitattributes", "payload"]);
        git(&root, &["commit", "-m", "plain pre-filter baseline"]);
        let sentinel = directory.path().join("helper-invoked");
        let helper = directory.path().join("sentinel-helper");
        fs::write(
            &helper,
            if driver == "clean" {
                "#!/bin/sh\nprintf invoked > \"$1\"\ncat\n"
            } else {
                "#!/bin/sh\nprintf invoked > \"$1\"\nexit 1\n"
            },
        )
        .unwrap();
        let quote = |path: &Path| format!("'{}'", path.to_str().unwrap().replace('\'', "'\\''"));
        let command = format!("sh {} {}", quote(&helper), quote(&sentinel));
        git(
            &root,
            &[
                "config",
                &format!("filter.capture-sentinel.{driver}"),
                &command,
            ],
        );
        git(
            &root,
            &["config", "filter.capture-sentinel.required", "true"],
        );
        // Same-length content with a deliberate stat difference would force
        // conversion in the old status-based implementation. No status/add/
        // commit is invoked by this fixture after configuring the helper.
        let payload = root.join("payload");
        let before = fs::metadata(&payload).unwrap();
        fs::write(&payload, "bb\n").unwrap();
        File::open(&payload)
            .unwrap()
            .set_modified(std::time::UNIX_EPOCH + Duration::from_secs(before.mtime() as u64 + 2))
            .unwrap();
        let captured = CapturedCheckout::capture_user(&root, &root, &token()).unwrap();
        assert_eq!(
            captured.facts().dirty,
            None,
            "unmeasured conversion semantics are not clean/dirty"
        );
        assert_eq!(captured.facts().branch.as_deref(), Some("main"));
        assert!(captured.facts().head.is_some());
        assert!(
            !sentinel.exists(),
            "configured {driver} helper executed during capture"
        );
        fs::write(&payload, "cc\n").unwrap();
        assert_eq!(captured.revalidate(&token()).unwrap().dirty, None);
        assert!(
            !sentinel.exists(),
            "configured {driver} helper executed during revalidation"
        );
    }
}

fn held_launch(path: &Path) -> (crate::agent_execution::DirectoryIdentity, Arc<File>) {
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC)
        .open(path)
        .unwrap();
    let identity =
        crate::agent_execution::DirectoryIdentity::from_metadata(&file.metadata().unwrap());
    (identity, Arc::new(file))
}

#[test]
fn actor_capture_uses_original_held_checkout_after_move_and_name_replacement() {
    if crate::repository::isolated_git_test(concat!(
        module_path!(),
        "::actor_capture_uses_original_held_checkout_after_move_and_name_replacement"
    )) {
        return;
    }
    let directory = tempfile::tempdir().unwrap();
    let root = project(directory.path(), "project");
    let original = directory.path().join("outside");
    linked(&root, &original);
    let (expected, pin) = held_launch(&original);
    let moved = directory.path().join("moved");
    git(
        &root,
        &[
            "worktree",
            "move",
            original.to_str().unwrap(),
            moved.to_str().unwrap(),
        ],
    );
    fs::create_dir(&original).unwrap();
    fs::write(original.join("replacement"), "leave untouched\n").unwrap();
    let captured =
        CapturedCheckout::capture_actor(&root, &expected, Arc::clone(&pin), &token()).unwrap();
    assert_eq!(captured.directory_identity(), expected);
    assert_eq!(captured.facts().path, moved);
    assert_eq!(captured.facts().branch.as_deref(), Some("linked"));
    assert_eq!(captured.facts().dirty, None);
    assert_eq!(
        captured.identity().worktree,
        DirectoryIdentity::of(&pin.metadata().unwrap())
    );
    assert!(Arc::ptr_eq(
        captured.tree.original_descriptor.as_ref().unwrap(),
        &pin
    ));
    let user = CapturedCheckout::capture_user(&root, &moved, &token()).unwrap();
    assert_eq!(user.directory_identity(), expected);
    assert_eq!(captured.identity(), user.identity());
    assert_eq!(captured.revalidate(&token()).unwrap(), user.facts());
    assert_eq!(
        fs::read(original.join("replacement")).unwrap(),
        b"leave untouched\n"
    );
    // A later unregistered move invalidates the captured display/metadata
    // bindings. It never causes a new lookup of the old launch recipe path.
    let moved_again = directory.path().join("moved-again");
    fs::rename(&moved, &moved_again).unwrap();
    assert!(captured.revalidate(&token()).is_err());
    assert!(
        CapturedCheckout::capture_actor(&root, &expected, Arc::clone(&pin), &token()).is_err(),
        "a stale worktree backlink is not silently repaired"
    );
    assert_eq!(
        DirectoryIdentity::of(&pin.metadata().unwrap()),
        captured.identity().worktree
    );
    assert_eq!(
        fs::read(original.join("replacement")).unwrap(),
        b"leave untouched\n"
    );
}

#[test]
fn actor_capture_preserves_traverse_only_launch_pin_and_unknown_dirty() {
    use std::os::unix::fs::PermissionsExt;
    if crate::repository::isolated_git_test(concat!(
        module_path!(),
        "::actor_capture_preserves_traverse_only_launch_pin_and_unknown_dirty"
    )) {
        return;
    }
    struct Restore(PathBuf);
    impl Drop for Restore {
        fn drop(&mut self) {
            let _ = fs::set_permissions(&self.0, fs::Permissions::from_mode(0o700));
        }
    }
    let directory = tempfile::tempdir().unwrap();
    let root = project(directory.path(), "project");
    let _restore = Restore(root.clone());
    fs::set_permissions(&root, fs::Permissions::from_mode(0o111)).unwrap();
    let (expected, pin) = held_launch(&root);
    let captured =
        CapturedCheckout::capture_actor(&root, &expected, Arc::clone(&pin), &token()).unwrap();
    let original_flags = unsafe { libc::fcntl(pin.as_raw_fd(), libc::F_GETFL) };
    let captured_flags = unsafe { libc::fcntl(captured.tree.root.descriptor(), libc::F_GETFL) };
    assert!(original_flags >= 0 && original_flags & libc::O_PATH != 0);
    assert!(captured_flags >= 0 && captured_flags & libc::O_PATH != 0);
    assert_eq!(captured.facts().branch.as_deref(), Some("main"));
    assert!(captured.facts().head.is_some());
    assert_eq!(captured.revalidate(&token()).unwrap().dirty, None);
    let cancelled = token();
    cancelled.cancel();
    assert!(CapturedCheckout::capture_actor(&root, &expected, pin, &cancelled).is_err());
}

#[test]
fn actor_capture_refuses_wrong_identity_non_directory_subdir_foreign_and_stale_home() {
    if crate::repository::isolated_git_test(concat!(
        module_path!(),
        "::actor_capture_refuses_wrong_identity_non_directory_subdir_foreign_and_stale_home"
    )) {
        return;
    }
    let directory = tempfile::tempdir().unwrap();
    let root = project(directory.path(), "project");
    let (expected, pin) = held_launch(&root);
    let wrong = crate::agent_execution::DirectoryIdentity {
        device: expected.device,
        inode: expected.inode.wrapping_add(1),
    };
    assert!(CapturedCheckout::capture_actor(&root, &wrong, Arc::clone(&pin), &token()).is_err());
    let regular_path = directory.path().join("regular");
    fs::write(&regular_path, "not a directory").unwrap();
    let regular = Arc::new(File::open(&regular_path).unwrap());
    let regular_identity =
        crate::agent_execution::DirectoryIdentity::from_metadata(&regular.metadata().unwrap());
    assert!(CapturedCheckout::capture_actor(&root, &regular_identity, regular, &token()).is_err());
    let subdir = root.join("nested");
    fs::create_dir(&subdir).unwrap();
    let (subdir_identity, subdir_pin) = held_launch(&subdir);
    assert!(
        CapturedCheckout::capture_actor(&root, &subdir_identity, Arc::clone(&subdir_pin), &token())
            .is_err(),
        "subdirectory original launches are explicitly unsupported in this slice"
    );
    fs::remove_dir(&subdir).unwrap();
    assert!(
        CapturedCheckout::capture_actor(&root, &subdir_identity, subdir_pin, &token()).is_err()
    );
    let foreign = project(directory.path(), "foreign");
    let (foreign_identity, foreign_pin) = held_launch(&foreign);
    assert!(
        CapturedCheckout::capture_actor(&root, &foreign_identity, foreign_pin, &token()).is_err()
    );
    // The actual launch object survives, but a stale Home repository name
    // cannot be rebound implicitly through that object's new display path.
    fs::rename(&root, directory.path().join("project-moved")).unwrap();
    fs::create_dir(&root).unwrap();
    fs::write(root.join("replacement"), "preserve\n").unwrap();
    assert!(CapturedCheckout::capture_actor(&root, &expected, pin, &token()).is_err());
    assert_eq!(fs::read(root.join("replacement")).unwrap(), b"preserve\n");
}
