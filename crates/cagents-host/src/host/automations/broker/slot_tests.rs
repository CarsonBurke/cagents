//! A firing refused for the agent concurrency limit waits for the scheduler,
//! which starts it through a real fixture provider on the first wake that
//! finds a slot free, and asks regardless on a backoff.
use super::*;

#[test]
fn slot_backoff_doubles_from_two_seconds_to_a_minute() {
    let delays: Vec<_> = (1..=8)
        .map(|refusals| SlotBackoff::delay(refusals).as_secs())
        .collect();
    assert_eq!(delays, [2, 4, 8, 16, 32, 60, 60, 60]);
    assert_eq!(SlotBackoff::delay(u32::MAX), SlotBackoff::LONGEST);
    let now = Instant::now();
    let first = SlotBackoff::refused(None, now);
    let second = SlotBackoff::refused(Some(first), now);
    assert_eq!((first.refusals, first.retry), (1, now + SlotBackoff::FIRST));
    assert_eq!(
        (second.refusals, second.retry),
        (2, now + SlotBackoff::FIRST * 2)
    );
}

/// A host whose only agent slot is held by someone else's accepted run, and
/// a scheduler for it that the test wakes by hand.
struct SlotFixture {
    host: SharedHost,
    registry: crate::host::agent_execution::SharedRegistry,
    handle: Handle,
    request: crate::agent_execution::Submit,
    holder: crate::agent_execution::Accepted,
}

impl SlotFixture {
    fn new() -> Self {
        use crate::{
            agent_execution::{
                Origin,
                concurrency::{Limits, Setting},
            },
            host::agent_execution::{self, tests::fixture},
        };
        let (host, request, path) = fixture();
        let registry = agent_execution::registry(&host).unwrap();
        let handle = Handle(Arc::new(Inner {
            host: Arc::downgrade(&host),
            registry: registry.clone(),
            root: path.parent().unwrap().join("prechecks"),
            wake: Wake::default(),
            workers: Mutex::default(),
            launching: Mutex::default(),
            cleanup: Mutex::new(None),
            attention: Mutex::default(),
            notifications: Mutex::default(),
            slots: Mutex::default(),
        }));
        registry
            .lock()
            .unwrap()
            .use_concurrency(Setting::fixed(Limits {
                per_caller: 1,
                total: 1,
            }));
        let holder = registry
            .lock()
            .unwrap()
            .submit_agent(&Origin::User, &request, 50)
            .unwrap();
        Self {
            host,
            registry,
            handle,
            request,
            holder,
        }
    }

    fn create(&self, name: &str) -> crate::automations::Automation {
        use crate::{
            agent_execution::Destination,
            automations::{Definition, Schedule, SchedulePattern},
        };
        let Destination::New(spec) = self.request.destination.clone() else {
            unreachable!()
        };
        service::create(
            &self.host,
            Definition {
                name: name.into(),
                prompt: self.request.prompt.clone(),
                agent: spec,
                schedule: Schedule {
                    pattern: SchedulePattern::Hourly { minute: 0 },
                    timezone: "UTC".into(),
                },
                session_mode: SessionMode::Fresh,
                precheck: None,
            },
        )
        .unwrap()
    }

    fn claim(&self, automation: &crate::automations::Automation, now: u64) -> Claimed {
        self.handle
            .claim_manual(
                automation.id,
                automation.execution_revision,
                Uuid::new_v4(),
                automation.next.clone(),
                true,
                now,
            )
            .unwrap()
            .0
    }

    /// Creates an automation whose manual claim is refused by the held slot.
    fn waiting(&self, name: &str, now: u64) -> (crate::automations::Automation, Firing) {
        let automation = self.create(name);
        let firing = self.claim(&automation, now).firing;
        assert!(awaits_agent_slot(&firing), "{firing:?}");
        (automation, firing)
    }

    fn execute_sql(&self, sql: &str) {
        self.registry
            .lock()
            .unwrap()
            .fixture_connection()
            .execute_batch(sql)
            .unwrap();
    }

    fn saved(&self, firing: &Firing) -> Firing {
        self.registry
            .lock()
            .unwrap()
            .automation_firing(firing.id)
            .unwrap()
    }

    fn backoff(&self, firing: &Firing) -> Option<SlotBackoff> {
        self.handle.0.slots.lock().unwrap().get(&firing.id).copied()
    }

    /// Brings the firing's timer due.
    fn elapse(&self, firing: &Firing) {
        self.set_timer(firing, Instant::now());
    }

    /// Puts the firing's timer out of the test's reach, so that only a wake
    /// that finds something changed can act on it.
    fn postpone(&self, firing: &Firing) {
        self.set_timer(firing, Instant::now() + Duration::from_secs(3600));
    }

    fn set_timer(&self, firing: &Firing, retry: Instant) {
        self.handle
            .0
            .slots
            .lock()
            .unwrap()
            .get_mut(&firing.id)
            .unwrap()
            .retry = retry;
    }

    /// Waits for the run a started firing was given to end.
    fn settled(&self, firing: &Firing) -> crate::agent_execution::Run {
        let run = self
            .saved(firing)
            .run
            .expect("the freed slot starts the waiting firing");
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let run = self.registry.lock().unwrap().agent_run(run).unwrap();
            if run.state.is_terminal() {
                return run;
            }
            assert!(Instant::now() < deadline, "run never settled: {run:?}");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Ends the run holding the slot.
    fn release(&self) {
        use crate::agent_execution::{State, Update};
        let mut registry = self.registry.lock().unwrap();
        let started = registry
            .update_agent_run(
                self.holder.run.id,
                self.holder.run.revision,
                Update::Started {
                    provider_key: "holder-turn".into(),
                },
                60,
            )
            .unwrap();
        registry
            .update_agent_run(
                self.holder.run.id,
                started.revision,
                Update::Finished {
                    state: State::Succeeded,
                    result: Default::default(),
                },
                70,
            )
            .unwrap();
    }
}

#[test]
fn scheduler_starts_a_waiting_firing_on_the_wake_that_finds_a_slot_free() {
    use crate::{agent_execution::State, host::agent_execution::tests::isolated};
    if isolated(
        "host::automations::broker::slot_tests::scheduler_starts_a_waiting_firing_on_the_wake_that_finds_a_slot_free",
    )
    .is_some()
    {
        return;
    }
    let fixture = SlotFixture::new();
    let now = service::now().unwrap();
    let (_, firing) = fixture.waiting("Slot wait fixture", now);
    let (_, later) = fixture.waiting("Later slot wait fixture", now + 1);
    let tick = || fixture.handle.tick(&fixture.host).unwrap();

    // The wake after the refusal starts the firing's timer, and sleeps no
    // longer than its first delay.
    let delay = tick().unwrap();
    assert!(delay <= SlotBackoff::FIRST, "{delay:?}");
    assert_eq!(fixture.backoff(&firing).unwrap().refusals, 1);

    // Other wakes while the slot is held find none free and leave the timer.
    fixture.postpone(&firing);
    tick();
    assert_eq!(fixture.backoff(&firing).unwrap().refusals, 1);

    // Its timer asks regardless, and a refusal doubles the delay.
    fixture.elapse(&firing);
    let delay = tick().unwrap();
    assert!(delay <= SlotBackoff::FIRST * 2, "{delay:?}");
    assert_eq!(fixture.backoff(&firing).unwrap().refusals, 2);
    assert_eq!(fixture.saved(&firing), firing);

    // The slot frees. The next wake starts the run without waiting out the
    // timer and launches it before the later firing.
    fixture.postpone(&firing);
    fixture.postpone(&later);
    fixture.release();
    tick();
    assert!(fixture.backoff(&firing).is_none());
    assert_eq!(
        fixture.saved(&firing).diagnostic.as_deref(),
        Some("Waited for an agent slot before starting.")
    );
    let run = fixture.settled(&firing);
    assert_eq!(run.state, State::Succeeded, "{run:?}");

    // The launch can finish within the same tick. The insertion order
    // records who received the slot first regardless of provider timing.
    tick();
    assert!(fixture.backoff(&later).is_none());
    let second = fixture.settled(&later);
    assert_eq!(second.state, State::Succeeded, "{second:?}");
    let registry = fixture.registry.lock().unwrap();
    let connection = registry.fixture_connection();
    let rowid = |id: crate::model::RunId| -> i64 {
        connection
            .query_row(
                "SELECT rowid FROM agent_runs WHERE id = ?1",
                [id.to_string()],
                |row| row.get(0),
            )
            .unwrap()
    };
    assert!(rowid(run.id) < rowid(second.id));
}

#[test]
fn scheduler_ends_a_wait_for_a_slot_on_the_wake_after_a_resume_or_a_cancel() {
    use crate::{automations::SkipReason, host::agent_execution::tests::isolated};
    if isolated(
        "host::automations::broker::slot_tests::scheduler_ends_a_wait_for_a_slot_on_the_wake_after_a_resume_or_a_cancel",
    )
    .is_some()
    {
        return;
    }
    let fixture = SlotFixture::new();
    let now = service::now().unwrap();
    let (resumed, skipped) = fixture.waiting("Resumed while waiting", now);
    let (cancelled_owner, cancelled) = fixture.waiting("Cancelled while waiting", now + 1);
    let tick = || fixture.handle.tick(&fixture.host).unwrap();
    tick();
    fixture.postpone(&skipped);
    fixture.postpone(&cancelled);
    fixture
        .handle
        .0
        .slots
        .lock()
        .unwrap()
        .get_mut(&skipped.id)
        .unwrap()
        .failed = true;

    // Resuming the schedule supersedes the run asked for before it: the next
    // wake ends the wait, with the slot still held and the timer not yet due.
    fixture
        .registry
        .lock()
        .unwrap()
        .set_automation_enabled(
            resumed.id,
            resumed.revision,
            Some(crate::automations::Occurrence {
                due_ms: now + 86_400_000,
                local: "fixture".into(),
            }),
            true,
            now,
        )
        .unwrap();
    tick();
    let saved = fixture.saved(&skipped);
    assert_eq!(
        (saved.state, saved.skip),
        (FiringState::Skipped, Some(SkipReason::DefinitionChanged))
    );
    assert!(fixture.backoff(&skipped).is_none());

    // Cancelling ends the other at once, and the scheduler forgets its timer.
    fixture
        .handle
        .cancel(cancelled_owner.id, cancelled.id)
        .unwrap();
    assert_eq!(fixture.saved(&cancelled).state, FiringState::Cancelled);
    tick();
    assert!(fixture.backoff(&cancelled).is_none());

    // Neither holds its automation, and the freed slot starts nothing.
    fixture.release();
    tick();
    let registry = fixture.registry.lock().unwrap();
    for automation in [resumed.id, cancelled_owner.id] {
        assert_eq!(registry.automation(automation).unwrap().active_firing, None);
    }
    for firing in [&skipped, &cancelled] {
        assert_eq!(registry.automation_firing(firing.id).unwrap().run, None);
    }
    assert!(registry.active_automation_firings().unwrap().is_empty());
}

#[test]
fn an_update_defers_manual_and_scheduled_automation_admission() {
    use crate::{
        agent_execution::Authority,
        automations::{Request, Response, SchedulePattern},
        host::{agent_execution::tests::isolated, client::process_request},
        terminal_ipc::{ClientRequest, ProtocolErrorCode, ServerResponse},
    };

    if isolated(
        "host::automations::broker::slot_tests::an_update_defers_manual_and_scheduled_automation_admission",
    )
    .is_some()
    {
        return;
    }
    let fixture = SlotFixture::new();
    fixture.host.lock().unwrap().automations = Some(fixture.handle.clone());
    let manual = fixture.create("Manual after update");
    let scheduled = fixture.create("Scheduled after update");
    let mut definition = scheduled.definition;
    definition.schedule.pattern = SchedulePattern::Cron {
        expression: "* * * * *".into(),
    };
    let scheduled =
        service::edit(&fixture.host, scheduled.id, scheduled.revision, definition).unwrap();
    let now = service::now().unwrap();
    let occurrence = Compiled::new(&scheduled.definition.schedule)
        .unwrap()
        .next_after(now - 60_000)
        .unwrap();
    let scheduled = fixture
        .registry
        .lock()
        .unwrap()
        .set_automation_enabled(
            scheduled.id,
            scheduled.revision,
            Some(occurrence.clone()),
            true,
            now,
        )
        .unwrap();
    let request = Uuid::new_v4();
    let run_now = || {
        process_request(
            &fixture.host,
            0,
            ClientRequest::Automation {
                authority: Authority::default(),
                operation: Request::RunNow {
                    id: manual.id,
                    execution_revision: manual.execution_revision,
                    request,
                    confirmed: true,
                },
            },
        )
    };

    let update = crate::instance::UiUpdateGuard::acquire().unwrap();
    let error = run_now().expect_err("manual automation was admitted during the update");
    assert_eq!(error.code, ProtocolErrorCode::TerminalUnavailable);
    assert_eq!(error.message, agent_execution::NEW_WORK_HELD);
    assert!(error.retryable);
    assert_eq!(
        fixture.handle.tick(&fixture.host).unwrap(),
        Some(Duration::from_millis(250))
    );
    {
        let registry = fixture.registry.lock().unwrap();
        assert!(registry.active_automation_firings().unwrap().is_empty());
        assert_eq!(
            registry.automation(scheduled.id).unwrap().next,
            Some(occurrence.clone())
        );
        assert!(
            registry
                .previous_manual_automation_firing(manual.id, manual.execution_revision, request)
                .unwrap()
                .is_none()
        );
    }

    drop(update);
    // The held agent slot keeps execution offline while still exercising the
    // durable claims and the ordinary advance of the schedule.
    fixture.handle.tick(&fixture.host).unwrap();
    let after = fixture
        .registry
        .lock()
        .unwrap()
        .automation(scheduled.id)
        .unwrap();
    assert!(
        after
            .next
            .is_some_and(|next| next.due_ms > occurrence.due_ms)
    );
    let ServerResponse::Automation(Response::Firing(accepted)) = run_now().unwrap().0 else {
        panic!("the same manual request was not accepted after the update");
    };
    assert_eq!(accepted.trigger, Trigger::Manual { request });
    let update = crate::instance::UiUpdateGuard::acquire().unwrap();
    let ServerResponse::Automation(Response::Firing(replayed)) = run_now().unwrap().0 else {
        panic!("the existing manual receipt was not returned during the update");
    };
    assert_eq!(replayed.id, accepted.id);
    drop(update);
}

#[test]
fn claim_time_dispatch_refusal_fails_the_firing_and_frees_the_slot() {
    use crate::{agent_execution::State, host::agent_execution::tests::isolated};
    if isolated(
        "host::automations::broker::slot_tests::claim_time_dispatch_refusal_fails_the_firing_and_frees_the_slot",
    )
    .is_some()
    {
        return;
    }
    let fixture = SlotFixture::new();
    fixture.release();
    let automation = fixture.create("Dispatch refused");
    fixture.execute_sql(
        "CREATE TEMP TRIGGER refuse_dispatch BEFORE INSERT ON main.agent_dispatches
         BEGIN SELECT RAISE(ABORT, 'fixture dispatch refusal'); END;",
    );
    let firing = fixture
        .handle
        .run_now(
            automation.id,
            automation.execution_revision,
            Uuid::new_v4(),
            true,
        )
        .unwrap();
    assert_eq!(firing.state, FiringState::Failed);
    assert_eq!(
        firing.diagnostic.as_deref(),
        Some("fixture dispatch refusal")
    );
    let registry = fixture.registry.lock().unwrap();
    let detail = registry.automation_run_detail(firing.id).unwrap();
    assert_eq!(detail.run.unwrap().state, State::Failed);
    assert!(detail.agent.unwrap().closed);
    assert_eq!(
        registry.automation(automation.id).unwrap().active_firing,
        None
    );
    drop(registry);

    fixture.execute_sql("DROP TRIGGER refuse_dispatch");
    let later = fixture.claim(&automation, service::now().unwrap());
    assert!(
        later.accepted.is_some(),
        "another firing can claim the freed slot"
    );
}

#[test]
fn dispatch_error_after_claim_leaves_the_run_to_execution() {
    use crate::{agent_execution::State, host::agent_execution::tests::isolated};
    if isolated(
        "host::automations::broker::slot_tests::dispatch_error_after_claim_leaves_the_run_to_execution",
    )
    .is_some()
    {
        return;
    }
    let fixture = SlotFixture::new();
    fixture.release();
    let automation = fixture.create("Dispatch claimed");
    let now = service::now().unwrap();
    let claim = fixture.claim(&automation, now);
    let accepted = claim.accepted.unwrap();
    assert!(
        fixture
            .registry
            .lock()
            .unwrap()
            .claim_agent_dispatch(&accepted, now)
            .unwrap()
    );
    let error = "fixture error after dispatch claim".to_owned();
    assert_eq!(
        fixture
            .handle
            .settle_dispatch(claim.firing.id, Err(error.clone()), now),
        Err(error),
    );
    let registry = fixture.registry.lock().unwrap();
    assert_eq!(
        registry.agent_run(accepted.run.id).unwrap().state,
        State::Accepted
    );
    assert_eq!(
        registry.automation_firing(claim.firing.id).unwrap(),
        claim.firing
    );
    assert_eq!(
        registry.automation(automation.id).unwrap().active_firing,
        Some(claim.firing.id),
    );
}

#[test]
fn slot_retry_error_backs_off_without_blocking_the_next_firing() {
    use crate::{agent_execution::State, host::agent_execution::tests::isolated};
    if isolated(
        "host::automations::broker::slot_tests::slot_retry_error_backs_off_without_blocking_the_next_firing",
    )
    .is_some()
    {
        return;
    }
    let fixture = SlotFixture::new();
    let now = service::now().unwrap();
    let (_, first) = fixture.waiting("Retry error", now);
    let (_, later) = fixture.waiting("Healthy waiter", now + 1);
    let retry = || fixture.handle.retry_slots(&fixture.host, now).unwrap();
    retry();
    fixture.postpone(&first);
    fixture.postpone(&later);
    fixture.execute_sql(&format!(
        "CREATE TEMP TRIGGER fail_first_waiter BEFORE UPDATE ON main.automation_firings
         WHEN OLD.id = '{}' AND NEW.run_id IS NOT NULL
         BEGIN SELECT RAISE(ABORT, 'fixture retry error'); END;",
        first.id,
    ));

    fixture.release();
    retry();
    assert_eq!(fixture.saved(&first), first);
    let backoff = fixture.backoff(&first).unwrap();
    assert!(backoff.failed);
    assert_eq!(
        backoff.refusals, 2,
        "an error on a capacity wake advances the timer"
    );
    assert_eq!(fixture.settled(&later).state, State::Succeeded);

    fixture.postpone(&first);
    let retry_at = fixture.backoff(&first).unwrap().retry;
    retry();
    assert_eq!(fixture.backoff(&first).unwrap().retry, retry_at);

    // The timer can try again, even when no capacity wake arrives.
    fixture.execute_sql("DROP TRIGGER fail_first_waiter");
    fixture.elapse(&first);
    retry();
    assert!(fixture.backoff(&first).is_none());
    assert_eq!(fixture.settled(&first).state, State::Succeeded);
}
