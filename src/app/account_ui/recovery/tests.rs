use super::*;

fn candidate(provider: Provider, key: u128) -> Candidate {
    Candidate {
        provider,
        key: Uuid::from_u128(key),
    }
}

fn replace(queue: &mut Queue, candidates: &[Candidate]) {
    assert!(queue.replace_candidates(queue.owner(), candidates.to_vec()));
}

fn next(queue: &mut Queue) -> Attempt {
    queue.next_pending(&BTreeSet::new()).unwrap()
}

#[test]
fn registry_reads_follow_accepted_completion_order() {
    let mut callbacks = RegistryCallbacks::default();
    // reload_registry only has a shared reference while creating its task.
    let shared = &callbacks;
    let older = shared.issue_read_at(40);
    let newer = shared.issue_read_at(50);
    assert_eq!(older.minimum_revision(), 40);
    assert_eq!(newer.minimum_revision(), 50);
    assert_ne!(older, newer);

    // The old read remains useful while a newer request is still pending.
    assert!(callbacks.accept_read(older));
    assert!(callbacks.accept_read(newer));
    assert!(!callbacks.accept_read(older));
    assert!(callbacks.accept_read(newer));
    assert!(callbacks.accept_read(callbacks.issue_read()));
}

#[test]
fn checked_recovery_rotation_rejects_old_registry_reads_and_writes() {
    let mut callbacks = RegistryCallbacks::default();
    let old_owner = callbacks.capture_owner();
    let applied = callbacks.issue_read();
    let pending = callbacks.issue_read();
    assert!(callbacks.accept_read(applied));
    assert!(callbacks.accept_owner(old_owner));

    // Integration must already have accepted the exact current recovery
    // attempt and checked result before resetting this callback generation.
    callbacks.reset_after_checked_recovery();
    assert!(!callbacks.accept_owner(old_owner));
    assert!(!callbacks.accept_read(pending));
    assert!(!callbacks.accept_read(applied));

    let new_owner = callbacks.capture_owner();
    assert_ne!(new_owner, old_owner);
    let new_read = callbacks.issue_read();
    assert_eq!(new_read.serial, 1);
    assert!(callbacks.accept_read(new_read));
    assert!(callbacks.accept_owner(new_owner));
    // Rejecting old work also leaves the new accepted sequence intact.
    assert!(!callbacks.accept_read(pending));
    assert!(callbacks.accept_read(new_read));
}

#[test]
fn a_different_device_registry_gate_rejects_copied_callback_tokens() {
    let old = RegistryCallbacks::default();
    let old_read = old.issue_read();
    let old_write = old.capture_owner();
    let mut current = RegistryCallbacks::default();
    assert_ne!(current.capture_owner(), old_write);
    assert!(!current.accept_read(old_read));
    assert!(!current.accept_owner(old_write));

    let current_read = current.issue_read();
    let current_write = current.capture_owner();
    assert!(current.accept_read(current_read));
    assert!(current.accept_owner(current_write));
    assert!(!current.accept_read(old_read));
    assert!(current.accept_read(current_read));
}

#[test]
fn duplicate_loaded_preserves_one_attempt_and_terminal_result() {
    let home = candidate(Provider::Claude, 1);
    let mut queue = Queue::default();
    let owner = queue.owner();
    replace(&mut queue, &[home.clone(), home.clone()]);
    assert_eq!(queue.entries().len(), 1);
    assert_eq!(queue.status(&home), Some(&Status::Pending));

    let attempt = next(&mut queue);
    replace(&mut queue, std::slice::from_ref(&home));
    assert_eq!(queue.owner(), owner);
    assert_eq!(queue.status(&home), Some(&Status::Checking));
    assert!(queue.next_pending(&BTreeSet::new()).is_none());
    assert_eq!(queue.finish(&attempt, Outcome::Restored), Finish::Applied);

    for _ in 0..3 {
        replace(&mut queue, std::slice::from_ref(&home));
        assert_eq!(queue.status(&home), Some(&Status::Restored));
        assert!(queue.next_pending(&BTreeSet::new()).is_none());
    }
    assert!(!queue.retry(&home));
    assert!(!queue.suppresses(home.provider));
}

#[test]
fn a_new_device_owner_rejects_old_loaded_and_completion() {
    let home = candidate(Provider::Codex, 2);
    let mut old = Queue::default();
    replace(&mut old, std::slice::from_ref(&home));
    let old_attempt = next(&mut old);

    let mut current = Queue::default();
    assert_ne!(current.owner(), old.owner());
    assert!(!current.replace_candidates(old.owner(), vec![home.clone()]));
    assert!(current.entries().is_empty());
    replace(&mut current, std::slice::from_ref(&home));
    let current_attempt = next(&mut current);
    assert_eq!(
        current.finish(&old_attempt, Outcome::Restored),
        Finish::Ignored
    );
    assert_eq!(current.status(&home), Some(&Status::Checking));
    assert!(current.next_pending(&BTreeSet::new()).is_none());
    assert_eq!(
        current.finish(&current_attempt, Outcome::NoLogin),
        Finish::Applied
    );
}

#[test]
fn removing_and_rediscovering_a_candidate_does_not_adopt_its_old_result() {
    let home = candidate(Provider::Claude, 3);
    let mut queue = Queue::default();
    replace(&mut queue, std::slice::from_ref(&home));
    let old = next(&mut queue);
    replace(&mut queue, &[]);
    assert!(queue.entries().is_empty());
    assert!(queue.suppresses(home.provider));
    assert!(queue.next_pending(&BTreeSet::new()).is_none());

    replace(&mut queue, std::slice::from_ref(&home));
    assert_eq!(queue.status(&home), Some(&Status::Pending));
    assert!(queue.next_pending(&BTreeSet::new()).is_none());
    assert_eq!(queue.finish(&old, Outcome::Restored), Finish::Retired);
    assert_eq!(queue.status(&home), Some(&Status::Pending));

    let current = next(&mut queue);
    assert_ne!(current, old);
    assert_eq!(queue.finish(&old, Outcome::NoLogin), Finish::Ignored);
    assert_eq!(queue.status(&home), Some(&Status::Checking));
    assert_eq!(queue.finish(&current, Outcome::Restored), Finish::Applied);
}

#[test]
fn failed_attempts_wait_for_an_explicit_retry_across_loads_and_ticks() {
    let home = candidate(Provider::Codex, 4);
    for outcome in [
        Outcome::Busy,
        Outcome::NoLogin,
        Outcome::Failed("Status could not be checked".into()),
    ] {
        let mut queue = Queue::default();
        replace(&mut queue, std::slice::from_ref(&home));
        assert!(!queue.retry(&home));
        let first = next(&mut queue);
        assert!(!queue.retry(&home));
        assert_eq!(queue.finish(&first, outcome.clone()), Finish::Applied);
        for _ in 0..3 {
            replace(&mut queue, std::slice::from_ref(&home));
            assert_eq!(queue.status(&home), Some(&Status::from(outcome.clone())));
            assert!(queue.next_pending(&BTreeSet::new()).is_none());
        }
        assert!(!queue.suppresses(home.provider));
        assert!(queue.retry(&home));
        assert!(queue.suppresses(home.provider));
        assert!(!queue.retry(&home));
        let second = next(&mut queue);
        assert_ne!(first, second);
        assert_eq!(queue.finish(&first, Outcome::Restored), Finish::Ignored);
        assert_eq!(queue.finish(&second, Outcome::Restored), Finish::Applied);
    }
}

#[test]
fn a_new_hint_runs_without_retrying_an_unchanged_failure() {
    let old = candidate(Provider::Claude, 5);
    let added = candidate(Provider::Codex, 6);
    let mut queue = Queue::default();
    replace(&mut queue, std::slice::from_ref(&old));
    let first = next(&mut queue);
    assert_eq!(queue.finish(&first, Outcome::Busy), Finish::Applied);
    assert!(queue.can_retry(&old));

    replace(&mut queue, &[added.clone(), old.clone(), added.clone()]);
    assert_eq!(queue.entries().len(), 2);
    assert_eq!(queue.entries()[0].candidate(), &added);
    assert_eq!(queue.status(&old), Some(&Status::Busy));
    let second = next(&mut queue);
    assert_eq!(second.candidate(), &added);
    assert!(!queue.can_retry(&old));
    assert!(!queue.retry(&old));
    assert_eq!(queue.finish(&second, Outcome::Restored), Finish::Applied);
    assert!(queue.can_retry(&old));
    assert!(!queue.can_retry(&added));
    assert!(queue.retry(&old));
    assert!(!queue.can_retry(&old));
}

#[test]
fn providers_wait_for_existing_probes_and_recover_serially() {
    let codex = candidate(Provider::Codex, 7);
    let claude = candidate(Provider::Claude, 8);
    let another = candidate(Provider::Codex, 9);
    let mut queue = Queue::default();
    replace(
        &mut queue,
        &[codex.clone(), claude.clone(), another.clone()],
    );
    let blocked = BTreeSet::from([Provider::Codex]);
    assert!(queue.suppresses(Provider::Codex));
    assert!(queue.suppresses(Provider::Claude));
    assert!(!queue.suppresses(Provider::Omp));

    let first = queue.next_pending(&blocked).unwrap();
    assert_eq!(first.candidate(), &claude);
    assert!(queue.next_pending(&BTreeSet::new()).is_none());
    assert_eq!(queue.finish(&first, Outcome::NoLogin), Finish::Applied);
    assert!(!queue.suppresses(Provider::Claude));
    assert!(queue.next_pending(&blocked).is_none());

    // The ordinary Codex probe completed; its callback pumps the queue.
    let second = next(&mut queue);
    assert_eq!(second.candidate(), &codex);
    assert_eq!(queue.finish(&second, Outcome::Restored), Finish::Applied);
    assert!(queue.suppresses(Provider::Codex));
    let third = next(&mut queue);
    assert_eq!(third.candidate(), &another);
    assert_eq!(queue.finish(&third, Outcome::Restored), Finish::Applied);
    assert!(!queue.suppresses(Provider::Codex));
    assert!(queue.next_pending(&BTreeSet::new()).is_none());
}

#[test]
fn a_completion_must_match_owner_serial_and_provider_key() {
    let home = candidate(Provider::Claude, 10);
    let mut queue = Queue::default();
    replace(&mut queue, std::slice::from_ref(&home));
    let attempt = next(&mut queue);
    let mut wrong_owner = attempt.clone();
    wrong_owner.owner = Uuid::nil();
    let mut wrong_serial = attempt.clone();
    wrong_serial.serial += 1;
    let mut wrong_provider = attempt.clone();
    wrong_provider.candidate.provider = Provider::Codex;
    let mut wrong_key = attempt.clone();
    wrong_key.candidate.key = Uuid::from_u128(11);

    for stale in [wrong_owner, wrong_serial, wrong_provider, wrong_key] {
        assert_eq!(queue.finish(&stale, Outcome::Restored), Finish::Ignored);
        assert_eq!(queue.status(&home), Some(&Status::Checking));
        assert!(queue.next_pending(&BTreeSet::new()).is_none());
    }
    assert_eq!(queue.finish(&attempt, Outcome::Busy), Finish::Applied);
}

#[test]
fn removing_a_finished_candidate_removes_its_status_and_retry_action() {
    let home = candidate(Provider::Codex, 12);
    let mut queue = Queue::default();
    replace(&mut queue, std::slice::from_ref(&home));
    let attempt = next(&mut queue);
    assert_eq!(queue.finish(&attempt, Outcome::NoLogin), Finish::Applied);
    replace(&mut queue, &[]);
    assert!(queue.status(&home).is_none());
    assert!(!queue.retry(&home));
    assert!(!queue.suppresses(home.provider));
    assert_eq!(queue.finish(&attempt, Outcome::Restored), Finish::Ignored);
}

fn registry(revision: u64, home: Option<&Candidate>) -> Registry {
    let mut registry: Registry =
        toml::from_str(&format!("version = 5\nrevision = {revision}\n")).unwrap();
    if let Some(home) = home {
        registry
            .register(
                crate::account::registry::Registered {
                    key: home.key,
                    provider: home.provider,
                    linked_home: None,
                    label: None,
                    email: Some("preserved@example.test".into()),
                    organization: None,
                    added_at: 10,
                },
                &[],
            )
            .unwrap();
    }
    registry
}

fn settled(registry: Registry) -> Result<Settled, String> {
    Ok(Settled::Registered {
        registry,
        identity: None,
        snapshot: None,
    })
}

#[test]
fn checked_registration_accepts_idempotent_and_archived_original_managed_rows() {
    let home = candidate(Provider::Codex, 13);
    let original = registry(2, Some(&home));
    let checked = checked_restoration(&home, settled(original.clone())).unwrap();
    assert_eq!(checked.registry, original);
    let mut archived = original;
    assert!(archived.archive_duplicate(home.key, &crate::account::Account::system(home.provider)));
    let checked = checked_restoration(&home, settled(archived)).unwrap();
    assert!(checked.registry.is_archived(home.key));
}

#[test]
fn checked_registration_rejects_another_key_provider_linked_or_unconfirmed_result() {
    let home = candidate(Provider::Claude, 14);
    let other_key = candidate(home.provider, 15);
    let other_provider = candidate(Provider::Codex, 14);
    for wrong in [
        registry(1, Some(&other_key)),
        registry(1, Some(&other_provider)),
    ] {
        assert!(checked_restoration(&home, settled(wrong)).is_err());
    }
    let mut linked: serde_json::Value = serde_json::to_value(registry(1, Some(&home))).unwrap();
    linked["account"][0]["linked_home"] = serde_json::json!("/private/fixture/linked-home");
    assert!(checked_restoration(&home, settled(serde_json::from_value(linked).unwrap())).is_err());
    let mut future: serde_json::Value = serde_json::to_value(registry(1, Some(&home))).unwrap();
    future["version"] = serde_json::json!(999);
    assert!(checked_restoration(&home, settled(serde_json::from_value(future).unwrap())).is_err());
    assert!(checked_restoration(&home, Ok(Settled::Abandoned(None))).is_err());
    assert!(
        checked_restoration(
            &home,
            Ok(Settled::Duplicate {
                existing: crate::account::Account::system(home.provider).id(),
                registry: Some(registry(1, Some(&home))),
            })
        )
        .is_err()
    );
    assert_eq!(
        checked_restoration(&home, Err("Status was not confirmed".into()))
            .err()
            .as_deref(),
        Some("Status was not confirmed")
    );
}

#[test]
fn a_later_scan_preserves_edits_only_when_it_contains_the_checked_original_row() {
    let home = candidate(Provider::Codex, 16);
    let checked = registry(2, Some(&home));
    let missing = registry(7, None);
    assert!(
        plan_registry(
            &home,
            checked.clone(),
            &Registry::default(),
            Some(&missing),
            None
        )
        .is_err()
    );
    let mut later = registry(7, Some(&home));
    let account =
        crate::account::Account::managed(home.provider, home.key, "/private/fixture".into());
    later.set_label(&account, Some("An unrelated later edit".into()));
    assert_eq!(
        plan_registry(
            &home,
            checked.clone(),
            &Registry::default(),
            Some(&later),
            None
        )
        .unwrap()
        .registry,
        later
    );
    assert_eq!(
        plan_registry(
            &home,
            checked.clone(),
            &Registry::default(),
            Some(&registry(1, Some(&home))),
            None
        )
        .unwrap()
        .registry,
        checked
    );
}

#[test]
fn a_request_captures_only_explicit_causal_reset_evidence() {
    let home = candidate(Provider::Claude, 17);
    let mut queue = Queue::default();
    replace(&mut queue, std::slice::from_ref(&home));
    let attempt = next(&mut queue);
    let callbacks = RegistryCallbacks::default();
    for reset_floor in [None, Some(40), Some(50)] {
        let request = Request::capture(attempt.clone(), &callbacks, reset_floor);
        assert_eq!(request.reset_floor, reset_floor);
        assert_eq!(request.candidate(), &home);
        assert!(callbacks.accept_owner(request.callback_owner));
    }
}

fn app() -> Cagents {
    super::super::super::tests::context_inspector_test_app(false)
}

fn active_request(app: &mut Cagents, home: &Candidate) -> Request {
    replace(&mut app.account_ui.recovery, std::slice::from_ref(home));
    let attempt = next(&mut app.account_ui.recovery);
    Request::capture(
        attempt,
        &app.account_ui.registry_callbacks,
        app.account_ui.registry_reset_floor,
    )
}

#[test]
fn checked_live_reset_installs_original_row_and_invalidates_old_callbacks() {
    let home = candidate(Provider::Claude, 18);
    let mut app = app();
    app.account_ui.registry = registry(80, Some(&home));
    app.account_ui.last_registry_scan = Some(registry(0, None));
    app.account_ui.registry_reset_floor = Some(app.account_ui.registry.revision());
    let old_owner = app.account_ui.registry_callbacks.capture_owner();
    let old_read = app.account_ui.registry_callbacks.issue_read();
    let request = active_request(&mut app, &home);
    assert_eq!(request.reset_floor, Some(80));
    let checked = registry(1, Some(&home));
    drop(app.preserved_restored(request, settled(checked.clone())));
    assert_eq!(app.account_ui.registry, checked);
    assert_eq!(app.account_ui.last_registry_scan, Some(checked));
    assert_eq!(
        app.account_ui.recovery.status(&home),
        Some(&Status::Restored)
    );
    assert!(!app.account_ui.registry_callbacks.accept_owner(old_owner));
    assert_eq!(app.account_ui.registry_reset_floor, None);
    assert!(!app.account_ui.registry_callbacks.accept_read(old_read));
    assert!(
        app.cards.is_empty(),
        "restoration must not open a conversation"
    );
}

#[test]
fn stale_lifecycle_reply_retires_its_operation_without_applying_response_data() {
    let home = candidate(Provider::Codex, 19);
    let mut app = app();
    app.account_ui.registry = registry(30, None);
    app.account_ui.last_registry_scan = Some(registry(0, None));
    let request = active_request(&mut app, &home);
    app.account_ui
        .registry_callbacks
        .reset_after_checked_recovery();
    let before = app.account_ui.registry.clone();
    let scan_before = app.account_ui.last_registry_scan.clone();
    drop(app.preserved_restored(request, settled(registry(1, Some(&home)))));
    assert_eq!(app.account_ui.registry, before);
    assert_eq!(app.account_ui.last_registry_scan, scan_before);
    assert!(app.status.is_none());
    assert!(matches!(
        app.account_ui.recovery.status(&home),
        Some(Status::Failed(_))
    ));
    assert!(app.account_ui.recovery.active.is_none());
    assert!(
        app.account_ui
            .recovery
            .next_pending(&BTreeSet::new())
            .is_none()
    );
}

#[test]
fn unknown_cli_waits_and_authoritative_absence_fails_without_a_retry_loop() {
    let home = candidate(Provider::Claude, 20);
    let mut app = app();
    replace(&mut app.account_ui.recovery, std::slice::from_ref(&home));
    drop(app.pump_account_recovery());
    assert_eq!(
        app.account_ui.recovery.status(&home),
        Some(&Status::Pending)
    );
    assert!(app.account_ui.recovery.active.is_none());
    app.account_ui.installed.insert(home.provider, false);
    app.account_ui.recovery_cli_pending = true;
    drop(app.pump_account_recovery());
    assert_eq!(
        app.account_ui.recovery.status(&home),
        Some(&Status::Pending)
    );
    assert!(app.account_ui.recovery.active.is_none());
    app.account_ui.recovery_cli_pending = false;
    drop(app.pump_account_recovery());
    let failed = app.account_ui.recovery.status(&home).cloned();
    assert!(matches!(failed, Some(Status::Failed(_))));
    for _ in 0..3 {
        replace(&mut app.account_ui.recovery, std::slice::from_ref(&home));
        drop(app.pump_account_recovery());
        assert_eq!(app.account_ui.recovery.status(&home), failed.as_ref());
        assert!(app.account_ui.recovery.active.is_none());
    }
}

#[test]
fn retained_error_details_survive_reloading_and_dismissed_notices() {
    let home = candidate(Provider::Codex, 21);
    let mut app = app();
    let request = active_request(&mut app, &home);
    drop(app.preserved_restored(request, Err("A precise retained failure".into())));
    app.status = None;
    drop(app.toggle_preserved_details(home.clone()));
    replace(&mut app.account_ui.recovery, std::slice::from_ref(&home));
    let entry = &app.account_ui.recovery.entries()[0];
    assert!(entry.details_open);
    assert_eq!(
        entry.status(),
        &Status::Failed("A precise retained failure".into())
    );
}

#[test]
fn an_unobserved_lower_reply_fails_without_claiming_restored_readiness() {
    let home = candidate(Provider::Claude, 22);
    let mut app = app();
    let before = registry(50, None);
    app.account_ui.registry = before.clone();
    app.account_ui.last_registry_scan = Some(before.clone());
    let request = active_request(&mut app, &home);
    assert_eq!(request.reset_floor, None);
    let owner = app.account_ui.registry_callbacks.capture_owner();
    drop(app.preserved_restored(request, settled(registry(1, Some(&home)))));
    assert_eq!(app.account_ui.registry, before);
    assert_eq!(app.account_ui.last_registry_scan, Some(before));
    assert!(app.account_ui.registry_callbacks.accept_owner(owner));
    assert!(matches!(
        app.account_ui.recovery.status(&home),
        Some(Status::Failed(_))
    ));
    assert_ne!(
        app.status.as_ref().unwrap().severity,
        super::super::NoticeSeverity::Success
    );
}

#[test]
fn a_confirming_scan_before_the_reply_restores_without_recreating_the_hint() {
    let home = candidate(Provider::Codex, 23);
    let mut app = app();
    app.account_ui.registry = registry(50, None);
    app.account_ui.last_registry_scan = Some(registry(50, None));
    let request = active_request(&mut app, &home);
    assert_eq!(request.reset_floor, None);
    let owner = app.account_ui.registry_callbacks.capture_owner();
    let confirmed = registry(1, Some(&home));
    drop(app.install_preserved_discovery(
        crate::account::recovery::Discovery::default(),
        confirmed.clone(),
    ));
    // The parent read reducer observed revision 1 below the issuance floor
    // of 50. Installing discovery alone must not infer that reset proof.
    assert_eq!(app.account_ui.registry_reset_floor, None);
    app.account_ui.registry_reset_floor = Some(app.account_ui.registry.revision());
    assert!(app.account_ui.recovery.entries().is_empty());
    assert!(app.account_ui.recovery.active.is_some());
    drop(app.preserved_restored(request, settled(confirmed.clone())));
    assert_eq!(app.account_ui.registry, confirmed);
    assert_eq!(app.account_ui.last_registry_scan, Some(confirmed));
    assert!(app.account_ui.recovery.entries().is_empty());
    assert!(app.account_ui.recovery.active.is_none());
    assert!(!app.account_ui.registry_callbacks.accept_owner(owner));
}

#[test]
fn a_newer_missing_or_relinked_scan_cannot_be_resurrected_by_an_old_reply() {
    let home = candidate(Provider::Claude, 24);
    for linked in [false, true] {
        for proof in [49, 50] {
            let mut app = app();
            let cached = registry(50, None);
            app.account_ui.registry = cached.clone();
            app.account_ui.last_registry_scan = Some(registry(0, None));
            app.account_ui.registry_reset_floor = Some(proof);
            let request = active_request(&mut app, &home);
            let newer = if linked {
                let mut value = serde_json::to_value(registry(2, Some(&home))).unwrap();
                value["account"][0]["linked_home"] = serde_json::json!("/private/fixture/relinked");
                serde_json::from_value(value).unwrap()
            } else {
                registry(2, None)
            };
            drop(app.install_preserved_discovery(
                crate::account::recovery::Discovery {
                    candidates: if linked { vec![] } else { vec![home.clone()] },
                    problem: None,
                },
                newer.clone(),
            ));
            drop(app.preserved_restored(request, settled(registry(1, Some(&home)))));
            assert_eq!(
                app.account_ui.registry,
                if proof == 50 { newer.clone() } else { cached }
            );
            assert_eq!(app.account_ui.last_registry_scan, Some(newer));
            assert!(app.account_ui.recovery.active.is_none());
            if linked {
                assert!(app.account_ui.recovery.entries().is_empty());
                assert_eq!(app.status.is_some(), proof == 50);
            } else {
                assert!(matches!(
                    app.account_ui.recovery.status(&home),
                    Some(Status::Failed(_))
                ));
            }
        }
    }
}

#[test]
fn an_equal_revision_scan_with_no_original_row_is_not_confirmation() {
    let home = candidate(Provider::Codex, 25);
    let reconciled = plan_registry(
        &home,
        registry(1, Some(&home)),
        &registry(50, None),
        Some(&registry(1, None)),
        Some(50),
    )
    .unwrap();
    assert!(reconciled.problem.is_some());
    assert!(!contains_original_home(&reconciled.registry, &home));
    assert!(
        plan_registry(
            &home,
            registry(1, Some(&home)),
            &registry(50, None),
            Some(&registry(1, Some(&home))),
            None,
        )
        .is_err(),
        "a low numeric revision alone is not reset evidence"
    );
}

#[test]
fn reset_proof_does_not_outlive_a_newer_ui_write() {
    let home = candidate(Provider::Codex, 30);
    let mut app = app();
    app.account_ui.registry = registry(50, None);
    app.account_ui.last_registry_scan = Some(registry(0, None));
    app.account_ui.registry_reset_floor = Some(50);
    let request = active_request(&mut app, &home);
    assert_eq!(request.reset_floor, Some(50));
    // A normal write became visible after the reset proof and request.
    app.account_ui.registry = registry(60, None);
    let scan_before = app.account_ui.last_registry_scan.clone();
    drop(app.preserved_restored(request, settled(registry(1, Some(&home)))));
    assert_eq!(app.account_ui.registry.revision(), 60);
    assert_eq!(app.account_ui.last_registry_scan, scan_before);
    assert!(matches!(
        app.account_ui.recovery.status(&home),
        Some(Status::Failed(_))
    ));
    assert_eq!(app.account_ui.registry_reset_floor, Some(50));
}

#[test]
fn restoration_does_not_override_discovery_absence_or_newer_observations() {
    let home = candidate(Provider::Claude, 26);
    let mut app = app();
    let homes = crate::account::home::Homes::at("/private/fixture/accounts");
    let account = crate::account::Account::managed(
        home.provider,
        home.key,
        homes.path(home.provider, home.key),
    );
    app.account_ui.homes = Some(homes);
    app.account_ui.registry = registry(0, Some(&home));
    app.account_ui.sync_accounts();
    let id = account.id();
    let old_identity = crate::account::Identity {
        email: Some("newer-observation@example.test".into()),
        organization: None,
        plan: None,
    };
    let old_snapshot = crate::account::UsageSnapshot::state(
        crate::account::UsageStatus::Ok,
        "newer observation",
        100,
    );
    let state = app.account_ui.account_mut(&id).unwrap();
    state.present = false;
    state.absent_since = Some(100);
    state.identity = Some(old_identity.clone());
    state.snapshot = Some(old_snapshot.clone());
    app.account_ui.last_registry_scan = Some(registry(0, None));
    let request = active_request(&mut app, &home);
    drop(app.preserved_restored(
        request,
        Ok(Settled::Registered {
            registry: registry(1, Some(&home)),
            identity: Some(crate::account::Identity {
                email: Some("older-reply@example.test".into()),
                organization: None,
                plan: None,
            }),
            snapshot: Some(crate::account::UsageSnapshot::state(
                crate::account::UsageStatus::Ok,
                "older reply",
                20,
            )),
        }),
    ));
    let state = app.account_ui.account(&id).unwrap();
    assert!(!state.present);
    assert_eq!(state.absent_since, Some(100));
    assert_eq!(state.identity.as_ref(), Some(&old_identity));
    assert_eq!(state.snapshot.as_ref(), Some(&old_snapshot));
}

#[test]
fn multiple_same_provider_rows_have_distinct_names_without_exposing_keys() {
    let mut queue = Queue::default();
    let homes = [
        candidate(Provider::Claude, 27),
        candidate(Provider::Codex, 28),
        candidate(Provider::Claude, 29),
    ];
    replace(&mut queue, &homes);
    let names = (0..homes.len())
        .map(|index| preserved_name(queue.entries(), index))
        .collect::<Vec<_>>();
    assert_eq!(
        names,
        [
            "Preserved Claude account 1",
            "Preserved Codex account",
            "Preserved Claude account 2"
        ]
    );
    for (home, name) in homes.iter().zip(names) {
        assert!(!name.contains(&home.key.to_string()));
    }
}

#[test]
fn a_completed_hint_requires_explicit_retry_after_proven_repeated_registry_loss() {
    let home = candidate(Provider::Codex, 31);
    let mut app = app();
    app.account_ui.last_registry_scan = Some(registry(0, None));
    let request = active_request(&mut app, &home);
    let registered = registry(1, Some(&home));
    drop(app.preserved_restored(request, settled(registered.clone())));
    assert_eq!(
        app.account_ui.recovery.status(&home),
        Some(&Status::Restored)
    );

    // The immediate follow-up cannot read metadata, so the completed hint
    // remains in the queue rather than being pruned as a registered home.
    drop(app.install_preserved_discovery(
        crate::account::recovery::Discovery {
            candidates: vec![],
            problem: Some("Metadata could not be read".into()),
        },
        registered,
    ));
    assert_eq!(
        app.account_ui.recovery.status(&home),
        Some(&Status::Restored)
    );
    let missing = || crate::account::recovery::Discovery {
        candidates: vec![home.clone()],
        problem: None,
    };
    // A delayed old read or expired proof must not announce another loss.
    for proof in [None, Some(0)] {
        app.account_ui.registry_reset_floor = proof;
        drop(app.install_preserved_discovery(missing(), registry(0, None)));
        assert_eq!(
            app.account_ui.recovery.status(&home),
            Some(&Status::Restored)
        );
        assert!(!app.account_ui.recovery.can_retry(&home));
    }

    app.account_ui.registry_reset_floor = Some(1);
    drop(app.install_preserved_discovery(missing(), registry(0, None)));
    let failure = app.account_ui.recovery.status(&home).cloned();
    assert!(matches!(&failure, Some(Status::Failed(message)) if message.contains("missing again")));
    assert!(app.account_ui.recovery.can_retry(&home));
    for _ in 0..2 {
        drop(app.install_preserved_discovery(missing(), registry(0, None)));
        assert_eq!(app.account_ui.recovery.status(&home), failure.as_ref());
        assert!(app.account_ui.recovery.active.is_none());
        assert!(
            app.account_ui
                .recovery
                .next_pending(&BTreeSet::new())
                .is_none()
        );
    }
    // Only the reader's explicit retry queues the same candidate again.
    drop(app.restore_preserved(home.clone()));
    assert_eq!(
        app.account_ui.recovery.status(&home),
        Some(&Status::Pending)
    );
    assert!(!app.account_ui.recovery.can_retry(&home));
}
