//! Ordinary teardown retains the serving listener while exact owners settle.
//! Cancellation precedes the control barrier; no waiting writer is queued.
use super::{agent_execution, chat, state::SharedHost};
use crate::{
    chat::{
        protocol::{ExecutionContext, HelperWaitProof, LaunchOutcome, LaunchOwnerKind},
        runtime::{ActorIdentity, Handle},
    },
    daemon::{self, child_reaper::ExitDrain},
};
use std::{
    sync::{Arc, Mutex, mpsc},
    thread::{self, JoinHandle},
    time::Duration,
};

const RETRY: Duration = Duration::from_millis(20);
const MAX_JOBS: usize = 4;

/// Cursor spans every cleanup class. First attempts take priority; retries
/// resume after the last dispatched owner rather than restarting at actor zero.
#[derive(Default)]
struct FairQueue {
    next: usize,
}

impl FairQueue {
    fn choose(&mut self, eligible: &[bool], first_attempt: &[bool]) -> Option<usize> {
        assert_eq!(eligible.len(), first_attempt.len());
        let count = eligible.len();
        if count == 0 {
            return None;
        }
        let new_owner = eligible
            .iter()
            .zip(first_attempt)
            .any(|(ready, first)| *ready && *first);
        for offset in 0..count {
            let index = (self.next % count + offset) % count;
            if eligible[index] && (!new_owner || first_attempt[index]) {
                self.next = (index + 1) % count;
                return Some(index);
            }
        }
        None
    }
}

#[derive(Default)]
struct Progress {
    drain: Option<ExitDrain>,
    complete: bool,
    diagnostic: Option<String>,
}

pub(super) struct Coordinator {
    progress: Arc<Mutex<Progress>>,
    worker: Option<JoinHandle<()>>,
}

impl Coordinator {
    /// The HOST transaction also serializes with upgrade's final validation.
    /// No control gate is acquired before every original context is latched.
    pub(super) fn start(
        host: &SharedHost,
        stop_producers: impl FnOnce() + Send + 'static,
    ) -> Result<Option<Self>, std::io::Error> {
        let (contexts, startups) = {
            let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
            if state.has_committed_upgrade() {
                return Ok(None);
            }
            state.begin_ordinary_exit_cleanup();
            (
                contexts(&state),
                state
                    .native_actor_startups
                    .values()
                    .cloned()
                    .collect::<Vec<_>>(),
            )
        };
        request_all(&contexts);
        let progress = Arc::new(Mutex::new(Progress::default()));
        let worker_progress = progress.clone();
        let worker_host = host.clone();
        let worker = thread::Builder::new()
            .name("cagents-ordinary-exit".into())
            .spawn(move || {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    stop_producers();
                    run(&worker_host, &worker_progress, &startups);
                }));
                if result.is_err() {
                    worker_progress
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .diagnostic = Some(
                        "Ordinary cleanup coordinator panicked; exact ownership is retained".into(),
                    );
                    daemon::wake();
                }
            })?;
        Ok(Some(Self {
            progress,
            worker: Some(worker),
        }))
    }

    pub(super) fn drain(&self) -> Option<ExitDrain> {
        self.progress
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .drain
            .clone()
    }

    pub(super) fn completed(&mut self) -> bool {
        if !self
            .progress
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .complete
        {
            return false;
        }
        // Completion is published only after every cleanup job is joined.
        // Joining the coordinator also covers its final thread destructors.
        if let Some(worker) = self.worker.take() {
            if worker.join().is_err() {
                return false;
            }
        }
        true
    }
}

fn contexts(state: &super::state::HostState) -> Vec<Arc<ExecutionContext>> {
    let mut contexts = Vec::new();
    for context in state
        .native_actor_startups
        .values()
        .map(|s| &s.context)
        .chain(
            state
                .execution_startups
                .values()
                .map(|(_, context)| context),
        )
    {
        if !contexts.iter().any(|old| Arc::ptr_eq(old, context)) {
            contexts.push(context.clone());
        }
    }
    contexts
}

fn request_all(contexts: &[Arc<ExecutionContext>]) {
    for context in contexts {
        context.request_ordinary_cleanup();
    }
}

struct ActorWork {
    identity: ActorIdentity,
    capture: chat::retained_actor::startup_shutdown::Capture,
    origin: Option<Handle>,
    bindings: Vec<(
        Arc<ExecutionContext>,
        crate::chat::protocol::LaunchOwnershipSnapshot,
    )>,
    semantic_done: bool,
    attempted: bool,
    worker: Option<CleanupJob>,
}

fn bindings(
    contexts: &[Arc<ExecutionContext>],
    identity: &ActorIdentity,
) -> Vec<(
    Arc<ExecutionContext>,
    crate::chat::protocol::LaunchOwnershipSnapshot,
)> {
    contexts.iter().filter_map(|context| {
        let snapshot = context.ownership_snapshot();
        matches!(&snapshot.kind, LaunchOwnerKind::RemoteActor { identity: current } if current == identity)
            .then(|| (context.clone(), snapshot))
    }).collect()
}

struct LocalWork {
    source: crate::chat::Session,
    handle: Handle,
    bindings: Vec<(
        Arc<ExecutionContext>,
        crate::chat::protocol::LaunchOwnershipSnapshot,
    )>,
    done: bool,
    attempted: bool,
    worker: Option<CleanupJob>,
}

struct GenericWork {
    context: Arc<ExecutionContext>,
    attempted: bool,
    worker: Option<CleanupJob>,
}

struct CleanupJob {
    result: mpsc::Receiver<Result<(), String>>,
    worker: Option<JoinHandle<()>>,
}

impl Drop for CleanupJob {
    fn drop(&mut self) {
        // This guard lives only in the coordinator. Even an unwind retains
        // and joins every owned job before dropping its captured evidence.
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn spawn_job(
    name: &str,
    work: impl FnOnce() -> Result<(), String> + Send + 'static,
) -> Result<CleanupJob, String> {
    let (complete, completion) = mpsc::channel();
    let worker = thread::Builder::new()
        .name(name.into())
        .spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(work))
                .unwrap_or_else(|_| Err("Exact cleanup worker panicked; evidence retained".into()));
            let _ = complete.send(result);
            daemon::wake();
        })
        .map_err(|error| error.to_string())?;
    Ok(CleanupJob {
        result: completion,
        worker: Some(worker),
    })
}

fn collect_job(job: &mut Option<CleanupJob>) -> Option<Result<(), String>> {
    let result = match job.as_ref()?.result.try_recv() {
        Ok(result) => result,
        Err(mpsc::TryRecvError::Empty) => return None,
        Err(mpsc::TryRecvError::Disconnected) => Err("Cleanup worker lost its receipt".into()),
    };
    let mut owned = job.take().unwrap();
    let worker = owned.worker.take().unwrap();
    Some(
        worker
            .join()
            .map_err(|_| "Cleanup worker join failed".to_owned())
            .and(result),
    )
}

fn run(
    host: &SharedHost,
    progress: &Arc<Mutex<Progress>>,
    startups: &[chat::retained_actor::Startup],
) {
    let gate = host
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .control_gate
        .clone();
    // Repeated try_write never queues this coordinator ahead of inspection.
    loop {
        let pending = {
            let state = host.lock().unwrap_or_else(|p| p.into_inner());
            contexts(&state)
        };
        request_all(&pending);
        match gate.try_write() {
            Ok(_barrier) => {
                let drain = daemon::child_reaper::begin_exit_drain();
                progress.lock().unwrap_or_else(|p| p.into_inner()).drain = Some(drain);
                daemon::wake();
                break;
            }
            Err(std::sync::TryLockError::Poisoned(poisoned)) => {
                let _barrier = poisoned.into_inner();
                let drain = daemon::child_reaper::begin_exit_drain();
                progress.lock().unwrap_or_else(|p| p.into_inner()).drain = Some(drain);
                daemon::wake();
                break;
            }
            Err(std::sync::TryLockError::WouldBlock) => thread::sleep(RETRY),
        }
    }
    let mut actors: Vec<ActorWork> = Vec::new();
    let mut locals: Vec<LocalWork> = Vec::new();
    let mut generics: Vec<GenericWork> = Vec::new();
    let mut terminals = None;
    let mut terminals_done = false;
    let mut fair = FairQueue::default();
    loop {
        let pending = {
            let state = host.lock().unwrap_or_else(|p| p.into_inner());
            contexts(&state)
        };
        request_all(&pending);
        for context in &pending {
            let snapshot = context.ownership_snapshot();
            if snapshot.kind == LaunchOwnerKind::Generic
                && snapshot.outcome == LaunchOutcome::Uncertain
                && !generics
                    .iter()
                    .any(|work| Arc::ptr_eq(&work.context, context))
            {
                generics.push(GenericWork {
                    context: context.clone(),
                    attempted: false,
                    worker: None,
                });
            }
        }
        let mut diagnostic = None;
        {
            let state = host.lock().unwrap_or_else(|p| p.into_inner());
            // Terminal recovery and checked Close replies keep their existing
            // initiation fence. Native startups themselves must not block it.
            if !state.ordinary_exit_cleanup_blocked() {
                // Hosted captures win over unpublished captures of the same
                // exact actor; one current attachment performs semantic close.
                for record in state.chats.values() {
                    if let Some(identity) = record.handle.actor_identity() {
                        if !actors.iter().any(|work| work.identity == identity) {
                            match chat::retained_actor::startup_shutdown::capture_hosted(
                                &state,
                                &record.session,
                                &record.handle,
                            ) {
                                Ok(capture) => actors.push(ActorWork {
                                    identity: identity.clone(),
                                    capture,
                                    origin: Some(record.handle.clone()),
                                    bindings: bindings(&pending, &identity),
                                    semantic_done: false,
                                    attempted: false,
                                    worker: None,
                                }),
                                Err(error) => diagnostic = Some(error),
                            }
                        }
                    } else if !locals
                        .iter()
                        .any(|work| work.handle.same_attachment(&record.handle))
                    {
                        locals.push(LocalWork {
                            source: record.session.clone(),
                            handle: record.handle.clone(),
                            bindings: state
                                .execution_startups
                                .values()
                                .filter_map(|(intent, context)| {
                                    let crate::agent_execution::HostedSession::Chat(source) =
                                        intent
                                    else {
                                        return None;
                                    };
                                    let snapshot = context.ownership_snapshot();
                                    (crate::agent_execution::same_chat_process(
                                        source,
                                        &record.session,
                                    ) && snapshot.kind == LaunchOwnerKind::Generic)
                                        .then(|| (context.clone(), snapshot))
                                })
                                .collect(),
                            done: false,
                            attempted: false,
                            worker: None,
                        });
                    }
                }
                for startup in startups {
                    if state
                        .native_actor_startups
                        .get(&startup.source.id)
                        .is_none_or(|current| !current.matches(startup))
                    {
                        diagnostic = Some(
                            "An original startup descriptor/context was replaced; custody retained"
                                .into(),
                        );
                        continue;
                    }
                    let snapshot = startup.context.ownership_snapshot();
                    if let LaunchOwnerKind::RemoteActor { identity } = &snapshot.kind {
                        if !actors.iter().any(|work| &work.identity == identity) {
                            match chat::retained_actor::startup_shutdown::capture_startup(
                                &state, startup,
                            ) {
                                Ok(capture) => actors.push(ActorWork {
                                    identity: identity.clone(),
                                    capture,
                                    origin: snapshot.remote,
                                    bindings: bindings(&pending, identity),
                                    semantic_done: false,
                                    attempted: false,
                                    worker: None,
                                }),
                                Err(error) => diagnostic = Some(error),
                            }
                        }
                    }
                }
                for (agent, (intent, context)) in &state.execution_startups {
                    let snapshot = context.ownership_snapshot();
                    if let LaunchOwnerKind::RemoteActor { identity } = &snapshot.kind {
                        if !actors.iter().any(|work| &work.identity == identity) {
                            match chat::retained_actor::startup_shutdown::capture_execution(
                                &state, *agent, intent, context,
                            ) {
                                Ok(capture) => actors.push(ActorWork {
                                    identity: identity.clone(),
                                    capture,
                                    origin: snapshot.remote,
                                    bindings: bindings(&pending, identity),
                                    semantic_done: false,
                                    attempted: false,
                                    worker: None,
                                }),
                                Err(error) => diagnostic = Some(error),
                            }
                        }
                    }
                }
            }
        }

        for work in &mut actors {
            if let Some(result) = collect_job(&mut work.worker) {
                match result {
                    Ok(()) => work.semantic_done = true,
                    Err(error) => diagnostic = Some(error),
                }
            }
        }
        for work in &mut locals {
            if let Some(result) = collect_job(&mut work.worker) {
                match result {
                    Ok(()) => work.done = true,
                    Err(error) => diagnostic = Some(error),
                }
            }
        }
        for work in &mut generics {
            if let Some(Err(error)) = collect_job(&mut work.worker) {
                diagnostic = Some(error);
            }
        }
        if let Some(result) = collect_job(&mut terminals) {
            match result {
                Ok(()) => terminals_done = true,
                Err(error) => diagnostic = Some(error),
            }
        }

        // All newly encountered owners get their first attempt before retries.
        let mut slots = MAX_JOBS.saturating_sub(
            actors.iter().filter(|w| w.worker.is_some()).count()
                + locals.iter().filter(|w| w.worker.is_some()).count()
                + generics.iter().filter(|w| w.worker.is_some()).count(),
        );
        while slots > 0 {
            let eligible = actors
                .iter()
                .map(|work| !work.semantic_done && work.worker.is_none())
                .chain(
                    locals
                        .iter()
                        .map(|work| !work.done && work.worker.is_none()),
                )
                .chain(generics.iter().map(|work| {
                    let snapshot = work.context.ownership_snapshot();
                    snapshot.outcome == LaunchOutcome::Uncertain
                        && snapshot.kind == LaunchOwnerKind::Generic
                        && work.worker.is_none()
                        && !locals.iter().any(|local| {
                            !local.done
                                && local
                                    .bindings
                                    .iter()
                                    .any(|(owner, _)| Arc::ptr_eq(owner, &work.context))
                        })
                }))
                .collect::<Vec<_>>();
            let first = actors
                .iter()
                .map(|work| !work.attempted)
                .chain(locals.iter().map(|work| !work.attempted))
                .chain(generics.iter().map(|work| !work.attempted))
                .collect::<Vec<_>>();
            let Some(index) = fair.choose(&eligible, &first) else {
                break;
            };
            slots -= 1;
            if index < actors.len() {
                let work = &mut actors[index];
                work.attempted = true;
                let worker_host = host.clone();
                let capture = work.capture.clone();
                match spawn_job("cagents-startup-semantic-close", move || {
                    chat::retained_actor::startup_shutdown::cleanup(&worker_host, &capture)
                }) {
                    Ok(job) => work.worker = Some(job),
                    Err(error) => diagnostic = Some(error),
                }
            } else if index < actors.len() + locals.len() {
                let work = &mut locals[index - actors.len()];
                work.attempted = true;
                let worker_host = host.clone();
                let source = work.source.clone();
                let handle = work.handle.clone();
                match spawn_job("cagents-chat-close", move || {
                    let require_current = || {
                        let state = worker_host.lock().unwrap_or_else(|p| p.into_inner());
                        if state.chats.get(&source.id).is_some_and(|record| {
                            record.handle.same_attachment(&handle)
                                && crate::agent_execution::same_chat_process(
                                    &record.session,
                                    &source,
                                )
                        }) {
                            Ok(())
                        } else {
                            Err("The captured local cleanup attachment changed".to_owned())
                        }
                    };
                    require_current()?;
                    handle.close()?;
                    chat::retained_actor::finish_close(&worker_host, &handle, &source)?;
                    require_current()?;
                    agent_execution::retired(&worker_host, &source)?;
                    require_current()
                }) {
                    Ok(job) => work.worker = Some(job),
                    Err(error) => diagnostic = Some(error),
                }
            } else {
                let work = &mut generics[index - actors.len() - locals.len()];
                work.attempted = true;
                let context = work.context.clone();
                match spawn_job("cagents-launch-cleanup", move || {
                    match context.reconcile_cleanup()? {
                        LaunchOutcome::NoProvider | LaunchOutcome::TeardownProved => Ok(()),
                        LaunchOutcome::Uncertain => {
                            Err("Exact launch cleanup remains unconfirmed".into())
                        }
                    }
                }) {
                    Ok(job) => work.worker = Some(job),
                    Err(error) => diagnostic = Some(error),
                }
            }
        }

        if !terminals_done && terminals.is_none() {
            let handles = {
                let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
                (!state.ordinary_exit_cleanup_blocked()).then(|| state.graceful_shutdown().0)
            };
            if let Some(handles) = handles {
                match spawn_job("cagents-terminal-close", move || {
                    let failures = super::finish_terminal_shutdown(&handles)
                        .into_iter()
                        .filter_map(Result::err)
                        .map(|error| error.to_string())
                        .collect::<Vec<_>>();
                    if failures.is_empty() {
                        Ok(())
                    } else {
                        Err(failures.join("; "))
                    }
                }) {
                    Ok(job) => terminals = Some(job),
                    Err(error) => diagnostic = Some(error),
                }
            }
        }

        // Persist exact intent clears outside HOST. The per-agent gate and
        // context/token comparisons fence late dispatch error publication.
        for context in &pending {
            let snapshot = context.ownership_snapshot();
            if let Some(error) = &snapshot.diagnostic {
                diagnostic = Some(error.clone());
            }
            match &snapshot.kind {
                LaunchOwnerKind::RemoteActor { identity } => {
                    for work in actors
                        .iter()
                        .filter(|work| &work.identity == identity && work.semantic_done)
                    {
                        for (original, captured) in &work.bindings {
                            if Arc::ptr_eq(original, context) && captured.kind == snapshot.kind {
                                if let Some(token) = captured.token {
                                    context.complete_semantic_cleanup(token, identity);
                                }
                            }
                        }
                    }
                }
                LaunchOwnerKind::Generic => {
                    for local in locals.iter().filter(|local| local.done) {
                        for (owner, captured) in &local.bindings {
                            if Arc::ptr_eq(owner, context) {
                                context.complete_hosted_cleanup(captured);
                            }
                        }
                    }
                    context.poll_cleanup(&snapshot);
                }
                _ => {
                    context.poll_cleanup(&snapshot);
                }
            }
        }
        if let Err(error) = agent_execution::finish_ordinary_startups(host) {
            diagnostic = Some(error);
        }
        {
            let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
            let releasable = state
                .native_actor_startups
                .iter()
                .filter_map(|(id, startup)| {
                    if !startups.iter().any(|original| startup.matches(original)) {
                        return None;
                    }
                    let snapshot = startup.context.ownership_snapshot();
                    let proved = matches!(
                        snapshot.outcome,
                        LaunchOutcome::NoProvider | LaunchOutcome::TeardownProved
                    );
                    let execution_pending = state
                        .execution_startups
                        .values()
                        .any(|(_, context)| Arc::ptr_eq(context, &startup.context));
                    (proved
                        && snapshot.ordinary_requested
                        && !execution_pending
                        && !state.chat_spawning.contains_key(id))
                    .then(|| (*id, startup.clone(), snapshot.token, snapshot.kind))
                })
                .collect::<Vec<_>>();
            for (id, captured, token, kind) in releasable {
                let current = captured.context.ownership_snapshot();
                if current.token == token
                    && current.kind == kind
                    && state
                        .native_actor_startups
                        .get(&id)
                        .is_some_and(|startup| startup.matches(&captured))
                {
                    state.native_actor_startups.remove(&id);
                    state.inventory_generation += 1;
                    daemon::wake();
                }
            }
            let actors_done = actors.iter().all(|work| {
                work.semantic_done
                    && work.worker.is_none()
                    && work.origin.as_ref().is_none_or(|origin| {
                        origin.original_helper_wait_proof() != HelperWaitProof::Pending
                    })
            });
            // Failed capture is retained custody, not an empty work list.
            // Every current attachment must have its own completed exact job.
            let inventory_done = state.chats.values().all(|record| {
                if let Some(identity) = record.handle.actor_identity() {
                    actors.iter().any(|work| {
                        work.identity == identity
                            && work.semantic_done
                            && work
                                .origin
                                .as_ref()
                                .is_some_and(|origin| origin.same_attachment(&record.handle))
                    })
                } else {
                    locals
                        .iter()
                        .any(|work| work.done && work.handle.same_attachment(&record.handle))
                }
            });
            if terminals_done
                && actors_done
                && inventory_done
                && locals.iter().all(|work| work.done && work.worker.is_none())
                && generics.iter().all(|work| {
                    work.worker.is_none() && work.context.outcome() != LaunchOutcome::Uncertain
                })
                && state.native_actor_startups.is_empty()
                && state.execution_startups.is_empty()
                && !state.ordinary_exit_cleanup_blocked()
            {
                let mut progress = progress.lock().unwrap_or_else(|p| p.into_inner());
                progress.complete = true;
                progress.diagnostic = None;
                drop(progress);
                daemon::wake();
                return;
            }
        }
        if let Some(error) = diagnostic {
            let mut progress = progress.lock().unwrap_or_else(|p| p.into_inner());
            if progress.diagnostic.as_ref() != Some(&error) {
                tracing::warn!(%error, "ordinary teardown retains exact ownership for retry");
            }
            progress.diagnostic = Some(error);
        }
        thread::sleep(RETRY);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retries_rotate_past_four_failing_actors_to_repaired_local_and_generic_owners() {
        // Four retained actors always fail. The local and generic owners each
        // fail once and are then repaired; both must get another attempt while
        // the original four still need cleanup. This is greater than MAX_JOBS.
        let mut fair = FairQueue::default();
        let mut attempts = vec![0usize; MAX_JOBS + 2];
        let mut done = vec![false; attempts.len()];
        for _ in 0..8 {
            let eligible = done.iter().map(|done| !done).collect::<Vec<_>>();
            let first = attempts
                .iter()
                .map(|attempts| *attempts == 0)
                .collect::<Vec<_>>();
            let mut in_round = eligible;
            for _ in 0..MAX_JOBS {
                let Some(owner) = fair.choose(&in_round, &first) else {
                    break;
                };
                in_round[owner] = false;
                attempts[owner] += 1;
                if owner >= MAX_JOBS && attempts[owner] >= 2 {
                    done[owner] = true;
                }
            }
            if done[MAX_JOBS] && done[MAX_JOBS + 1] {
                break;
            }
        }
        assert!(
            done[MAX_JOBS] && done[MAX_JOBS + 1],
            "repaired owners starved behind failing actors: {attempts:?}"
        );
        assert!(attempts[..MAX_JOBS].iter().all(|attempts| *attempts >= 1));
    }

    #[test]
    fn a_new_owner_gets_its_first_attempt_before_an_eligible_retry() {
        let mut fair = FairQueue { next: 0 };
        assert_eq!(
            fair.choose(&[true, true, true], &[false, false, true]),
            Some(2)
        );
        assert_eq!(
            fair.choose(&[true, true, true], &[false, false, false]),
            Some(0)
        );
        assert_eq!(
            fair.choose(&[true, true, true], &[false, false, false]),
            Some(1)
        );
    }
}
