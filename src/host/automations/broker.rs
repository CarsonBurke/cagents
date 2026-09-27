//! One sleeping coordinator owns schedule deadlines and bounded precheck
//! observations. Provider execution stays in the shared execution service.
use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{Arc, Condvar, Mutex, Weak},
    thread::JoinHandle,
    time::Duration,
};

use uuid::Uuid;

use super::{
    precheck::worker::{Request, Status, Worker},
    schedule::Compiled,
    service,
};
use crate::{
    automations::{Firing, FiringState, SessionMode, Trigger},
    host::{
        agent_execution,
        model::Claimed,
        state::{HostState, SharedHost},
    },
};

#[derive(Default)]
struct WakeState {
    generation: u64,
    stopped: bool,
}

#[derive(Default)]
struct Wake {
    state: Mutex<WakeState>,
    changed: Condvar,
}

impl Wake {
    fn notify(&self) {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        state.generation = state.generation.wrapping_add(1);
        self.changed.notify_all();
    }
    fn wait(&self, observed: u64, timeout: Duration) -> bool {
        let state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        let (state, _) = self
            .changed
            .wait_timeout_while(state, timeout, |s| !s.stopped && s.generation == observed)
            .unwrap_or_else(|p| p.into_inner());
        !state.stopped
    }
}

struct Inner {
    host: Weak<Mutex<HostState>>,
    registry: agent_execution::SharedRegistry,
    root: PathBuf,
    wake: Wake,
    workers: Mutex<BTreeMap<Uuid, Worker>>,
    // One lazy startup census catches receipts left after a committed result
    // followed by daemon loss. Each wake visits at most 32 owned directories.
    cleanup: Mutex<Option<std::fs::ReadDir>>,
    // The journal is bounded. Polling must not resurrect a blocked run after
    // newer unrelated events evict its durable replay identity.
    attention: Mutex<BTreeMap<crate::model::RunId, crate::notifications::Evidence>>,
}

#[derive(Clone)]
pub(in crate::host) struct Handle(Arc<Inner>);

pub(in crate::host) struct Broker {
    handle: Handle,
    thread: Option<JoinHandle<()>>,
}

impl Broker {
    pub(super) fn start(host: &SharedHost, root: PathBuf) -> Result<Self, String> {
        let cleanup = std::fs::read_dir(&root).map_err(|error| error.to_string())?;
        let registry = agent_execution::registry(host)?;
        let attention = initial_attention(&registry.lock().unwrap_or_else(|p| p.into_inner()))?;
        let handle = Handle(Arc::new(Inner {
            host: Arc::downgrade(host),
            registry,
            root,
            wake: Wake::default(),
            workers: Mutex::new(BTreeMap::new()),
            cleanup: Mutex::new(Some(cleanup)),
            attention: Mutex::new(attention),
        }));
        let owner = handle.clone();
        let thread = std::thread::Builder::new()
            .name("automation-scheduler".into())
            .spawn(move || owner.run())
            .map_err(|e| e.to_string())?;
        Ok(Self {
            handle,
            thread: Some(thread),
        })
    }
    pub(super) fn handle(&self) -> Handle {
        self.handle.clone()
    }
}

impl Drop for Broker {
    fn drop(&mut self) {
        self.handle
            .0
            .wake
            .state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .stopped = true;
        self.handle.0.wake.changed.notify_all();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        // Dedicated precheck owners retain their deadline after daemon exit;
        // dropping a controller is not evidence that shell descendants ended.
    }
}

impl Handle {
    pub(super) fn wake(&self) {
        self.0.wake.notify();
    }

    /// Called by the management dispatcher while it owns control.read().
    pub(super) fn cancel(&self, id: Uuid, firing: Uuid) -> Result<(), String> {
        let host = self.0.host.upgrade().ok_or("The daemon is stopping")?;
        let detail = {
            let registry = self.0.registry.lock().unwrap_or_else(|p| p.into_inner());
            let automation = registry.automation(id)?;
            let detail = registry.automation_run_detail(firing)?;
            if automation.active_firing != Some(firing) || detail.firing.automation != id {
                return Err("This firing no longer owns the automation".into());
            }
            detail
        };
        let result = if let Some(run) = detail.run {
            agent_execution::cancel_agent_run_under_control(&host, &run)
        } else {
            self.0
                .registry
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .cancel_automation_precheck(id, firing)
                .map(|_| ())
        };
        self.wake();
        result
    }

    pub(super) fn run_now(
        &self,
        id: Uuid,
        revision: u64,
        request: Uuid,
        confirmed: bool,
    ) -> Result<Firing, String> {
        if !confirmed {
            return Err(
                "Confirm this automation's target, account and permissions before running it"
                    .into(),
            );
        }
        if let Some(previous) = self
            .0
            .registry
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .previous_manual_automation_firing(id, revision, request)?
        {
            return Ok(previous);
        }
        let host = self.0.host.upgrade().ok_or("The daemon is stopping")?;
        let automation = self
            .0
            .registry
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .automation(id)?;
        if automation.execution_revision != revision {
            return Err("The automation changed before this run was confirmed".into());
        }
        service::validate_target(&host, &automation.definition, &automation.target_identity)?;
        agent_execution::selected_account(&automation.definition.agent)?;
        let claim = self
            .0
            .registry
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .claim_automation(
                id,
                revision,
                None,
                Trigger::Manual { request },
                automation.next,
                confirmed,
                service::now()?,
            )?;
        let firing = claim.firing.clone();
        let result = self.launch(&host, claim);
        self.wake();
        result?;
        self.0
            .registry
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .automation_firing(firing.id)
    }

    /// Caller holds the daemon control read gate, either the IPC dispatcher or
    /// the scheduled wrapper below. Never recursively acquire std's RwLock.
    fn launch(&self, host: &SharedHost, claim: Claimed) -> Result<(), String> {
        if !claim.created || !claim.firing.state.is_active() {
            return Ok(());
        }
        if let Err(error) = crate::host::client::ensure_control_open(host) {
            self.0
                .registry
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .fail_automation_start(claim.firing.id, error.message, service::now()?, false)?;
            return Ok(());
        }
        let directory =
            match service::validate_target(host, &claim.definition, &claim.target_identity)
                .and_then(|directory| {
                    agent_execution::selected_account(&claim.definition.agent).map(|_| directory)
                }) {
                Ok(directory) => directory,
                Err(error) => {
                    self.0
                        .registry
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .fail_automation_start(claim.firing.id, error, service::now()?, true)?;
                    return Ok(());
                }
            };
        if let Some(accepted) = claim.accepted {
            agent_execution::dispatch_accepted_in_directory(host, accepted, directory)?;
        } else if let Some(precheck) = claim.definition.precheck {
            let request = Request {
                firing: claim.firing.id,
                revision: claim.firing.revision,
                directory: claim.definition.agent.target.directory,
                target_identity: claim.target_identity,
                precheck,
            };
            match Worker::launch(&self.0.root, &request) {
                Ok(worker) => {
                    self.0
                        .workers
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .insert(claim.firing.id, worker);
                }
                Err(error) => {
                    let mut registry = self.0.registry.lock().unwrap_or_else(|p| p.into_inner());
                    if error.kind() == std::io::ErrorKind::AlreadyExists {
                        // A colliding private record could belong to an earlier
                        // owner. Never overwrite its evidence or repeat a shell.
                        registry.interrupt_automation_precheck(
                            claim.firing.id,
                            format!("Cannot prove precheck ownership: {error}"),
                            service::now()?,
                        )?;
                    } else {
                        // launch returns only after successful spawn and has no
                        // fallible post-spawn operation. These errors prove no
                        // helper owns the new claim, so capacity can be released.
                        registry.fail_automation_start(
                            claim.firing.id,
                            format!("Cannot start the precheck: {error}"),
                            service::now()?,
                            false,
                        )?;
                    }
                }
            }
        }
        Ok(())
    }

    fn launch_scheduled(&self, host: &SharedHost, claim: Claimed) -> Result<(), String> {
        let control = host
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .control_gate
            .clone();
        let _control = control.read().unwrap_or_else(|p| p.into_inner());
        self.launch(host, claim)
    }

    fn run(&self) {
        loop {
            let observed = {
                let state = self.0.wake.state.lock().unwrap_or_else(|p| p.into_inner());
                if state.stopped {
                    break;
                }
                state.generation
            };
            let Some(host) = self.0.host.upgrade() else {
                break;
            };
            let delay = match self.tick(&host) {
                Ok(delay) => delay,
                Err(error) => {
                    tracing::warn!(%error, "automation scheduling is temporarily unavailable");
                    Duration::from_secs(5)
                }
            };
            drop(host);
            if !self.0.wake.wait(observed, delay) {
                break;
            }
        }
    }

    fn tick(&self, host: &SharedHost) -> Result<Duration, String> {
        let now = service::now()?;
        let active = self
            .0
            .registry
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .active_automation_firings()?;
        let owners: std::collections::BTreeSet<_> = active.iter().map(|firing| firing.id).collect();
        let runs: std::collections::BTreeSet<_> =
            active.iter().filter_map(|firing| firing.run).collect();
        self.0
            .attention
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .retain(|run, _| runs.contains(run));
        let mut checking = false;
        let candidates = self
            .0
            .workers
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .keys()
            .filter(|id| !owners.contains(id))
            .copied()
            .collect::<Vec<_>>();
        for id in candidates {
            // A manual request can add a worker after the active census. The
            // absent snapshot entry alone never authorizes deleting its proof.
            if self
                .0
                .registry
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .retired_automation_precheck(id)?
                .is_none()
            {
                continue;
            }
            let mut workers = self.0.workers.lock().unwrap_or_else(|p| p.into_inner());
            let Some(worker) = workers.get_mut(&id) else {
                continue;
            };
            match worker.retire() {
                Ok(true) => {
                    workers.remove(&id);
                }
                Ok(false) => {} // Reap/reclaim again on the next bounded tick.
                Err(error) => {
                    tracing::warn!(%id, %error, "retired precheck files could not be reclaimed");
                    // A reaped owner may leave inaccessible files for the next
                    // startup census; never spin on permanent storage errors.
                    if worker.reaped().unwrap_or(false) {
                        workers.remove(&id);
                    }
                }
            }
        }
        checking |= self.sweep_prechecks()?;
        for firing in active {
            if let Some(run) = firing.run {
                let reconciled = self
                    .0
                    .registry
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .reconcile_automation_run(run, now)?;
                let detail = self
                    .0
                    .registry
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .automation_run_detail(firing.id)?;
                if let Some(event) = run_attention(&detail) {
                    let mut attention = self.0.attention.lock().unwrap_or_else(|p| p.into_inner());
                    if attention.get(&run) != Some(&event.evidence)
                        && let Some(notifications) =
                            &host.lock().unwrap_or_else(|p| p.into_inner()).notifications
                    {
                        attention.insert(run, event.evidence.clone());
                        notifications.record(event);
                    }
                }
                if let Some(firing) = reconciled.filter(|firing| firing.release_pending) {
                    checking |= self.release_run(host, &firing, now)?;
                }
            } else if matches!(
                firing.state,
                FiringState::Prechecking | FiringState::Interrupted
            ) {
                checking |= self.observe_precheck(host, &firing, now)?;
            }
        }
        // Four jobs per wake keeps a burst of slow filesystem validation from
        // starving completion/cleanup handling. Remaining due rows wake again.
        let due = self
            .0
            .registry
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .due_automations(now, 4)?;
        for automation in due {
            let Some(previous) = &automation.next else {
                continue;
            };
            let plan = Compiled::new(&automation.definition.schedule)?.due(previous, now)?;
            if let Some(occurrence) = plan.fire {
                let expected = if plan.missed && occurrence.due_ms > previous.due_ms {
                    self.0
                        .registry
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .skip_automation_deadlines(
                            automation.id,
                            automation.execution_revision,
                            previous,
                            occurrence.clone(),
                            now,
                        )?;
                    &occurrence
                } else {
                    previous
                };
                let claim = self
                    .0
                    .registry
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .claim_automation(
                        automation.id,
                        automation.execution_revision,
                        Some(expected),
                        Trigger::Scheduled {
                            occurrence: occurrence.clone(),
                        },
                        Some(plan.next),
                        false,
                        now,
                    )?;
                checking |= claim.created
                    && claim.firing.state == FiringState::Prechecking
                    && claim.definition.precheck.is_some();
                self.launch_scheduled(host, claim)?;
            } else {
                self.0
                    .registry
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .skip_automation_deadlines(
                        automation.id,
                        automation.execution_revision,
                        previous,
                        plan.next,
                        now,
                    )?;
            }
        }
        let next = self
            .0
            .registry
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .next_automation_deadline()?;
        let delay = next.map_or(Duration::from_secs(60), |due| {
            Duration::from_millis(due.saturating_sub(now).max(50))
        });
        // Wall-clock changes are noticed at least once a minute. Only actual
        // precheck owners require subsecond receipt observation.
        Ok(delay.min(if checking {
            Duration::from_millis(250)
        } else {
            Duration::from_secs(60)
        }))
    }

    fn release_run(&self, host: &SharedHost, firing: &Firing, now: u64) -> Result<bool, String> {
        let detail = self
            .0
            .registry
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .automation_run_detail(firing.id)?;
        let run = detail
            .run
            .ok_or("The automation lost its accepted run identity")?;
        let agent = detail
            .agent
            .ok_or("The automation lost its agent identity")?;
        // Explicit user close is valid cleanup evidence even when the earlier
        // provider outcome was interrupted. It must never authorize a resend.
        let disposition = if agent.closed {
            Ok(agent_execution::Reconciliation::Closed)
        } else {
            agent_execution::reconcile_agent_run(
                host,
                &run,
                detail.definition.session_mode == SessionMode::Fresh,
            )
        };
        let mut registry = self.0.registry.lock().unwrap_or_else(|p| p.into_inner());
        let result = match disposition {
            Ok(agent_execution::Reconciliation::Closed) => {
                let current = registry.agent(agent.id)?;
                if !current.closed {
                    Err("The provider close has no durable ownership receipt".into())
                } else {
                    registry.finish_automation_release(firing.id, &current, now)
                }
            }
            Ok(agent_execution::Reconciliation::Idle) => {
                registry.finish_automation_release(firing.id, &agent, now)
            }
            Ok(agent_execution::Reconciliation::UserOwned) => {
                registry.block_automation_release(
                    firing.id,
                    "Interactive work now owns this agent. Close it before scheduling more runs."
                        .into(),
                    now,
                )?;
                return Ok(false);
            }
            Err(error) => Err(error),
        };
        match result {
            Ok(()) => Ok(false),
            // A durable result can arrive just before the actor drains its last
            // output and becomes idle. Keep the slot while allowing that brief
            // settling period, then surface a bounded cleanup failure.
            Err(_)
                if run.state != crate::agent_execution::State::Interrupted
                    && run
                        .finished_ms
                        .is_some_and(|finished| now >= finished && now - finished < 5_000) =>
            {
                Ok(true)
            }
            Err(error) => {
                registry.block_automation_release(
                    firing.id,
                    format!("The run ended, but its session could not be released: {error}"),
                    now,
                )?;
                Ok(false)
            }
        }
    }

    fn observe_precheck(
        &self,
        host: &SharedHost,
        firing: &Firing,
        now: u64,
    ) -> Result<bool, String> {
        let status = {
            let mut workers = self.0.workers.lock().unwrap_or_else(|p| p.into_inner());
            workers
                .entry(firing.id)
                .or_insert_with(|| Worker::recover(&self.0.root, firing.id, firing.revision))
                .status()
        };
        match status {
            Ok(Status::Running) => {
                let automation = self.0.registry.lock().unwrap_or_else(|p| p.into_inner()).automation(firing.automation)?;
                if firing.cancel_requested || automation.execution_revision != firing.revision || (!automation.enabled && matches!(firing.trigger, Trigger::Scheduled { .. })) {
                    if let Some(worker) = self.0.workers.lock().unwrap_or_else(|p| p.into_inner()).get(&firing.id) { worker.cancel().map_err(|e| e.to_string())?; }
                }
                return Ok(true);
            }
            Ok(Status::Complete(outcome)) => {
                let accepted = self.0.registry.lock().unwrap_or_else(|p| p.into_inner()).finish_automation_precheck(firing.id, outcome, now)?;
                if let Some(accepted) = accepted {
                    let automation = self.0.registry.lock().unwrap_or_else(|p| p.into_inner()).automation(firing.automation)?;
                    match service::validate_target(host, &automation.definition, &automation.target_identity) {
                        Ok(directory) => agent_execution::dispatch_accepted_in_directory(host, accepted, directory)?,
                        Err(error) => self.0.registry.lock().unwrap_or_else(|p| p.into_inner()).fail_automation_start(firing.id, error, now, true)?,
                    }
                }
            }
            Ok(Status::CleanupUnconfirmed) | Err(_) if firing.state == FiringState::Interrupted => {}
            Ok(Status::CleanupUnconfirmed) => self.0.registry.lock().unwrap_or_else(|p| p.into_inner()).interrupt_automation_precheck(firing.id, "The precheck owner ended without a cleanup receipt; its effects and descendants are uncertain".into(), now)?,
            Err(error) => self.0.registry.lock().unwrap_or_else(|p| p.into_inner()).interrupt_automation_precheck(firing.id, format!("Cannot verify precheck cleanup: {error}"), now)?,
        }
        Ok(false)
    }

    fn sweep_prechecks(&self) -> Result<bool, String> {
        let (entries, more) = {
            let mut scan = self.0.cleanup.lock().unwrap_or_else(|p| p.into_inner());
            let Some(iterator) = scan.as_mut() else {
                return Ok(false);
            };
            let entries = iterator
                .take(32)
                .collect::<Result<Vec<_>, _>>()
                .map_err(|error| error.to_string())?;
            let more = entries.len() == 32;
            if !more {
                *scan = None;
            }
            (entries, more)
        };
        for entry in entries {
            if !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                continue;
            }
            let Some(id) = entry
                .file_name()
                .to_str()
                .and_then(|name| Uuid::parse_str(name).ok())
            else {
                continue;
            };
            let revision = self
                .0
                .registry
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .retired_automation_precheck(id)?;
            let Some(revision) = revision else {
                continue;
            };
            let mut worker = Worker::recover(&self.0.root, id, revision);
            match worker.retire() {
                Ok(true) => {}
                Ok(false) => {
                    self.0
                        .workers
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .entry(id)
                        .or_insert(worker);
                }
                Err(error) => {
                    tracing::warn!(%id, %error, "retired precheck files could not be reclaimed")
                }
            }
        }
        Ok(more)
    }
}

fn initial_attention(
    registry: &crate::host::model::Registry,
) -> Result<BTreeMap<crate::model::RunId, crate::notifications::Evidence>, String> {
    let mut attention = BTreeMap::new();
    // Existing terminal/waiting states are history, even if cleanup remains
    // blocked. Startup cannot reannounce them after journal eviction.
    for firing in registry.active_automation_firings()? {
        if let Some(run) = firing.run
            && let Some(event) = run_attention(&registry.automation_run_detail(firing.id)?)
        {
            attention.insert(run, event.evidence);
        }
    }
    Ok(attention)
}

fn run_attention(detail: &crate::automations::RunDetail) -> Option<crate::notifications::Event> {
    use crate::{
        agent_execution::{HostedSession, Origin, State},
        notifications::{Event, Evidence, Kind, Target},
    };
    let run = detail.run.as_ref()?;
    let agent = detail.agent.as_ref()?;
    if run.agent != agent.id
        || detail.firing.run != Some(run.id)
        || !matches!(agent.origin, Origin::Automation { id } if id == detail.firing.automation)
    {
        return None;
    }
    let kind = match run.state {
        State::Succeeded => Kind::Succeeded,
        State::Failed => Kind::Failed,
        State::Interrupted => Kind::Interrupted,
        State::Cancelled => Kind::Cancelled,
        State::Waiting => Kind::Waiting,
        _ => return None,
    };
    let source_session = agent.session.as_ref().map(|session| match session {
        HostedSession::Chat(session) => session.id,
        HostedSession::Terminal(session) => session.id,
    });
    Some(Event {
        target: Target::AgentRun(run.id),
        source_session,
        provider: Some(agent.spec.provider),
        directory: agent.spec.target.directory.clone(),
        evidence: Evidence {
            key: if kind == Kind::Waiting {
                format!("{}:waiting:{}", run.id, run.revision)
            } else {
                run.id.to_string()
            },
            kind,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn automation_attention_baselines_history_but_distinguishes_new_waiting_edges() {
        use crate::{
            agent_execution::{State, Update},
            automations::DirectoryIdentity,
            host::model::Registry,
            model::ProjectKind,
            workspace_model::ModelEdit,
        };
        let directory = tempfile::tempdir().unwrap();
        let mut registry = Registry::at(&directory.path().join("model.sqlite3")).unwrap();
        registry
            .apply(&ModelEdit::OpenProject {
                repository: "/fixture".into(),
                name: "Fixture".into(),
                kind: ProjectKind::Folder,
            })
            .unwrap();
        let definition = serde_json::from_value(serde_json::json!({
            "name":"Fixture", "prompt":"Fixture prompt",
            "agent":{"provider":"Claude","account":null,"target":{"repository":"/fixture","directory":"/fixture","channel":null},"surface":"Chat","model":null,"permission_mode":null},
            "schedule":{"pattern":{"daily":{"hour":9,"minute":0}},"timezone":"UTC"},
            "session_mode":"fresh","precheck":null
        })).unwrap();
        let automation = registry
            .create_automation(
                definition,
                100,
                DirectoryIdentity {
                    device: 1,
                    inode: 1,
                },
            )
            .unwrap();
        let claim = registry
            .claim_automation(
                automation.id,
                1,
                None,
                Trigger::Manual {
                    request: Uuid::new_v4(),
                },
                None,
                true,
                200,
            )
            .unwrap();
        let accepted = claim.accepted.unwrap();
        assert!(run_attention(&registry.automation_run_detail(claim.firing.id).unwrap()).is_none());
        let run = registry
            .update_agent_run(
                accepted.run.id,
                1,
                Update::Started {
                    provider_key: "turn".into(),
                },
                300,
            )
            .unwrap();
        let run = registry
            .update_agent_run(run.id, run.revision, Update::Waiting, 400)
            .unwrap();
        let first =
            run_attention(&registry.automation_run_detail(claim.firing.id).unwrap()).unwrap();
        assert_eq!(
            initial_attention(&registry).unwrap()[&run.id],
            first.evidence
        );
        let run = registry
            .update_agent_run(run.id, run.revision, Update::Resumed, 500)
            .unwrap();
        let run = registry
            .update_agent_run(run.id, run.revision, Update::Waiting, 600)
            .unwrap();
        let second =
            run_attention(&registry.automation_run_detail(claim.firing.id).unwrap()).unwrap();
        assert_ne!(first.evidence.key, second.evidence.key);
        assert_eq!(first.target(), second.target());
        registry
            .update_agent_run(
                run.id,
                run.revision,
                Update::Finished {
                    state: State::Interrupted,
                    result: Default::default(),
                },
                700,
            )
            .unwrap();
        let finished =
            run_attention(&registry.automation_run_detail(claim.firing.id).unwrap()).unwrap();
        assert_eq!(
            finished.evidence.kind,
            crate::notifications::Kind::Interrupted
        );
        assert_eq!(
            finished.source_session(),
            None,
            "pre-attachment evidence needs no fabricated session"
        );
        assert_eq!(
            initial_attention(&registry).unwrap()[&run.id],
            finished.evidence
        );
    }
}
