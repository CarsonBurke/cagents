// Included in the existing Registry automation tests; no provider or shell runs.

/// Lets one agent run at a time and starts someone else's, which holds the
/// only slot until `slot_wait_release` ends it.
fn slot_wait_hold(
    registry: &mut Registry,
    definition: &Definition,
) -> crate::agent_execution::Accepted {
    use crate::agent_execution::{
        Destination, Origin, Submit,
        concurrency::{Limits, Setting},
    };
    registry.use_concurrency(Setting::fixed(Limits {
        per_caller: 1,
        total: 1,
    }));
    registry
        .submit_agent(
            &Origin::User,
            &Submit {
                name: None,
                destination: Destination::New(definition.agent.clone()),
                prompt: "Hold the only slot".into(),
                idempotency_key: "slot-holder".into(),
            },
            50,
        )
        .unwrap()
}

fn slot_wait_release(registry: &mut Registry, holder: &crate::agent_execution::Accepted) {
    slot_wait_finish(registry, holder, crate::agent_execution::State::Succeeded);
}

fn slot_wait_finish(
    registry: &mut Registry,
    accepted: &crate::agent_execution::Accepted,
    state: crate::agent_execution::State,
) {
    use crate::agent_execution::Update;
    let started = registry
        .update_agent_run(
            accepted.run.id,
            accepted.run.revision,
            Update::Started {
                provider_key: format!("turn-{}", accepted.run.id),
            },
            60,
        )
        .unwrap();
    registry
        .update_agent_run(
            accepted.run.id,
            started.revision,
            Update::Finished {
                state,
                result: Default::default(),
            },
            70,
        )
        .unwrap();
}

fn slot_wait_claim(registry: &mut Registry, automation: Uuid, now_ms: u64) -> Claimed {
    registry
        .claim_automation(
            automation,
            1,
            None,
            crate::automations::Trigger::Manual {
                request: Uuid::new_v4(),
            },
            None,
            true,
            now_ms,
        )
        .unwrap()
}

fn slot_wait_rows(registry: &Registry, table: &str) -> usize {
    registry
        .db
        .query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
            row.get(0)
        })
        .unwrap()
}

#[test]
fn a_firing_refused_for_the_agent_limit_waits_and_starts_once_a_slot_frees() {
    let (mut registry, _directory, definition) = fixture();
    let holder = slot_wait_hold(&mut registry, &definition);
    let automation = registry
        .create_automation(definition, 100, test_identity())
        .unwrap();

    let claim = slot_wait_claim(&mut registry, automation.id, 200);
    assert!(claim.created && claim.accepted.is_none());
    let waiting = claim.firing;
    assert!(awaits_agent_slot(&waiting));
    assert_eq!(waiting.state, FiringState::Accepted);
    assert_eq!((waiting.agent, waiting.run), (None, None));
    assert_eq!(waiting.finished_ms, None);
    assert_eq!(
        waiting.diagnostic.as_deref(),
        Some(
            "Waiting for an agent slot: 1 of the 1 agent runs allowed at once are active or unconfirmed. This run starts once one finishes or its agent is closed; an interrupted run keeps its slot until its agent is closed."
        )
    );
    let owner = registry.automation(automation.id).unwrap();
    assert_eq!(owner.active_firing, Some(waiting.id));
    assert_eq!((owner.reuse_agent, owner.channel), (None, None));
    // The refused submission left nothing of its own behind.
    for table in ["agents", "agent_runs", "channels"] {
        assert_eq!(slot_wait_rows(&registry, table), 1, "{table}");
    }

    // Asking again while the slot is held changes nothing.
    assert!(!registry.automation_slot_retry_due(waiting.id).unwrap());
    assert!(matches!(
        registry.retry_automation_slot(waiting.id, 300).unwrap(),
        SlotRetry::Waiting
    ));
    assert_eq!(registry.automation_firing(waiting.id).unwrap(), waiting);
    assert_eq!(registry.automation(automation.id).unwrap(), owner);

    slot_wait_release(&mut registry, &holder);
    assert!(registry.automation_slot_retry_due(waiting.id).unwrap());
    let SlotRetry::Started(accepted) = registry.retry_automation_slot(waiting.id, 400).unwrap()
    else {
        panic!("the freed slot starts the waiting firing");
    };
    let started = registry.automation_firing(waiting.id).unwrap();
    assert_eq!(started.state, FiringState::Accepted);
    assert_eq!(started.created_ms, 200);
    assert_eq!(started.agent, Some(accepted.agent.id));
    assert_eq!(started.run, Some(accepted.run.id));
    assert_eq!(started.diagnostic.as_deref(), Some(SLOT_WAITED));
    assert_eq!(
        registry.automation(automation.id).unwrap().reuse_agent,
        Some(accepted.agent.id)
    );
    // One firing records the whole of it, and it no longer asks for a slot.
    assert_eq!(slot_wait_rows(&registry, "automation_firings"), 1);
    assert!(registry.automation_slot_retry_due(waiting.id).unwrap());
    assert!(matches!(
        registry.retry_automation_slot(waiting.id, 410).unwrap(),
        SlotRetry::Settled
    ));

    // A run that ends with nothing to say keeps the note that it waited.
    slot_wait_finish(
        &mut registry,
        &accepted,
        crate::agent_execution::State::Succeeded,
    );
    let finished = registry
        .reconcile_automation_run(accepted.run.id, 500)
        .unwrap()
        .unwrap();
    assert_eq!(finished.state, FiringState::Succeeded);
    assert_eq!(finished.diagnostic.as_deref(), Some(SLOT_WAITED));
}

#[test]
fn a_run_that_waited_for_a_slot_reports_its_own_failure() {
    use crate::agent_execution::{State, Update};
    let (mut registry, _directory, definition) = fixture();
    let holder = slot_wait_hold(&mut registry, &definition);
    let automation = registry
        .create_automation(definition, 100, test_identity())
        .unwrap();
    let waiting = slot_wait_claim(&mut registry, automation.id, 200).firing;
    slot_wait_release(&mut registry, &holder);
    let SlotRetry::Started(accepted) = registry.retry_automation_slot(waiting.id, 300).unwrap()
    else {
        panic!("the freed slot starts the waiting firing");
    };
    let result = crate::agent_execution::ResultSummary {
        diagnostic: Some("The provider stopped".into()),
        ..Default::default()
    };
    registry
        .update_agent_run(
            accepted.run.id,
            accepted.run.revision,
            Update::Finished {
                state: State::Failed,
                result,
            },
            310,
        )
        .unwrap();
    let failed = registry
        .reconcile_automation_run(accepted.run.id, 320)
        .unwrap()
        .unwrap();
    assert_eq!(failed.state, FiringState::Failed);
    assert_eq!(failed.diagnostic.as_deref(), Some("The provider stopped"));
}

#[test]
fn a_firing_waiting_for_a_slot_owns_its_automation_but_no_precheck_slot() {
    let (mut registry, _directory, definition) = fixture();
    let holder = slot_wait_hold(&mut registry, &definition);
    let mut checked = definition.clone();
    checked.precheck = Some(crate::automations::Precheck {
        command: "true".into(),
        timeout_seconds: 5,
    });
    let automation = registry
        .create_automation(checked.clone(), 100, test_identity())
        .unwrap();

    // The limit is met after the precheck passes; the firing waits then too.
    let claim = slot_wait_claim(&mut registry, automation.id, 200);
    assert_eq!(claim.firing.state, FiringState::Prechecking);
    assert!(
        registry
            .finish_automation_precheck(
                claim.firing.id,
                crate::host::automations::precheck::Outcome::Passed,
                210,
            )
            .unwrap()
            .is_none()
    );
    let waiting = registry.automation_firing(claim.firing.id).unwrap();
    assert!(awaits_agent_slot(&waiting));
    assert_eq!(
        registry.automation(automation.id).unwrap().active_firing,
        Some(waiting.id)
    );

    // A later occurrence is skipped while this one waits, as for any run.
    let overlap = slot_wait_claim(&mut registry, automation.id, 220).firing;
    assert_eq!(overlap.state, FiringState::Skipped);
    assert_eq!(overlap.skip, Some(crate::automations::SkipReason::Overlap));

    // It holds no precheck slot, and has nothing an update must wait for.
    assert!(registry.automation_update_blockers().unwrap().is_empty());
    for _ in 0..crate::automations::MAX_CONCURRENT_PRECHECKS {
        let other = registry
            .create_automation(checked.clone(), 100, test_identity())
            .unwrap();
        assert_eq!(
            slot_wait_claim(&mut registry, other.id, 230).firing.state,
            FiringState::Prechecking
        );
    }
    assert_eq!(
        registry.automation_update_blockers().unwrap().len(),
        crate::automations::MAX_CONCURRENT_PRECHECKS
    );

    slot_wait_release(&mut registry, &holder);
    assert!(matches!(
        registry.retry_automation_slot(waiting.id, 300).unwrap(),
        SlotRetry::Started(_)
    ));
}

#[test]
fn cancelling_a_firing_waiting_for_a_slot_ends_it_at_once() {
    let (mut registry, _directory, definition) = fixture();
    let holder = slot_wait_hold(&mut registry, &definition);
    let automation = registry
        .create_automation(definition, 100, test_identity())
        .unwrap();
    let waiting = slot_wait_claim(&mut registry, automation.id, 200).firing;

    let cancelled = registry
        .cancel_unstarted_automation_firing(automation.id, waiting.id, 250)
        .unwrap();
    assert_eq!(cancelled.state, FiringState::Cancelled);
    assert_eq!(cancelled.finished_ms, Some(250));
    assert_eq!(
        cancelled.diagnostic.as_deref(),
        Some("Cancelled while waiting for an agent slot")
    );
    assert_eq!(registry.automation_firing(waiting.id).unwrap(), cancelled);
    assert_eq!(
        registry.automation(automation.id).unwrap().active_firing,
        None
    );

    // A slot freed later starts nothing for it.
    slot_wait_release(&mut registry, &holder);
    assert!(matches!(
        registry.retry_automation_slot(waiting.id, 300).unwrap(),
        SlotRetry::Settled
    ));
    assert_eq!(slot_wait_rows(&registry, "agent_runs"), 1);
    assert!(
        registry
            .cancel_unstarted_automation_firing(automation.id, waiting.id, 310)
            .is_err()
    );
}

#[test]
fn disabling_an_automation_skips_its_firing_waiting_for_a_slot() {
    let (mut registry, _directory, definition) = fixture();
    let holder = slot_wait_hold(&mut registry, &definition);
    let automation = registry
        .create_automation(definition, 100, test_identity())
        .unwrap();
    let waiting = slot_wait_claim(&mut registry, automation.id, 200).firing;
    registry
        .set_automation_enabled(automation.id, automation.revision, None, false, 250)
        .unwrap();

    // The scheduler need not wait for a slot to end it.
    assert!(registry.automation_slot_retry_due(waiting.id).unwrap());

    // Even with a slot free, the paused automation's firing does not start.
    slot_wait_release(&mut registry, &holder);
    assert!(matches!(
        registry.retry_automation_slot(waiting.id, 300).unwrap(),
        SlotRetry::Settled
    ));
    let skipped = registry.automation_firing(waiting.id).unwrap();
    assert_eq!(skipped.state, FiringState::Skipped);
    assert_eq!(
        skipped.skip,
        Some(crate::automations::SkipReason::DefinitionChanged)
    );
    assert_eq!(skipped.finished_ms, Some(300));
    assert_eq!(
        skipped.diagnostic.as_deref(),
        Some(
            "The automation was paused or resumed, or its project was closed, while this run waited for an agent slot"
        )
    );
    assert_eq!(
        registry.automation(automation.id).unwrap().active_firing,
        None
    );
    assert_eq!(slot_wait_rows(&registry, "agent_runs"), 1);
}

#[test]
fn a_scheduled_firing_waiting_for_a_slot_keeps_its_schedule_and_skips_what_overlaps() {
    use crate::automations::{Occurrence, SkipReason, Trigger};
    let (mut registry, _directory, definition) = fixture();
    let holder = slot_wait_hold(&mut registry, &definition);
    let automation = registry
        .create_automation(definition, 100, test_identity())
        .unwrap();
    let occurrence = |due_ms: u64, day: u8| Occurrence {
        due_ms,
        local: format!("2026-09-{day}T09:00"),
    };
    let (first, second, third) = (occurrence(200, 27), occurrence(400, 28), occurrence(600, 29));
    let enabled = registry
        .set_automation_enabled(automation.id, 1, Some(first.clone()), true, 150)
        .unwrap();
    let mut claim = |expected: &Occurrence, next: &Occurrence, now_ms: u64| {
        registry
            .claim_automation(
                automation.id,
                enabled.execution_revision,
                Some(expected),
                Trigger::Scheduled {
                    occurrence: expected.clone(),
                },
                Some(next.clone()),
                false,
                now_ms,
            )
            .unwrap()
    };

    let waiting = claim(&first, &second, 200).firing;
    assert!(awaits_agent_slot(&waiting));
    // The occurrence that passes while it waits is recorded as overlapping.
    let overlap = claim(&second, &third, 400).firing;
    assert_eq!(overlap.state, FiringState::Skipped);
    assert_eq!(overlap.skip, Some(SkipReason::Overlap));

    // The schedule was neither paused nor blocked by the refusal.
    let owner = registry.automation(automation.id).unwrap();
    assert!(owner.enabled);
    assert!(owner.blocked.is_none());
    assert_eq!(owner.next, Some(third));
    assert_eq!(owner.active_firing, Some(waiting.id));

    slot_wait_release(&mut registry, &holder);
    assert!(matches!(
        registry.retry_automation_slot(waiting.id, 450).unwrap(),
        SlotRetry::Started(_)
    ));
}

#[test]
fn a_waiting_firing_says_which_limit_it_waits_on() {
    use crate::agent_execution::concurrency::{LimitReached, Scope};
    let refusal = |scope| LimitReached {
        scope,
        running: 2,
        limit: 2,
        yours: 2,
        holders: Vec::new(),
    };
    assert!(
        slot_wait_diagnostic(&refusal(Scope::Caller)).starts_with(
            "Waiting for an agent slot: 2 of the 2 agent runs this automation may have at once are active or unconfirmed."
        )
    );
    assert!(
        slot_wait_diagnostic(&refusal(Scope::Global)).starts_with(
            "Waiting for an agent slot: 2 of the 2 agent runs allowed at once are active or unconfirmed."
        )
    );
}
