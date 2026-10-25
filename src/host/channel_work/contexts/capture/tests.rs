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
