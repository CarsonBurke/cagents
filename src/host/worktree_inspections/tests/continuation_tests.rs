//! Actual public requests and Git effects on an isolated original repository.
use super::*;
use crate::worktree_control::BranchDeleteFailureKind;

/// Removal checks the real durable registry too; bare HostState is insufficient.
fn host_with_registry(root: &Path) -> SharedHost {
    let host = super::host();
    let registry =
        crate::host::model::Registry::at(&root.join("registry/workspace.sqlite3")).unwrap();
    host.lock().unwrap().install_model(registry);
    host
}

fn setup(root: &Path, unmerged: bool) -> (PathBuf, PathBuf) {
    let repository = root.join("repository");
    std::fs::create_dir(&repository).unwrap();
    git(&repository, &["init", "--quiet", "--initial-branch=main"]);
    git(
        &repository,
        &["commit", "--quiet", "--allow-empty", "-m", "base"],
    );
    let target = root.join("target");
    git(
        &repository,
        &[
            "worktree",
            "add",
            "--quiet",
            "-b",
            "topic",
            target.to_str().unwrap(),
        ],
    );
    if unmerged {
        git(
            &target,
            &["commit", "--quiet", "--allow-empty", "-m", "topic only"],
        );
    }
    (repository, target)
}
fn inspected(
    host: &SharedHost,
    repository: &Path,
    target: &Path,
) -> (crate::repository::WorktreeSafetySnapshot, uuid::Uuid) {
    match request(
        host,
        1,
        Operation::InspectForSessionStop {
            repository: repository.into(),
            path: target.into(),
        },
    )
    .unwrap()
    {
        Reply::Inspected {
            snapshot,
            stop_authority: Some(token),
            ..
        } => (serde_json::from_str(&snapshot).unwrap(), token),
        _ => panic!("actual original inspection"),
    }
}
fn stopped(
    host: &SharedHost,
    repository: &Path,
    target: &Path,
    snapshot: &crate::repository::WorktreeSafetySnapshot,
    token: uuid::Uuid,
) -> uuid::Uuid {
    match request(
        host,
        1,
        Operation::StopSessions {
            repository: repository.into(),
            path: target.into(),
            consent: SessionStopConsent { sessions: vec![] },
            expected_fingerprint: snapshot.fingerprint.clone(),
            stop_authority: token,
        },
    )
    .unwrap()
    {
        Reply::SessionsStopped {
            continuation: Some(token),
            ..
        } => token,
        _ => panic!("zero-session Stop must transfer the original inspection"),
    }
}
fn removal(
    repository: &Path,
    target: &Path,
    snapshot: &crate::repository::WorktreeSafetySnapshot,
    token: Option<uuid::Uuid>,
) -> Operation {
    Operation::Remove {
        repository: repository.into(),
        target: target.to_str().unwrap().into(),
        force: false,
        delete_branch: false,
        expected_fingerprint: Some(snapshot.fingerprint.clone()),
        resolved_path: None,
        continuation: token,
    }
}
fn removed(
    host: &SharedHost,
    repository: &Path,
    target: &Path,
    snapshot: &crate::repository::WorktreeSafetySnapshot,
    token: uuid::Uuid,
) -> uuid::Uuid {
    match request(host, 1, removal(repository, target, snapshot, Some(token))).unwrap() {
        Reply::Removed {
            continuation: Some(token),
            ..
        } => token,
        _ => panic!("removed branch must retain its original repository scope"),
    }
}
fn branch(
    repository: &Path,
    snapshot: &crate::repository::WorktreeSafetySnapshot,
    force: bool,
    token: uuid::Uuid,
) -> Operation {
    Operation::DeleteBranch {
        repository: repository.into(),
        branch: snapshot.branch.clone().unwrap(),
        force,
        expected_oid: snapshot.head_oid.clone(),
        continuation: Some(token),
    }
}

#[test]
fn original_zero_session_inspection_survives_remove_and_exact_branch_delete() {
    const TEST: &str = "host::worktree_inspections::tests::continuation_tests::original_zero_session_inspection_survives_remove_and_exact_branch_delete";
    let Some(root) = super::private_runtime::enter(TEST) else {
        return;
    };
    let (repository, target) = setup(&root, false);
    let host = host_with_registry(&root);
    let (snapshot, token) = inspected(&host, &repository, &target);
    let expiry = host.lock().unwrap().stop_inspections[&token].expires;
    let token = stopped(&host, &repository, &target, &snapshot, token);
    assert_eq!(
        host.lock().unwrap().stop_inspections[&token].expires,
        expiry
    );
    assert!(request(&host, 1, removal(&repository, &target, &snapshot, None)).is_err());
    assert!(target.exists());
    let token = removed(&host, &repository, &target, &snapshot, token);
    assert!(!target.exists());
    assert_eq!(
        host.lock().unwrap().stop_inspections[&token].expires,
        expiry
    );
    assert!(matches!(
        request(&host, 1, branch(&repository, &snapshot, false, token)).unwrap(),
        Reply::BranchDeleted { .. }
    ));
    assert!(matches!(
        request(&host, 1, branch(&repository, &snapshot, true, token)).unwrap(),
        Reply::BranchDeleteRefused {
            failure: crate::worktree_control::BranchDeleteFailure {
                kind: BranchDeleteFailureKind::AuthorityChanged,
                ..
            },
            continuation: None
        }
    ));
}
#[test]
fn unmerged_refusal_keeps_same_branch_token_for_explicit_force_only() {
    const TEST: &str = "host::worktree_inspections::tests::continuation_tests::unmerged_refusal_keeps_same_branch_token_for_explicit_force_only";
    let Some(root) = super::private_runtime::enter(TEST) else {
        return;
    };
    let (repository, target) = setup(&root, true);
    let sibling = root.join("surviving-sibling");
    git(
        &repository,
        &[
            "worktree",
            "add",
            "--quiet",
            "-b",
            "sibling",
            sibling.to_str().unwrap(),
        ],
    );
    let host = host_with_registry(&root);
    let (snapshot, token) = inspected(&host, &repository, &target);
    let token = stopped(&host, &repository, &target, &snapshot, token);
    let token = removed(&host, &repository, &target, &snapshot, token);
    assert!(
        matches!(request(&host,1,branch(&repository,&snapshot,false,token)).unwrap(),Reply::BranchDeleteRefused { failure: crate::worktree_control::BranchDeleteFailure { kind: BranchDeleteFailureKind::Unmerged, .. }, continuation: Some(same) } if same == token)
    );
    assert!(matches!(
        request(&host, 1, branch(&repository, &snapshot, true, token)).unwrap(),
        Reply::BranchDeleted { .. }
    ));
}
#[test]
fn unmerged_force_retry_revalidates_tip_expiry_and_original_connection() {
    const TEST: &str = "host::worktree_inspections::tests::continuation_tests::unmerged_force_retry_revalidates_tip_expiry_and_original_connection";
    let Some(root) = super::private_runtime::enter(TEST) else {
        return;
    };
    for change in ["moved-tip", "expired", "reconnected"] {
        let case = root.join(change);
        std::fs::create_dir(&case).unwrap();
        let (repository, target) = setup(&case, true);
        let host = host_with_registry(&case);
        let (snapshot, token) = inspected(&host, &repository, &target);
        let expiry = host.lock().unwrap().stop_inspections[&token].expires;
        let token = stopped(&host, &repository, &target, &snapshot, token);
        let token = removed(&host, &repository, &target, &snapshot, token);
        assert!(matches!(
            request(&host, 1, branch(&repository, &snapshot, false, token)).unwrap(),
            Reply::BranchDeleteRefused {
                failure: crate::worktree_control::BranchDeleteFailure {
                    kind: BranchDeleteFailureKind::Unmerged,
                    ..
                },
                continuation: Some(same),
            } if same == token
        ));
        assert_eq!(
            host.lock().unwrap().stop_inspections[&token].expires,
            expiry,
            "an unmerged refusal must not renew the original review deadline"
        );
        match change {
            "moved-tip" => git(&repository, &["update-ref", "refs/heads/topic", "HEAD"]),
            "expired" => {
                host.lock()
                    .unwrap()
                    .stop_inspections
                    .get_mut(&token)
                    .and_then(Arc::get_mut)
                    .unwrap()
                    .expires = Instant::now();
            }
            "reconnected" => {
                host.lock().unwrap().unregister_client(1);
                host.lock().unwrap().register_client(1);
            }
            _ => unreachable!(),
        }
        let reply = request(&host, 1, branch(&repository, &snapshot, true, token)).unwrap();
        assert!(
            matches!(
                reply,
                Reply::BranchDeleteRefused {
                    failure: crate::worktree_control::BranchDeleteFailure {
                        kind: BranchDeleteFailureKind::AuthorityChanged,
                        ..
                    },
                    continuation: None,
                }
            ),
            "{change} must invalidate the retained force retry"
        );
        git(&repository, &["show-ref", "--verify", "refs/heads/topic"]);
        assert!(
            !target.exists(),
            "known removal stays complete for {change}"
        );
        if change == "moved-tip" {
            // Restoring the reviewed tip must not revive a spent stale grant.
            git(
                &repository,
                &[
                    "update-ref",
                    "refs/heads/topic",
                    snapshot.head_oid.as_deref().unwrap(),
                ],
            );
            assert!(matches!(
                request(&host, 1, branch(&repository, &snapshot, true, token)).unwrap(),
                Reply::BranchDeleteRefused {
                    failure: crate::worktree_control::BranchDeleteFailure {
                        kind: BranchDeleteFailureKind::AuthorityChanged,
                        ..
                    },
                    continuation: None,
                }
            ));
            git(&repository, &["show-ref", "--verify", "refs/heads/topic"]);
        }
    }
}

#[test]
fn changed_files_and_replaced_checkout_after_stop_never_rebind_removal() {
    const TEST: &str = "host::worktree_inspections::tests::continuation_tests::changed_files_and_replaced_checkout_after_stop_never_rebind_removal";
    let Some(root) = super::private_runtime::enter(TEST) else {
        return;
    };
    let (repository, target) = setup(&root, false);
    let host = host_with_registry(&root);
    let (snapshot, token) = inspected(&host, &repository, &target);
    let token = stopped(&host, &repository, &target, &snapshot, token);
    std::fs::write(target.join("new-user-work"), "preserve").unwrap();
    let refusal = request(
        &host,
        1,
        removal(&repository, &target, &snapshot, Some(token)),
    )
    .unwrap_err();
    assert!(
        refusal.contains(REFRESH),
        "wrong refusal mechanism: {refusal}"
    );
    assert_eq!(
        std::fs::read_to_string(target.join("new-user-work")).unwrap(),
        "preserve"
    );
    std::fs::remove_file(target.join("new-user-work")).unwrap();
    let (snapshot, token) = inspected(&host, &repository, &target);
    let token = stopped(&host, &repository, &target, &snapshot, token);
    std::fs::rename(&target, root.join("original-target")).unwrap();
    std::fs::create_dir(&target).unwrap();
    std::fs::write(target.join("replacement"), "preserve").unwrap();
    let refusal = request(
        &host,
        1,
        removal(&repository, &target, &snapshot, Some(token)),
    )
    .unwrap_err();
    assert!(
        refusal.contains(REFRESH),
        "wrong refusal mechanism: {refusal}"
    );
    assert!(root.join("original-target").exists());
    assert!(target.join("replacement").exists());
}
#[test]
fn new_actual_actor_after_stop_is_preserved_by_removal_refusal() {
    const TEST: &str = "host::worktree_inspections::tests::continuation_tests::new_actual_actor_after_stop_is_preserved_by_removal_refusal";
    let Some(root) = super::private_runtime::enter(TEST) else {
        return;
    };
    let (repository, target) = setup(&root, false);
    let host = host_with_registry(&root);
    let (snapshot, token) = inspected(&host, &repository, &target);
    let token = stopped(&host, &repository, &target, &snapshot, token);
    let actor = super::live_actor_tests::Actor::start(&host, &target);
    let refusal = request(
        &host,
        1,
        removal(&repository, &target, &snapshot, Some(token)),
    )
    .unwrap_err();
    assert!(
        refusal.contains("uses this worktree")
            || refusal.contains("New sessions appeared after stopping"),
        "the live actor must veto removal, not an unrelated missing prerequisite: {refusal}"
    );
    actor.assert_live(&host);
    assert!(target.exists());
}
#[test]
fn distinct_linked_repository_keeps_its_original_private_head_merge_target() {
    const TEST: &str = "host::worktree_inspections::tests::continuation_tests::distinct_linked_repository_keeps_its_original_private_head_merge_target";
    let Some(root) = super::private_runtime::enter(TEST) else {
        return;
    };
    let (primary, target) = setup(&root, true);
    let repository = root.join("linked-source");
    git(
        &primary,
        &[
            "worktree",
            "add",
            "--quiet",
            "-b",
            "source",
            repository.to_str().unwrap(),
            "topic",
        ],
    );
    let host = host_with_registry(&root);
    let (snapshot, token) = inspected(&host, &repository, &target);
    assert!(
        host.lock().unwrap().stop_inspections[&token]
            .physical
            .repository_admin
            .is_some()
    );
    let token = stopped(&host, &repository, &target, &snapshot, token);
    let token = removed(&host, &repository, &target, &snapshot, token);
    // topic is unmerged into primary HEAD but merged into this source HEAD.
    assert!(matches!(
        request(&host, 1, branch(&repository, &snapshot, false, token)).unwrap(),
        Reply::BranchDeleted { .. }
    ));
    assert!(repository.exists());
    assert!(primary.exists());
}
#[test]
fn moved_tip_disconnect_and_expiry_do_not_create_a_force_grant() {
    const TEST: &str = "host::worktree_inspections::tests::continuation_tests::moved_tip_disconnect_and_expiry_do_not_create_a_force_grant";
    let Some(root) = super::private_runtime::enter(TEST) else {
        return;
    };
    let (repository, target) = setup(&root, true);
    let host = host_with_registry(&root);
    let (snapshot, token) = inspected(&host, &repository, &target);
    let token = stopped(&host, &repository, &target, &snapshot, token);
    let token = removed(&host, &repository, &target, &snapshot, token);
    git(&repository, &["update-ref", "refs/heads/topic", "HEAD"]);
    assert!(matches!(
        request(&host, 1, branch(&repository, &snapshot, true, token)).unwrap(),
        Reply::BranchDeleteRefused {
            continuation: None,
            ..
        }
    ));
    assert!(!host.lock().unwrap().stop_inspections.contains_key(&token));
    // A separate still-existing target exercises original expiry and stream.
    let other = root.join("other");
    git(
        &repository,
        &[
            "worktree",
            "add",
            "--quiet",
            "-b",
            "other",
            other.to_str().unwrap(),
        ],
    );
    let (snapshot, token) = inspected(&host, &repository, &other);
    let token = stopped(&host, &repository, &other, &snapshot, token);
    host.lock()
        .unwrap()
        .stop_inspections
        .get_mut(&token)
        .and_then(Arc::get_mut)
        .unwrap()
        .expires = Instant::now();
    assert!(
        request(
            &host,
            1,
            removal(&repository, &other, &snapshot, Some(token))
        )
        .is_err()
    );
    assert!(other.exists());
    let (snapshot, token) = inspected(&host, &repository, &other);
    let token = stopped(&host, &repository, &other, &snapshot, token);
    host.lock().unwrap().unregister_client(1);
    host.lock().unwrap().register_client(1);
    assert!(
        request(
            &host,
            1,
            removal(&repository, &other, &snapshot, Some(token))
        )
        .is_err()
    );
    assert!(other.exists());
}

#[test]
fn known_removal_survives_expired_post_effect_continuation_without_branch_grant() {
    const TEST: &str = "host::worktree_inspections::tests::continuation_tests::known_removal_survives_expired_post_effect_continuation_without_branch_grant";
    let Some(root) = super::private_runtime::enter(TEST) else {
        return;
    };
    let (repository, target) = setup(&root, false);
    let host = host_with_registry(&root);
    let (snapshot, token) = inspected(&host, &repository, &target);
    let token = stopped(&host, &repository, &target, &snapshot, token);
    super::super::continuation::AFTER_REMOVE.with(|slot| {
        *slot.borrow_mut() = Some(Box::new(|inspection| {
            inspection.expires = Instant::now();
        }));
    });
    struct Clear;
    impl Drop for Clear {
        fn drop(&mut self) {
            super::super::continuation::AFTER_REMOVE.with(|slot| {
                slot.borrow_mut().take();
            });
        }
    }
    let _clear = Clear;
    let reply = request(
        &host,
        1,
        removal(&repository, &target, &snapshot, Some(token)),
    )
    .unwrap();
    match reply {
        Reply::Removed {
            continuation: None,
            warnings,
            ..
        } => assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("branch was retained"))
        ),
        _ => panic!("known removal must survive failed continuation publication"),
    }
    assert!(!target.exists());
    git(&repository, &["show-ref", "--verify", "refs/heads/topic"]);
    assert!(!host.lock().unwrap().stop_inspections.contains_key(&token));
    assert!(matches!(
        request(&host, 1, branch(&repository, &snapshot, true, token)).unwrap(),
        Reply::BranchDeleteRefused {
            continuation: None,
            ..
        }
    ));
}

#[test]
fn removed_source_checkout_continues_on_original_common_without_deleted_head() {
    const TEST: &str = "host::worktree_inspections::tests::continuation_tests::removed_source_checkout_continues_on_original_common_without_deleted_head";
    let Some(root) = super::private_runtime::enter(TEST) else {
        return;
    };
    let (primary, target) = setup(&root, true);
    let host = host_with_registry(&root);
    let (snapshot, token) = inspected(&host, &target, &target);
    let token = stopped(&host, &target, &target, &snapshot, token);
    let token = removed(&host, &target, &target, &snapshot, token);
    assert!(!target.exists());
    assert!(
        matches!(request(&host,1,branch(&target,&snapshot,false,token)).unwrap(),Reply::BranchDeleteRefused { failure: crate::worktree_control::BranchDeleteFailure { kind: BranchDeleteFailureKind::Unmerged, .. }, continuation: Some(same) } if same == token)
    );
    assert!(matches!(
        request(&host, 1, branch(&target, &snapshot, true, token)).unwrap(),
        Reply::BranchDeleted { .. }
    ));
    assert!(primary.exists());
}

#[test]
fn target_and_source_locators_cannot_borrow_a_sibling_registration() {
    const TEST: &str = "host::worktree_inspections::tests::continuation_tests::target_and_source_locators_cannot_borrow_a_sibling_registration";
    let Some(root) = super::private_runtime::enter(TEST) else {
        return;
    };
    let (repository, target) = setup(&root, false);
    let sibling = root.join("sibling");
    git(
        &repository,
        &[
            "worktree",
            "add",
            "--quiet",
            "-b",
            "sibling",
            sibling.to_str().unwrap(),
        ],
    );
    let third = root.join("third");
    git(
        &repository,
        &[
            "worktree",
            "add",
            "--quiet",
            "-b",
            "third",
            third.to_str().unwrap(),
        ],
    );
    let host = host_with_registry(&root);
    let original = std::fs::read(target.join(".git")).unwrap();
    let sibling_locator = std::fs::read(sibling.join(".git")).unwrap();
    std::fs::write(target.join(".git"), &sibling_locator).unwrap();
    let refusal = request(
        &host,
        1,
        Operation::InspectForSessionStop {
            repository: repository.clone(),
            path: target.clone(),
        },
    )
    .unwrap_err();
    assert!(refusal.contains("backlink disagree"), "{refusal}");
    assert!(target.exists() && sibling.exists());
    assert!(host.lock().unwrap().stop_inspections.is_empty());
    std::fs::write(target.join(".git"), &original).unwrap();
    inspected(&host, &repository, &target);
    std::fs::write(target.join(".git"), &sibling_locator).unwrap();
    let refusal = request(
        &host,
        1,
        Operation::InspectForSessionStop {
            repository: target.clone(),
            path: third.clone(),
        },
    )
    .unwrap_err();
    assert!(refusal.contains("backlink disagree"), "{refusal}");
    std::fs::write(target.join(".git"), &original).unwrap();
    inspected(&host, &target, &third);
}

#[test]
fn separate_git_primary_retains_its_checkout_but_metadata_only_source_refuses() {
    const TEST: &str = "host::worktree_inspections::tests::continuation_tests::separate_git_primary_retains_its_checkout_but_metadata_only_source_refuses";
    let Some(root) = super::private_runtime::enter(TEST) else {
        return;
    };
    let repository = root.join("primary");
    let metadata = root.join("metadata");
    git(
        &root,
        &[
            "init",
            "--quiet",
            "--initial-branch=main",
            "--separate-git-dir",
            metadata.to_str().unwrap(),
            repository.to_str().unwrap(),
        ],
    );
    git(
        &repository,
        &["commit", "--quiet", "--allow-empty", "-m", "base"],
    );
    let target = root.join("target");
    git(
        &repository,
        &[
            "worktree",
            "add",
            "--quiet",
            "-b",
            "topic",
            target.to_str().unwrap(),
        ],
    );
    let host = host_with_registry(&root);
    let refusal = request(
        &host,
        1,
        Operation::InspectForSessionStop {
            repository: metadata.clone(),
            path: target.clone(),
        },
    )
    .unwrap_err();
    assert!(refusal.contains("unbound metadata"), "{refusal}");
    let (snapshot, token) = inspected(&host, &repository, &target);
    let token = stopped(&host, &repository, &target, &snapshot, token);
    let token = removed(&host, &repository, &target, &snapshot, token);
    assert!(matches!(
        request(&host, 1, branch(&repository, &snapshot, false, token)).unwrap(),
        Reply::BranchDeleted { .. }
    ));
    assert!(!target.exists());
    assert!(repository.exists() && metadata.exists());
    // A native create with the same name succeeds only if deletion removed it.
    git(&repository, &["branch", "topic", "main"]);
}
