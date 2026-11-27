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

    /// Creates an automation and runs it now, which the held slot refuses.
    fn waiting(&self, name: &str) -> (crate::automations::Automation, Firing) {
        use crate::{
            agent_execution::Destination,
            automations::{Definition, Schedule, SchedulePattern},
        };
        let Destination::New(spec) = self.request.destination.clone() else {
            unreachable!()
        };
        let automation = service::create(
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
        .unwrap();
        let firing = self
            .handle
            .run_now(
                automation.id,
                automation.execution_revision,
                Uuid::new_v4(),
                true,
            )
            .unwrap();
        assert!(awaits_agent_slot(&firing), "{firing:?}");
        (automation, firing)
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
        self.handle
            .0
            .slots
            .lock()
            .unwrap()
            .get_mut(&firing.id)
            .unwrap()
            .retry = Instant::now();
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
    let (_, firing) = fixture.waiting("Slot wait fixture");
    let tick = || fixture.handle.tick(&fixture.host).unwrap();

    // The wake after the refusal starts the firing's timer, and sleeps no
    // longer than its first delay.
    let delay = tick().unwrap();
    assert!(delay <= SlotBackoff::FIRST, "{delay:?}");
    assert_eq!(fixture.backoff(&firing).unwrap().refusals, 1);

    // Other wakes while the slot is held find none free and leave the timer.
    tick();
    assert_eq!(fixture.backoff(&firing).unwrap().refusals, 1);

    // Its timer asks regardless, and a refusal doubles the delay.
    fixture.elapse(&firing);
    let delay = tick().unwrap();
    assert!(delay <= SlotBackoff::FIRST * 2, "{delay:?}");
    assert_eq!(fixture.backoff(&firing).unwrap().refusals, 2);
    assert_eq!(fixture.saved(&firing), firing);

    // The slot frees. The next wake starts the run without waiting out the
    // timer, and launches it.
    fixture.release();
    tick();
    assert!(fixture.backoff(&firing).is_none());
    let started = fixture.saved(&firing);
    let run = started
        .run
        .expect("the freed slot starts the waiting firing");
    assert_eq!(
        started.diagnostic.as_deref(),
        Some("Waited for an agent slot before starting.")
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    let run = loop {
        let run = fixture.registry.lock().unwrap().agent_run(run).unwrap();
        if run.state.is_terminal() {
            break run;
        }
        assert!(Instant::now() < deadline, "run never settled: {run:?}");
        std::thread::sleep(Duration::from_millis(10));
    };
    assert_eq!(run.state, State::Succeeded, "{run:?}");
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
    let (resumed, skipped) = fixture.waiting("Resumed while waiting");
    let (cancelled_owner, cancelled) = fixture.waiting("Cancelled while waiting");
    let tick = || fixture.handle.tick(&fixture.host).unwrap();
    tick();
    assert!(fixture.backoff(&skipped).is_some() && fixture.backoff(&cancelled).is_some());

    // Resuming the schedule supersedes the run asked for before it: the next
    // wake ends the wait, with the slot still held and the timer not yet due.
    service::enabled(&fixture.host, resumed.id, resumed.revision, true, true).unwrap();
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
