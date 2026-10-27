// Included in the existing Registry automation tests; no provider or shell runs.
fn history_occurrence(index: u64) -> Occurrence {
    Occurrence {
        due_ms: index * 60_000,
        local: format!("1970-01-01T{:02}:{:02}", index / 60, index % 60),
    }
}
fn history_precheck_definition(mut value: Definition) -> Definition {
    value.schedule.pattern = SchedulePattern::Cron {
        expression: "* * * * *".into(),
    };
    value.schedule.timezone = "UTC".into();
    value.precheck = Some(crate::automations::Precheck {
        command: "exit 7".into(),
        timeout_seconds: 5,
    });
    value
}
fn history_claim_manual(
    registry: &mut Registry,
    id: Uuid,
    revision: u64,
    now: u64,
) -> (Uuid, Claimed) {
    let request = Uuid::new_v4();
    let claim = registry
        .claim_automation(
            id,
            revision,
            None,
            crate::automations::Trigger::Manual { request },
            None,
            true,
            now,
        )
        .unwrap();
    (request, claim)
}
fn history_stop_precheck(registry: &mut Registry, firing: &Firing, now: u64, clear: bool) {
    assert!(
        registry
            .finish_automation_precheck(
                firing.id,
                crate::host::automations::precheck::Outcome::Stopped {
                    state: FiringState::Failed,
                    skip: None,
                    diagnostic: "Fixture precheck refused".into()
                },
                now
            )
            .unwrap()
            .is_none()
    );
    if clear {
        registry
            .confirm_retired_automation_precheck(firing.id, firing.revision, now + 1)
            .unwrap();
    }
}
fn history_prune_to(registry: &mut Registry, id: Uuid, limit: usize, now: u64) {
    let tx = registry
        .db
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .unwrap();
    history::prune(&tx, Some(id), now, limit).unwrap();
    tx.commit().unwrap();
}
fn history_count(registry: &Registry, id: Uuid) -> usize {
    registry
        .db
        .query_row(
            "SELECT count(*) FROM automation_firings WHERE automation=?1",
            [id.to_string()],
            |row| row.get(0),
        )
        .unwrap()
}
fn history_finish_agent(
    registry: &mut Registry,
    accepted: crate::agent_execution::Accepted,
    now: u64,
) {
    use crate::agent_execution::{State, Update};
    let run = registry.agent_run(accepted.run.id).unwrap();
    let run = if run.state == State::Accepted {
        registry
            .update_agent_run(
                run.id,
                run.revision,
                Update::Started {
                    provider_key: "history-fixture".into(),
                },
                now,
            )
            .unwrap()
    } else {
        run
    };
    registry
        .update_agent_run(
            run.id,
            run.revision,
            Update::Finished {
                state: State::Succeeded,
                result: Default::default(),
            },
            now + 1,
        )
        .unwrap();
    let done = registry
        .reconcile_automation_run(run.id, now + 2)
        .unwrap()
        .unwrap();
    let agent = registry.agent(run.agent).unwrap();
    let closed = registry.close_agent(&agent.origin, agent.id).unwrap();
    registry
        .finish_automation_release(done.id, &closed, now + 3)
        .unwrap();
}

#[test]
fn one_hundred_overlaps_keep_waiting_owner_and_one_exact_skip_group_after_reopen() {
    use crate::agent_execution::Update;
    let (mut registry, directory, mut definition) = fixture();
    definition.session_mode = SessionMode::Fresh;
    definition.schedule.pattern = SchedulePattern::Cron {
        expression: "* * * * *".into(),
    };
    definition.schedule.timezone = "UTC".into();
    let automation = registry
        .create_automation(definition, 0, test_identity())
        .unwrap();
    let enabled = registry
        .set_automation_enabled(
            automation.id,
            automation.revision,
            Some(history_occurrence(1)),
            true,
            1,
        )
        .unwrap();
    let (_, owner) =
        history_claim_manual(&mut registry, enabled.id, enabled.execution_revision, 100);
    let accepted = owner.accepted.unwrap();
    let started = registry
        .update_agent_run(
            accepted.run.id,
            accepted.run.revision,
            Update::Started {
                provider_key: "waiting-history".into(),
            },
            200,
        )
        .unwrap();
    registry
        .update_agent_run(started.id, started.revision, Update::Waiting, 300)
        .unwrap();
    registry.reconcile_automation_run(started.id, 301).unwrap();
    let original_name = enabled.definition.name.clone();
    let mut group = None;
    for index in 1..=100 {
        if index == 51 {
            let current = registry.automation(enabled.id).unwrap();
            let mut renamed = current.definition.clone();
            renamed.name = "Renamed while waiting".into();
            let renamed = registry
                .edit_automation(
                    enabled.id,
                    current.revision,
                    renamed,
                    3_000_001,
                    test_identity(),
                )
                .unwrap();
            assert_eq!(renamed.execution_revision, enabled.execution_revision);
        }
        let occurrence = history_occurrence(index);
        let claim = registry
            .claim_automation(
                enabled.id,
                enabled.execution_revision,
                Some(&occurrence),
                crate::automations::Trigger::Scheduled {
                    occurrence: occurrence.clone(),
                },
                Some(history_occurrence(index + 1)),
                false,
                occurrence.due_ms,
            )
            .unwrap();
        assert!(claim.accepted.is_none());
        assert!(claim.created);
        assert_eq!(
            claim.firing.name, original_name,
            "group retains first immutable presentation snapshot"
        );
        if let Some(id) = group {
            assert_eq!(claim.firing.id, id);
        } else {
            group = Some(claim.firing.id);
        }
        let summary = claim.firing.skipped_occurrences.unwrap();
        assert_eq!(
            (
                summary.count,
                summary.first_occurrence_ms,
                summary.last_occurrence_ms
            ),
            (index, 60_000, index * 60_000)
        );
    }
    let page = registry
        .automation_history(Some(enabled.id), None, 100)
        .unwrap();
    assert_eq!(page.firings.len(), 2);
    assert!(page.next.is_none());
    assert!(
        page.firings
            .iter()
            .any(|row| row.id == owner.firing.id && row.state == FiringState::Waiting)
    );
    assert_eq!(
        registry.automation(enabled.id).unwrap().active_firing,
        Some(owner.firing.id)
    );
    let latest = history_occurrence(100);
    let replay = registry
        .claim_automation(
            enabled.id,
            enabled.execution_revision,
            Some(&latest),
            crate::automations::Trigger::Scheduled {
                occurrence: latest.clone(),
            },
            Some(history_occurrence(101)),
            false,
            6_000_000,
        )
        .unwrap();
    assert!(!replay.created);
    assert_eq!(
        replay.firing.skipped_occurrences.as_ref().unwrap().count,
        100
    );
    let stale = history_occurrence(1);
    assert!(
        registry
            .claim_automation(
                enabled.id,
                enabled.execution_revision,
                Some(&stale),
                crate::automations::Trigger::Scheduled {
                    occurrence: stale.clone()
                },
                Some(history_occurrence(2)),
                false,
                6_000_000
            )
            .is_err()
    );
    drop(registry);
    let registry = Registry::at(&directory.path().join("model.sqlite3")).unwrap();
    assert_eq!(
        registry
            .automation_history(Some(enabled.id), None, 100)
            .unwrap(),
        page
    );
    assert_eq!(
        registry.agent_run(started.id).unwrap().state,
        crate::agent_execution::State::Waiting
    );
}

#[test]
fn exact_reason_manual_success_and_execution_revision_break_scheduled_groups() {
    use crate::automations::{SkipReason, Trigger};
    let (mut registry, _directory, definition) = fixture();
    let definition = history_precheck_definition(definition);
    let mut owners = Vec::new();
    for _ in 0..crate::automations::MAX_CONCURRENT_PRECHECKS {
        let automation = registry
            .create_automation(definition.clone(), 0, test_identity())
            .unwrap();
        owners.push(
            history_claim_manual(
                &mut registry,
                automation.id,
                automation.execution_revision,
                1,
            )
            .1,
        );
    }
    let automation = registry
        .create_automation(definition.clone(), 0, test_identity())
        .unwrap();
    let enabled = registry
        .set_automation_enabled(
            automation.id,
            automation.revision,
            Some(history_occurrence(1)),
            true,
            1,
        )
        .unwrap();
    let scheduled = |registry: &mut Registry, index| {
        let occurrence = history_occurrence(index);
        registry
            .claim_automation(
                enabled.id,
                enabled.execution_revision,
                Some(&occurrence),
                Trigger::Scheduled {
                    occurrence: occurrence.clone(),
                },
                Some(history_occurrence(index + 1)),
                false,
                occurrence.due_ms,
            )
            .unwrap()
    };
    let capacity = scheduled(&mut registry, 1);
    let repeated = scheduled(&mut registry, 2);
    assert_eq!(capacity.firing.id, repeated.firing.id);
    assert_eq!(repeated.firing.skip, Some(SkipReason::Capacity));
    assert_eq!(
        repeated.firing.skipped_occurrences.as_ref().unwrap().count,
        2
    );
    // Same reason, no intervening firing, but a changed execution revision:
    // this is an independent negative for the coalescer revision guard.
    let revised = registry
        .create_automation(definition.clone(), 0, test_identity())
        .unwrap();
    let revised = registry
        .set_automation_enabled(revised.id, 1, Some(history_occurrence(1)), true, 1)
        .unwrap();
    let due = history_occurrence(1);
    let old_revision = registry
        .claim_automation(
            revised.id,
            revised.execution_revision,
            Some(&due),
            Trigger::Scheduled {
                occurrence: due.clone(),
            },
            Some(history_occurrence(2)),
            false,
            due.due_ms,
        )
        .unwrap();
    let paused = registry
        .set_automation_enabled(revised.id, revised.revision, None, false, 60_001)
        .unwrap();
    let revised = registry
        .set_automation_enabled(
            revised.id,
            paused.revision,
            Some(history_occurrence(2)),
            true,
            60_002,
        )
        .unwrap();
    let due = history_occurrence(2);
    let new_revision = registry
        .claim_automation(
            revised.id,
            revised.execution_revision,
            Some(&due),
            Trigger::Scheduled {
                occurrence: due.clone(),
            },
            Some(history_occurrence(3)),
            false,
            due.due_ms,
        )
        .unwrap();
    assert_eq!(old_revision.firing.skip, new_revision.firing.skip);
    assert_ne!(old_revision.firing.revision, new_revision.firing.revision);
    assert_ne!(old_revision.firing.id, new_revision.firing.id);
    history_stop_precheck(&mut registry, &owners[0].firing, 150_000, true);
    let first = scheduled(&mut registry, 3);
    assert_eq!(first.firing.state, FiringState::Prechecking);
    let skipped = || crate::host::automations::precheck::Outcome::Stopped {
        state: FiringState::Skipped,
        skip: Some(SkipReason::PrecheckExit),
        diagnostic: "exit7".into(),
    };
    registry
        .finish_automation_precheck(first.firing.id, skipped(), 180_001)
        .unwrap();
    registry
        .confirm_retired_automation_precheck(first.firing.id, first.firing.revision, 180_002)
        .unwrap();
    let next = scheduled(&mut registry, 4);
    registry
        .finish_automation_precheck(next.firing.id, skipped(), 240_001)
        .unwrap();
    registry
        .confirm_retired_automation_precheck(next.firing.id, next.firing.revision, 240_002)
        .unwrap();
    let grouped = registry.automation_firing(first.firing.id).unwrap();
    assert_eq!(grouped.skipped_occurrences.as_ref().unwrap().count, 2);
    assert_ne!(grouped.id, capacity.firing.id);
    let (_, manual) = history_claim_manual(
        &mut registry,
        enabled.id,
        enabled.execution_revision,
        250_000,
    );
    history_stop_precheck(&mut registry, &manual.firing, 250_001, true);
    let after_manual = scheduled(&mut registry, 5);
    registry
        .finish_automation_precheck(after_manual.firing.id, skipped(), 300_001)
        .unwrap();
    registry
        .confirm_retired_automation_precheck(
            after_manual.firing.id,
            after_manual.firing.revision,
            300_002,
        )
        .unwrap();
    assert_ne!(after_manual.firing.id, grouped.id);
    let success = scheduled(&mut registry, 6);
    let accepted = registry
        .finish_automation_precheck(
            success.firing.id,
            crate::host::automations::precheck::Outcome::Passed,
            360_001,
        )
        .unwrap()
        .unwrap();
    history_finish_agent(&mut registry, accepted, 360_002);
    registry
        .confirm_retired_automation_precheck(success.firing.id, success.firing.revision, 360_006)
        .unwrap();
    let after_success = scheduled(&mut registry, 7);
    registry
        .finish_automation_precheck(after_success.firing.id, skipped(), 420_001)
        .unwrap();
    registry
        .confirm_retired_automation_precheck(
            after_success.firing.id,
            after_success.firing.revision,
            420_002,
        )
        .unwrap();
    assert_ne!(after_success.firing.id, after_manual.firing.id);
    assert_eq!(
        registry.automation_firing(success.firing.id).unwrap().state,
        FiringState::Succeeded
    );
    // Re-enabling changes execution revision; a same-reason skip cannot adopt
    // the preceding revision's immutable definition.
    let current = registry.automation(enabled.id).unwrap();
    let paused = registry
        .set_automation_enabled(current.id, current.revision, None, false, 430_000)
        .unwrap();
    let changed = registry
        .set_automation_enabled(
            paused.id,
            paused.revision,
            Some(history_occurrence(8)),
            true,
            430_001,
        )
        .unwrap();
    let (_, active) = history_claim_manual(
        &mut registry,
        changed.id,
        changed.execution_revision,
        440_000,
    );
    let due = history_occurrence(8);
    let changed_skip = registry
        .claim_automation(
            changed.id,
            changed.execution_revision,
            Some(&due),
            Trigger::Scheduled {
                occurrence: due.clone(),
            },
            Some(history_occurrence(9)),
            false,
            due.due_ms,
        )
        .unwrap();
    assert_eq!(changed_skip.firing.revision, changed.execution_revision);
    assert_ne!(changed_skip.firing.id, after_success.firing.id);
    assert_eq!(
        registry.automation(changed.id).unwrap().active_firing,
        Some(active.firing.id)
    );
}

#[test]
fn protected_cleanup_rows_do_not_consume_settled_history_slots() {
    let (mut registry, _directory, definition) = fixture();
    let definition = history_precheck_definition(definition);
    let automation = registry
        .create_automation(definition, 0, test_identity())
        .unwrap();
    let mut protected = Vec::new();
    for now in 1..=3 {
        let (_, claim) = history_claim_manual(
            &mut registry,
            automation.id,
            automation.execution_revision,
            now * 10,
        );
        registry
            .interrupt_automation_precheck(
                claim.firing.id,
                "cleanup unconfirmed".into(),
                now * 10 + 1,
            )
            .unwrap();
        registry
            .finish_automation_precheck(
                claim.firing.id,
                crate::host::automations::precheck::Outcome::Passed,
                now * 10 + 2,
            )
            .unwrap();
        protected.push(claim.firing.id);
    }
    for now in 4..=7 {
        let (_, claim) = history_claim_manual(
            &mut registry,
            automation.id,
            automation.execution_revision,
            now * 10,
        );
        history_stop_precheck(&mut registry, &claim.firing, now * 10 + 1, true);
    }
    let (_, active) = history_claim_manual(
        &mut registry,
        automation.id,
        automation.execution_revision,
        80,
    );
    history_prune_to(&mut registry, automation.id, 2, 100);
    assert_eq!(
        history_count(&registry, automation.id),
        6,
        "four protected rows plus two eligible slots"
    );
    for id in protected {
        assert_eq!(
            registry.automation_firing(id).unwrap().state,
            FiringState::Interrupted
        );
        assert!(registry.retired_automation_precheck(id).unwrap().is_some());
    }
    assert!(
        registry
            .retired_automation_precheck(active.firing.id)
            .unwrap()
            .is_none()
    );
    assert_eq!(
        registry.automation(automation.id).unwrap().active_firing,
        Some(active.firing.id)
    );
    registry
        .interrupt_automation_precheck(active.firing.id, "still uncertain".into(), 101)
        .unwrap();
    history_prune_to(&mut registry, automation.id, 1, u64::from(1_000_000u32));
    assert_eq!(
        history_count(&registry, automation.id),
        6,
        "blocked ownership is never silently pruned"
    );
}

#[test]
fn retention_ranks_late_completion_not_old_creation_and_keeps_manual_replay_proofs() {
    let (mut registry, directory, definition) = fixture();
    let automation = registry
        .create_automation(history_precheck_definition(definition), 0, test_identity())
        .unwrap();
    let (_, old) = history_claim_manual(&mut registry, automation.id, 1, 10);
    let (pruned, _) = history_claim_manual(&mut registry, automation.id, 1, 20);
    history_claim_manual(&mut registry, automation.id, 1, 30);
    history_stop_precheck(&mut registry, &old.firing, 10_000, true);
    let (_, latest) = history_claim_manual(&mut registry, automation.id, 1, 10_001);
    history_stop_precheck(&mut registry, &latest.firing, 10_002, true);
    history_prune_to(&mut registry, automation.id, 2, 10_003);
    assert_eq!(history_count(&registry, automation.id), 2);
    assert!(registry.automation_firing(old.firing.id).is_ok());
    assert!(registry.automation_firing(latest.firing.id).is_ok());
    assert!(
        registry
            .previous_manual_automation_firing(automation.id, 1, pruned)
            .unwrap_err()
            .contains("already recorded")
    );
    let rows = history_count(&registry, automation.id);
    assert!(
        registry
            .claim_automation(
                automation.id,
                1,
                None,
                crate::automations::Trigger::Manual { request: pruned },
                None,
                true,
                10_004
            )
            .is_err()
    );
    assert_eq!(history_count(&registry, automation.id), rows);
    registry
        .remove_automation(automation.id, 1, 10_005)
        .unwrap();
    registry
        .maintain_removed_automation_history(10_006 + history::REMOVED_TTL_MS)
        .unwrap();
    assert_eq!(history_count(&registry, automation.id), 0);
    let revisions: usize = registry
        .db
        .query_row(
            "SELECT count(*) FROM automation_revisions WHERE automation=?1",
            [automation.id.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(revisions, 0);
    drop(registry);
    let registry = Registry::at(&directory.path().join("model.sqlite3")).unwrap();
    assert!(
        registry
            .previous_manual_automation_firing(automation.id, 1, pruned)
            .unwrap_err()
            .contains("already recorded")
    );
    assert!(
        registry
            .previous_manual_automation_firing(automation.id, 2, pruned)
            .unwrap_err()
            .contains("another automation revision")
    );
}

#[test]
fn removed_history_ttl_keeps_cleanup_authority_until_exact_ack_then_idle_maintenance_deletes() {
    let (mut registry, directory, definition) = fixture();
    let automation = registry
        .create_automation(history_precheck_definition(definition), 0, test_identity())
        .unwrap();
    let (_, claim) = history_claim_manual(&mut registry, automation.id, 1, 10);
    history_stop_precheck(&mut registry, &claim.firing, 11, false);
    registry.remove_automation(automation.id, 1, 12).unwrap();
    let expired = 13 + history::REMOVED_TTL_MS;
    registry
        .maintain_removed_automation_history(expired)
        .unwrap();
    assert_eq!(
        registry
            .retired_automation_precheck(claim.firing.id)
            .unwrap(),
        Some(1)
    );
    assert_eq!(history_count(&registry, automation.id), 1);
    drop(registry);
    let mut registry = Registry::at(&directory.path().join("model.sqlite3")).unwrap();
    assert_eq!(
        registry
            .retired_automation_precheck(claim.firing.id)
            .unwrap(),
        Some(1)
    );
    // Exact caller ACK represents actual Worker retirement, not filesystem
    // absence. A removed owner is still inspectable for cleanup, not management.
    registry
        .confirm_retired_automation_precheck(claim.firing.id, 1, expired)
        .unwrap();
    assert!(registry.automation(automation.id).is_err());
    assert_eq!(history_count(&registry, automation.id), 0);
    registry
        .confirm_retired_automation_precheck(claim.firing.id, 1, expired + 1)
        .unwrap();
}

#[test]
fn expired_removed_batch_skips_protected_tombstones_and_reaches_idle_eligible_history() {
    let (mut registry, _directory, definition) = fixture();
    let definition = history_precheck_definition(definition);
    for index in 0..10 {
        let automation = registry
            .create_automation(definition.clone(), 0, test_identity())
            .unwrap();
        let (_, claim) = history_claim_manual(&mut registry, automation.id, 1, 1);
        history_stop_precheck(&mut registry, &claim.firing, 2, false);
        registry
            .remove_automation(automation.id, 1, 3 + index)
            .unwrap();
    }
    let eligible = registry
        .create_automation(definition, 20, test_identity())
        .unwrap();
    let (_, claim) = history_claim_manual(&mut registry, eligible.id, 1, 21);
    history_stop_precheck(&mut registry, &claim.firing, 22, true);
    registry.remove_automation(eligible.id, 1, 23).unwrap();
    for _ in 0..2 {
        registry
            .maintain_removed_automation_history(24 + history::REMOVED_TTL_MS)
            .unwrap();
    }
    assert_eq!(
        history_count(&registry, eligible.id),
        0,
        "protected old tombstones must not monopolize the eight-ID maintenance batch"
    );
}

#[test]
fn delayed_cleanup_cannot_coalesce_across_pruned_manual_predecessor() {
    use crate::automations::{SkipReason, Trigger};
    let (mut registry, _directory, definition) = fixture();
    let automation = registry
        .create_automation(history_precheck_definition(definition), 0, test_identity())
        .unwrap();
    let enabled = registry
        .set_automation_enabled(automation.id, 1, Some(history_occurrence(1)), true, 1)
        .unwrap();
    let due = history_occurrence(1);
    let first = registry
        .claim_automation(
            enabled.id,
            enabled.execution_revision,
            Some(&due),
            Trigger::Scheduled {
                occurrence: due.clone(),
            },
            Some(history_occurrence(2)),
            false,
            due.due_ms,
        )
        .unwrap();
    let skipped = || crate::host::automations::precheck::Outcome::Stopped {
        state: FiringState::Skipped,
        skip: Some(SkipReason::PrecheckExit),
        diagnostic: "exit7".into(),
    };
    registry
        .finish_automation_precheck(first.firing.id, skipped(), 60_001)
        .unwrap();
    let (manual_request, manual) = history_claim_manual(
        &mut registry,
        enabled.id,
        enabled.execution_revision,
        61_000,
    );
    history_stop_precheck(&mut registry, &manual.firing, 61_001, true);
    let due = history_occurrence(2);
    let second = registry
        .claim_automation(
            enabled.id,
            enabled.execution_revision,
            Some(&due),
            Trigger::Scheduled {
                occurrence: due.clone(),
            },
            Some(history_occurrence(3)),
            false,
            due.due_ms,
        )
        .unwrap();
    registry
        .finish_automation_precheck(second.firing.id, skipped(), 120_001)
        .unwrap();
    registry
        .confirm_retired_automation_precheck(second.firing.id, second.firing.revision, 120_002)
        .unwrap();
    history_prune_to(&mut registry, enabled.id, 1, 120_003);
    assert!(registry.automation_firing(manual.firing.id).is_err());
    assert!(
        registry
            .previous_manual_automation_firing(
                enabled.id,
                enabled.execution_revision,
                manual_request
            )
            .is_err()
    );
    registry
        .confirm_retired_automation_precheck(first.firing.id, first.firing.revision, 120_004)
        .unwrap();
    assert!(
        registry
            .automation_firing(second.firing.id)
            .unwrap()
            .skipped_occurrences
            .is_none(),
        "missing predecessor remains a barrier"
    );
    let due = history_occurrence(3);
    let third = registry
        .claim_automation(
            enabled.id,
            enabled.execution_revision,
            Some(&due),
            Trigger::Scheduled {
                occurrence: due.clone(),
            },
            Some(history_occurrence(4)),
            false,
            due.due_ms,
        )
        .unwrap();
    registry
        .finish_automation_precheck(third.firing.id, skipped(), 180_001)
        .unwrap();
    registry
        .confirm_retired_automation_precheck(third.firing.id, third.firing.revision, 180_002)
        .unwrap();
    let group = registry.automation_firing(second.firing.id).unwrap();
    assert_eq!(group.skipped_occurrences.as_ref().unwrap().count, 2);
    assert_eq!(
        (
            group
                .skipped_occurrences
                .as_ref()
                .unwrap()
                .first_occurrence_ms,
            group
                .skipped_occurrences
                .as_ref()
                .unwrap()
                .last_occurrence_ms
        ),
        (120_000, 180_000)
    );
}

#[test]
fn skipped_downtime_intervals_are_not_fabricated_occurrence_counts() {
    let (mut registry, _directory, definition) = fixture();
    let automation = registry
        .create_automation(definition, 0, test_identity())
        .unwrap();
    let enabled = registry
        .set_automation_enabled(automation.id, 1, Some(history_occurrence(1)), true, 1)
        .unwrap();
    let row = registry
        .skip_automation_deadlines(
            enabled.id,
            enabled.execution_revision,
            &history_occurrence(1),
            history_occurrence(100),
            99 * 60_000,
        )
        .unwrap();
    assert_eq!(
        row.skip,
        Some(crate::automations::SkipReason::MissedDeadline)
    );
    assert!(row.skipped_occurrences.is_none());
    assert_eq!(
        registry
            .automation_history(Some(enabled.id), None, 100)
            .unwrap()
            .firings
            .len(),
        1
    );
}

#[test]
fn cleanup_ack_failure_keeps_history_and_exact_retry_commits_without_resend() {
    let (mut registry, _directory, definition) = fixture();
    let automation = registry
        .create_automation(history_precheck_definition(definition), 0, test_identity())
        .unwrap();
    let (request, claim) = history_claim_manual(&mut registry, automation.id, 1, 10);
    history_stop_precheck(&mut registry, &claim.firing, 11, false);
    registry.db.execute_batch("CREATE TEMP TRIGGER refuse_history_cleanup BEFORE INSERT ON main.automation_precheck_history_cleanup BEGIN SELECT RAISE(ABORT,'fixture ACK storage failure'); END;").unwrap();
    assert!(
        registry
            .confirm_retired_automation_precheck(claim.firing.id, 1, 12)
            .is_err()
    );
    assert_eq!(
        registry
            .retired_automation_precheck(claim.firing.id)
            .unwrap(),
        Some(1)
    );
    assert_eq!(
        registry
            .previous_manual_automation_firing(automation.id, 1, request)
            .unwrap()
            .unwrap()
            .id,
        claim.firing.id
    );
    history_prune_to(&mut registry, automation.id, 1, 13);
    assert!(registry.automation_firing(claim.firing.id).is_ok());
    registry
        .db
        .execute_batch("DROP TRIGGER temp.refuse_history_cleanup")
        .unwrap();
    registry
        .confirm_retired_automation_precheck(claim.firing.id, 1, 14)
        .unwrap();
    registry
        .confirm_retired_automation_precheck(claim.firing.id, 1, 15)
        .unwrap();
    assert_eq!(history_count(&registry, automation.id), 1);
    let markers: usize = registry
        .db
        .query_row(
            "SELECT count(*) FROM automation_precheck_history_cleanup WHERE firing=?1",
            [claim.firing.id.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(markers, 1);
}

#[test]
fn published_twenty_one_upgrades_history_without_inventing_legacy_cleanup_proof() {
    let (mut registry, _directory, definition) = fixture();
    let automation = registry
        .create_automation(history_precheck_definition(definition), 0, test_identity())
        .unwrap();
    let (_, first) = history_claim_manual(&mut registry, automation.id, 1, 10);
    let (_, second) = history_claim_manual(&mut registry, automation.id, 1, 20);
    history_stop_precheck(&mut registry, &first.firing, 30, false);
    let mut legacy =
        serde_json::to_value(registry.automation_firing(first.firing.id).unwrap()).unwrap();
    legacy
        .as_object_mut()
        .unwrap()
        .remove("skipped_occurrences");
    registry
        .db
        .execute(
            "UPDATE automation_firings SET record=?2 WHERE id=?1",
            params![first.firing.id.to_string(), legacy.to_string()],
        )
        .unwrap();
    let before = registry.model().unwrap();
    let path = super::super::automation_proposals::published_fixture_from_rows(&registry, 21);
    drop(registry);
    let registry = Registry::at(&path).unwrap();
    assert_eq!(registry.model().unwrap(), before);
    assert!(
        registry
            .automation_firing(first.firing.id)
            .unwrap()
            .skipped_occurrences
            .is_none()
    );
    let previous: String = registry
        .db
        .query_row(
            "SELECT previous_firing FROM automation_firings WHERE id=?1",
            [second.firing.id.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(previous, first.firing.id.to_string());
    let clear: usize = registry
        .db
        .query_row(
            "SELECT count(*) FROM automation_precheck_history_cleanup",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(clear, 0, "migration cannot manufacture Worker retirement");
    let version: u32 = registry
        .db
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap();
    assert_eq!(version, super::super::SCHEMA_VERSION);
}

#[test]
fn retained_history_writer_refuses_future_schema_without_changes() {
    let (mut registry, _directory, definition) = fixture();
    let automation = registry
        .create_automation(history_precheck_definition(definition), 0, test_identity())
        .unwrap();
    let (_, claim) = history_claim_manual(&mut registry, automation.id, 1, 10);
    history_stop_precheck(&mut registry, &claim.firing, 11, false);
    registry
        .db
        .pragma_update(None, "user_version", super::super::SCHEMA_VERSION + 1)
        .unwrap();
    let count = history_count(&registry, automation.id);
    let before = registry.automation_firing(claim.firing.id).unwrap();
    assert!(
        registry
            .confirm_retired_automation_precheck(claim.firing.id, 1, 12)
            .is_err()
    );
    assert!(
        registry
            .maintain_removed_automation_history(12 + history::REMOVED_TTL_MS)
            .is_err()
    );
    assert!(registry.maintain_automation_history(13).is_err());
    assert!(
        registry
            .claim_automation(
                automation.id,
                1,
                None,
                crate::automations::Trigger::Manual {
                    request: Uuid::new_v4()
                },
                None,
                true,
                14
            )
            .is_err()
    );
    assert_eq!(history_count(&registry, automation.id), count);
    assert_eq!(registry.automation_firing(claim.firing.id).unwrap(), before);
    let version: u32 = registry
        .db
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap();
    assert_eq!(version, super::super::SCHEMA_VERSION + 1);
}

#[test]
fn terminal_prelaunch_failure_and_dispatch_cancellation_prune_newly_eligible_history() {
    for cancel_at_dispatch in [false, true] {
        let (mut registry, _directory, mut definition) = fixture();
        definition.precheck = None;
        let automation = registry
            .create_automation(definition, 0, test_identity())
            .unwrap();
        let (_, owner) = history_claim_manual(&mut registry, automation.id, 1, 1);
        let accepted = owner.accepted.unwrap();
        let mut oldest = None;
        for now in 2..=501 {
            let (request, skipped) = history_claim_manual(&mut registry, automation.id, 1, now);
            assert_eq!(
                skipped.firing.skip,
                Some(crate::automations::SkipReason::Overlap)
            );
            assert!(skipped.accepted.is_none());
            oldest.get_or_insert(request);
        }
        assert_eq!(
            history_count(&registry, automation.id),
            501,
            "the Accepted owner is protected outside the 500 settled slots"
        );
        if cancel_at_dispatch {
            let current = registry.automation(automation.id).unwrap();
            registry
                .set_automation_enabled(automation.id, current.revision, None, false, 600)
                .unwrap();
            assert!(!registry.claim_agent_dispatch(&accepted, 601).unwrap());
        } else {
            registry
                .fail_automation_start(
                    owner.firing.id,
                    "Fixture authority refused before dispatch".into(),
                    601,
                    true,
                )
                .unwrap();
        }
        assert_eq!(history_count(&registry, automation.id), 500);
        let firing = registry.automation_firing(owner.firing.id).unwrap();
        assert_eq!(
            firing.state,
            if cancel_at_dispatch {
                FiringState::Cancelled
            } else {
                FiringState::Failed
            }
        );
        assert!(
            registry
                .automation(automation.id)
                .unwrap()
                .active_firing
                .is_none()
        );
        assert!(
            registry
                .previous_manual_automation_firing(automation.id, 1, oldest.unwrap())
                .unwrap_err()
                .contains("already recorded")
        );
        let dispatches: usize = registry
            .db
            .query_row("SELECT count(*) FROM agent_dispatches", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(
            dispatches, 0,
            "neither terminal path may dispatch or require a provider"
        );
    }
}
