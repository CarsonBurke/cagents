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
        let contexts = {
            let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
            if state.has_committed_upgrade() {
                return Ok(None);
            }
            state.begin_ordinary_exit_cleanup();
            contexts(&state)
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
                    run(&worker_host, &worker_progress);
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
    semantic_done: bool,
    attempted: bool,
    worker: Option<(mpsc::Receiver<Result<(), String>>, JoinHandle<()>)>,
}

struct LocalWork {
    source: crate::chat::Session,
    handle: Handle,
    done: bool,
    attempted: bool,
    worker: Option<(mpsc::Receiver<Result<(), String>>, JoinHandle<()>)>,
}

fn spawn_job(
    name: &str,
    work: impl FnOnce() -> Result<(), String> + Send + 'static,
) -> Result<(mpsc::Receiver<Result<(), String>>, JoinHandle<()>), String> {
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
    Ok((completion, worker))
}

fn collect_job(
    job: &mut Option<(mpsc::Receiver<Result<(), String>>, JoinHandle<()>)>,
) -> Option<Result<(), String>> {
    let result = match job.as_ref()?.0.try_recv() {
        Ok(result) => result,
        Err(mpsc::TryRecvError::Empty) => return None,
        Err(mpsc::TryRecvError::Disconnected) => Err("Cleanup worker lost its receipt".into()),
    };
    let (_, worker) = job.take().unwrap();
    Some(
        worker
            .join()
            .map_err(|_| "Cleanup worker join failed".to_owned())
            .and(result),
    )
}

fn run(host: &SharedHost, progress: &Arc<Mutex<Progress>>) {
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
    let mut terminals = None;
    let mut terminals_done = false;
    loop {
        let pending = {
            let state = host.lock().unwrap_or_else(|p| p.into_inner());
            contexts(&state)
        };
        request_all(&pending);
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
                                    identity,
                                    capture,
                                    origin: Some(record.handle.clone()),
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
                            done: false,
                            attempted: false,
                            worker: None,
                        });
                    }
                }
                for startup in state.native_actor_startups.values() {
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
        if let Some(result) = collect_job(&mut terminals) {
            match result {
                Ok(()) => terminals_done = true,
                Err(error) => diagnostic = Some(error),
            }
        }

        // All newly encountered owners get their first attempt before retries.
        let mut slots = MAX_JOBS.saturating_sub(
            actors.iter().filter(|w| w.worker.is_some()).count()
                + locals.iter().filter(|w| w.worker.is_some()).count(),
        );
        for retry in [false, true] {
            for work in &mut actors {
                if slots == 0 {
                    break;
                }
                if work.semantic_done || work.worker.is_some() || work.attempted != retry {
                    continue;
                }
                let worker_host = host.clone();
                let capture = work.capture.clone();
                match spawn_job("cagents-startup-semantic-close", move || {
                    chat::retained_actor::startup_shutdown::cleanup(&worker_host, &capture)
                }) {
                    Ok(job) => {
                        work.worker = Some(job);
                        work.attempted = true;
                        slots -= 1;
                    }
                    Err(error) => diagnostic = Some(error),
                }
            }
            for work in &mut locals {
                if slots == 0 {
                    break;
                }
                if work.done || work.worker.is_some() || work.attempted != retry {
                    continue;
                }
                let worker_host = host.clone();
                let source = work.source.clone();
                let handle = work.handle.clone();
                match spawn_job("cagents-chat-close", move || {
                    handle.close()?;
                    chat::retained_actor::finish_close(&worker_host, &handle, &source)?;
                    agent_execution::retired(&worker_host, &source)
                }) {
                    Ok(job) => {
                        work.worker = Some(job);
                        work.attempted = true;
                        slots -= 1;
                    }
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
            match &snapshot.kind {
                LaunchOwnerKind::RemoteActor { identity } => {
                    if actors
                        .iter()
                        .any(|work| &work.identity == identity && work.semantic_done)
                    {
                        if let Some(token) = snapshot.token {
                            context.complete_semantic_cleanup(token, identity);
                        }
                    }
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
            if terminals_done
                && actors_done
                && locals.iter().all(|work| work.done && work.worker.is_none())
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
