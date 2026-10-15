//! Daemon-owned agent observation. Clients consume semantic events and the
//! reconnect snapshot; their visibility and connection count do not schedule I/O.
use std::{
    collections::{BTreeMap, BTreeSet},
    io,
    sync::{Arc, Condvar, Mutex, Weak},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use super::state::{HostLock, SharedHost};
use crate::{
    agent::Provider,
    model::{HostSessionProof, TerminalId, agent_recovery::AgentProcess},
    terminal::TerminalHandle,
};

const WORKERS: usize = 2;
const INTERVAL: Duration = Duration::from_secs(2);
const MAX_BACKOFF: Duration = Duration::from_secs(30);

#[derive(Clone, Copy)]
enum Outcome {
    Observed,
    Failed,
    Busy,
}

#[derive(Default)]
struct WakeState {
    generation: u64,
    process_generation: u64,
    stopped: bool,
}

#[derive(Default)]
pub(super) struct Wake {
    state: Mutex<WakeState>,
    changed: Condvar,
}

impl Wake {
    pub(super) fn notify(&self) {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        state.generation = state.generation.wrapping_add(1);
        state.process_generation = state.process_generation.wrapping_add(1);
        self.changed.notify_one();
    }

    fn completed(&self) {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        state.generation = state.generation.wrapping_add(1);
        self.changed.notify_one();
    }

    fn process_generation(&self) -> u64 {
        self.state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .process_generation
    }

    fn stop(&self) {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        state.stopped = true;
        self.changed.notify_all();
    }

    fn generation(&self) -> Option<u64> {
        let state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        (!state.stopped).then_some(state.generation)
    }

    fn wait(&self, generation: u64, deadline: Option<Instant>) {
        let state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        let unchanged = |state: &mut WakeState| !state.stopped && state.generation == generation;
        if let Some(deadline) = deadline {
            let _ = self.changed.wait_timeout_while(
                state,
                deadline.saturating_duration_since(Instant::now()),
                unchanged,
            );
        } else {
            drop(self.changed.wait_while(state, unchanged));
        }
    }
}

#[derive(Clone)]
struct Candidate {
    id: TerminalId,
    handle: TerminalHandle,
    process: AgentProcess,
    proof: HostSessionProof,
}

impl Candidate {
    fn matches(&self, other: &Self) -> bool {
        self.id == other.id
            && self.proof == other.proof
            && self.handle.same_worker(&other.handle)
            && self.process == other.process
    }
}

struct Entry {
    candidate: Candidate,
    due: Instant,
    failures: u32,
}

#[derive(Default)]
struct Schedule {
    entries: BTreeMap<TerminalId, Entry>,
    // Separate from candidates: an exiting/replaced process cannot free a slot
    // until its old observation has finished, or admit duplicate work for its ID.
    running: BTreeSet<TerminalId>,
}

impl Schedule {
    fn reconcile(&mut self, candidates: Vec<Candidate>, now: Instant) {
        let mut retained = BTreeSet::new();
        for candidate in candidates {
            retained.insert(candidate.id);
            let entry = self.entries.entry(candidate.id).or_insert_with(|| Entry {
                candidate: candidate.clone(),
                due: now,
                failures: 0,
            });
            if !entry.candidate.matches(&candidate) {
                *entry = Entry {
                    candidate,
                    due: now,
                    failures: 0,
                };
            }
        }
        self.entries.retain(|id, _| retained.contains(id));
    }

    fn next(&mut self, now: Instant) -> Option<Candidate> {
        if self.running.len() >= WORKERS {
            return None;
        }
        // Oldest deadline first, so slow or failing accounts cannot starve
        // terminals later in the ID map. No backlog is admitted to the workers.
        let entry = self
            .entries
            .values()
            .filter(|entry| entry.due <= now && !self.running.contains(&entry.candidate.id))
            .min_by_key(|entry| (entry.due, entry.candidate.id))?;
        let candidate = entry.candidate.clone();
        self.running.insert(candidate.id);
        Some(candidate)
    }

    fn complete(&mut self, candidate: &Candidate, outcome: Outcome, now: Instant) {
        self.running.remove(&candidate.id);
        if let Some(entry) = self.entries.get_mut(&candidate.id)
            && entry.candidate.matches(candidate)
        {
            let delay = match outcome {
                Outcome::Observed => {
                    entry.failures = 0;
                    INTERVAL
                }
                Outcome::Failed => {
                    entry.failures = entry.failures.saturating_add(1);
                    (INTERVAL * (1 << entry.failures.min(4))).min(MAX_BACKOFF)
                }
                Outcome::Busy => Duration::from_millis(500),
            };
            entry.due = now + delay;
        }
    }

    fn deadline(&self) -> Option<Instant> {
        if self.running.len() >= WORKERS {
            return None;
        }
        self.entries
            .values()
            .filter(|entry| !self.running.contains(&entry.candidate.id))
            .map(|entry| entry.due)
            .min()
    }
}

pub(super) struct Monitor {
    wake: Arc<Wake>,
    thread: Option<JoinHandle<()>>,
}

impl Monitor {
    pub(super) fn start(host: &SharedHost) -> io::Result<Self> {
        let wake = Arc::clone(
            &host
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .agent_activity_wake,
        );
        let (jobs, received) = crossbeam_channel::bounded::<Candidate>(WORKERS);
        let (completed, results) = crossbeam_channel::unbounded();
        let mut workers = Vec::new();
        for index in 0..WORKERS {
            let host = Arc::downgrade(host);
            let received = received.clone();
            let completed = completed.clone();
            let wake = Arc::clone(&wake);
            match thread::Builder::new()
                .name(format!("cagentsd-activity-{index}"))
                .spawn(move || {
                    work(received, completed, &wake, |candidate| {
                        observe(&host, candidate)
                    });
                }) {
                Ok(worker) => workers.push(worker),
                Err(error) => {
                    drop(jobs);
                    for worker in workers {
                        let _ = worker.join();
                    }
                    return Err(error);
                }
            }
        }
        let host = Arc::downgrade(host);
        let coordinator_wake = Arc::clone(&wake);
        let thread = thread::Builder::new()
            .name("cagentsd-activity".into())
            .spawn(move || {
                coordinate(host, &coordinator_wake, &jobs, &results, candidates);
                drop(jobs);
                for worker in workers {
                    let _ = worker.join();
                }
            })?;
        Ok(Self {
            wake,
            thread: Some(thread),
        })
    }
}

impl Drop for Monitor {
    fn drop(&mut self) {
        self.wake.stop();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn work(
    received: crossbeam_channel::Receiver<Candidate>,
    completed: crossbeam_channel::Sender<(Candidate, Outcome)>,
    wake: &Wake,
    mut observe: impl FnMut(&Candidate) -> Outcome,
) {
    for candidate in received {
        let outcome = if wake.generation().is_some() {
            super::service::attempt("agent activity observation", || Ok(observe(&candidate)))
                .unwrap_or_else(|error| {
                    tracing::warn!(%error, "agent activity observation failed");
                    Outcome::Failed
                })
        } else {
            Outcome::Failed
        };
        if completed.send((candidate, outcome)).is_err() {
            break;
        }
        wake.completed();
    }
}

fn candidates(host: &SharedHost) -> Option<Vec<Candidate>> {
    let state = host.lock().unwrap_or_else(|p| p.into_inner());
    if state.shutting_down {
        return None;
    }
    Some(
        state
            .terminals
            .iter()
            .filter_map(|(&id, record)| {
                if record.handle.is_completed() {
                    return None;
                }
                let process = record.handle.cached_agent_process()?;
                matches!(process.provider, Provider::Claude | Provider::Codex).then(|| Candidate {
                    id,
                    process,
                    handle: record.handle.clone(),
                    proof: state.proof().clone(),
                })
            })
            .collect(),
    )
}

fn coordinate(
    host: Weak<HostLock>,
    coordinator_wake: &Wake,
    jobs: &crossbeam_channel::Sender<Candidate>,
    results: &crossbeam_channel::Receiver<(Candidate, Outcome)>,
    mut candidates: impl FnMut(&SharedHost) -> Option<Vec<Candidate>>,
) {
    let mut schedule = Schedule::default();
    let mut process_generation = None;
    while let Some(generation) = coordinator_wake.generation() {
        let Some(host) = host.upgrade() else {
            break;
        };
        let observed_generation = coordinator_wake.process_generation();
        if process_generation != Some(observed_generation) {
            match super::service::attempt("activity reconciliation", || {
                let Some(candidates) = candidates(&host) else {
                    return Ok(false);
                };
                schedule.reconcile(candidates, Instant::now());
                Ok(true)
            }) {
                Ok(false) => break,
                Ok(true) => {}
                Err(error) => {
                    tracing::warn!(%error, "agent activity reconciliation is temporarily unavailable");
                    drop(host);
                    coordinator_wake.wait(generation, Some(Instant::now() + INTERVAL));
                    continue;
                }
            }
            process_generation = Some(observed_generation);
        }
        for (candidate, outcome) in results.try_iter() {
            schedule.complete(&candidate, outcome, Instant::now());
        }
        while let Some(candidate) = schedule.next(Instant::now()) {
            if jobs.send(candidate).is_err() {
                break;
            }
        }
        drop(host);
        coordinator_wake.wait(generation, schedule.deadline());
    }
}

fn observe(host: &Weak<HostLock>, candidate: &Candidate) -> Outcome {
    let Some(host) = host.upgrade() else {
        return Outcome::Failed;
    };
    let (gate, observer) = {
        let state = host.lock().unwrap_or_else(|p| p.into_inner());
        (
            Arc::clone(&state.control_gate),
            Arc::clone(&state.agent_observer),
        )
    };
    // Keep provider observation and owner apply inside upgrade admission, but
    // never hold HostState during provider I/O or waiting on the PTY owner.
    let _control = gate.read().unwrap_or_else(|p| p.into_inner());
    let current = || {
        let state = host.lock().unwrap_or_else(|p| p.into_inner());
        !state.shutting_down
            && state.proof() == &candidate.proof
            && state
                .terminals
                .get(&candidate.id)
                .is_some_and(|record| record.handle.same_worker(&candidate.handle))
            && candidate.handle.cached_agent_process().as_ref() == Some(&candidate.process)
    };
    if !current() {
        return Outcome::Failed;
    }
    let observation = observer.try_observe(&candidate.process);
    if !current() {
        return Outcome::Failed;
    }
    let (observation, outcome) = match observation {
        Ok(observation) => (observation, Outcome::Observed),
        Err(crate::agent::observe::ObserveError::Busy) => return Outcome::Busy,
        Err(crate::agent::observe::ObserveError::Unavailable) => {
            (Default::default(), Outcome::Failed)
        }
    };
    if let Some(evidence) = observation.attention {
        let state = host.lock().unwrap_or_else(|p| p.into_inner());
        if !state.shutting_down
            && !state.notification_closed.contains(&candidate.id)
            && state.proof() == &candidate.proof
            && state
                .terminals
                .get(&candidate.id)
                .is_some_and(|record| record.handle.same_worker(&candidate.handle))
            && candidate.handle.cached_agent_process().as_ref() == Some(&candidate.process)
            && let Some(notifications) = &state.notifications
        {
            let mut event = crate::notifications::Event::session(
                candidate.id,
                candidate.process.provider,
                candidate.process.launch.working_directory.clone(),
                evidence,
            );
            let title = observation.title.as_deref().or_else(|| state.terminals.get(&candidate.id).and_then(|record| record.conversation_title()));
            if let Some(title) = title {
                event.content = Some(crate::notifications::Content {
                    conversation: crate::notifications::presentation::title(title),
                    preview: String::new(), request: None,
                });
            }
            notifications.record(event);
        }
    }
    candidate.handle.apply_agent_activity(
        candidate.process.clone(),
        observation.activity,
        observation.title,
    );
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{
        BootId, HostIncarnation, HostSessionId, LaunchRecipe, TerminalContinuity, TerminalSession,
    };

    fn candidate() -> Candidate {
        let launch = LaunchRecipe {
            unset_environment: Default::default(),
            program: "/bin/sh".into(),
            arguments: vec!["-c".into(), "read line".into()],
            working_directory: "/".into(),
            environment: BTreeMap::new(),
        };
        let id = TerminalId::new();
        let handle = TerminalHandle::spawn(
            TerminalSession {
                id,
                recovery_origin: None,
                predecessor: None,
                owned_codex_listener: None,
                launch: launch.clone(),
                transcript_path: None,
                resume_hint: None,
                continuity: TerminalContinuity::RelaunchAvailable {
                    recipe: launch.clone(),
                    last_exit: None,
                },
            },
            Default::default(),
            |_| {},
        )
        .unwrap();
        Candidate {
            id,
            handle,
            process: AgentProcess {
                pid: 123,
                start: 456,
                provider: Provider::Claude,
                launch: launch.clone(),
                recovery: launch,
            },
            proof: HostSessionProof {
                boot_id: BootId::new("activity-test"),
                host_session_id: HostSessionId::new("activity-test"),
                host_incarnation: HostIncarnation::new("activity-test"),
            },
        }
    }

    #[test]
    fn activity_worker_reports_panics_and_observes_the_next_job() {
        let candidate = candidate();
        let host = crate::host::service::test_host();
        let wake = Arc::new(Wake::default());
        let (jobs, received) = crossbeam_channel::bounded(1);
        let (completed, results) = crossbeam_channel::unbounded();
        let worker_wake = wake.clone();
        let thread = std::thread::spawn(move || {
            let mut calls = 0;
            work(received, completed, &worker_wake, |_| {
                calls += 1;
                let _guard = host.lock().unwrap();
                if calls == 1 {
                    let _reentrant = host.lock().unwrap();
                    unreachable!();
                }
                Outcome::Observed
            });
        });
        let now = Instant::now();
        let mut schedule = Schedule::default();
        schedule.reconcile(vec![candidate.clone()], now);
        jobs.send(schedule.next(now).unwrap()).unwrap();
        let (failed, outcome) = results.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(matches!(outcome, Outcome::Failed));
        schedule.complete(&failed, outcome, now);
        assert!(schedule.next(now + INTERVAL).is_none());
        jobs.send(schedule.next(now + INTERVAL * 2).unwrap())
            .unwrap();
        let (observed, outcome) = results.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(matches!(outcome, Outcome::Observed));
        schedule.complete(&observed, outcome, now);
        drop(jobs);
        thread.join().unwrap();
        let _ = candidate.handle.shutdown();
    }

    #[test]
    fn activity_coordinator_retries_panicked_reconciliation_without_a_new_process_event() {
        let candidate = candidate();
        let host = crate::host::service::test_host();
        let weak_host = Arc::downgrade(&host);
        let wake = Arc::new(Wake::default());
        let (jobs, received) = crossbeam_channel::bounded(1);
        let (_completed, results) = crossbeam_channel::unbounded();
        let coordinator_wake = wake.clone();
        let returned = candidate.clone();
        let thread = std::thread::spawn(move || {
            let mut calls = 0;
            coordinate(weak_host, &coordinator_wake, &jobs, &results, |host| {
                calls += 1;
                let _guard = host.lock().unwrap();
                if calls == 1 {
                    let _reentrant = host.lock().unwrap();
                    unreachable!();
                }
                Some(vec![returned.clone()])
            });
            calls
        });
        let dispatched = received.recv_timeout(Duration::from_secs(5));
        wake.stop();
        assert_eq!(thread.join().unwrap(), 2);
        assert_eq!(dispatched.unwrap().id, candidate.id);
        let _ = candidate.handle.shutdown();
    }

    #[test]
    fn scheduling_is_bounded_fair_and_failed_accounts_back_off() {
        let first = candidate();
        let mut second = first.clone();
        second.id = TerminalId::new();
        let mut third = first.clone();
        third.id = TerminalId::new();
        let now = Instant::now();
        let mut schedule = Schedule::default();
        schedule.reconcile(vec![first.clone(), second, third], now);
        let a = schedule.next(now).unwrap();
        let b = schedule.next(now).unwrap();
        assert_ne!(a.id, b.id);
        assert!(schedule.next(now).is_none());
        assert!(
            schedule.deadline().is_none(),
            "full workers park until completion"
        );
        schedule.complete(&a, Outcome::Failed, now);
        let c = schedule.next(now).unwrap();
        assert_ne!(c.id, a.id);
        assert_ne!(c.id, b.id);
        schedule.complete(&b, Outcome::Observed, now);
        schedule.complete(&c, Outcome::Observed, now);
        assert!(schedule.next(now).is_none());
        assert_eq!(schedule.deadline(), Some(now + INTERVAL));
        let next = schedule.next(now + INTERVAL).unwrap();
        assert_ne!(
            next.id, a.id,
            "failed account waits longer than healthy accounts"
        );
        for _ in 0..10 {
            schedule.complete(&a, Outcome::Failed, now);
        }
        assert_eq!(schedule.entries[&a.id].due, now + MAX_BACKOFF);
        let _ = first.handle.shutdown();
    }

    #[test]
    fn replacement_waits_for_old_job_but_does_not_inherit_its_backoff() {
        let first = candidate();
        let mut schedule = Schedule::default();
        let now = Instant::now();
        schedule.reconcile(vec![first.clone()], now);
        let old = schedule.next(now).unwrap();
        let mut replacement = first.clone();
        replacement.process.start += 1;
        schedule.reconcile(vec![replacement.clone()], now);
        assert!(
            schedule.next(now).is_none(),
            "only one in-flight probe per terminal"
        );
        schedule.complete(&old, Outcome::Failed, now);
        let new = schedule.next(now).unwrap();
        assert_eq!(new.process.start, replacement.process.start);
        schedule.reconcile(Vec::new(), now);
        schedule.complete(&new, Outcome::Failed, now);
        assert!(schedule.deadline().is_none(), "no agents means no timer");
        assert!(schedule.next(now).is_none());
        let _ = first.handle.shutdown();
    }

    #[test]
    fn wake_retains_changes_before_wait_and_stop_cancels_indefinite_wait() {
        let wake = Arc::new(Wake::default());
        let generation = wake.generation().unwrap();
        wake.notify();
        let process_generation = wake.process_generation();
        wake.completed();
        assert_eq!(
            wake.process_generation(),
            process_generation,
            "completed probes do not trigger a scan of every terminal"
        );
        // A lost edge here would block forever: wait must inspect generation.
        wake.wait(generation, None);
        let waiting = Arc::clone(&wake);
        let generation = wake.generation().unwrap();
        let waiter = thread::spawn(move || waiting.wait(generation, None));
        wake.stop();
        waiter.join().unwrap();
        assert!(wake.generation().is_none());
    }
}
