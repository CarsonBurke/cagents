//! Daemon-owned, non-PTY provider actor. Detaching never stops its transport.
use super::{
    Command, Event, Item, ItemContent, Phase, Snapshot, Subagent, ToolStatus,
    protocol::{Adapter, ProcessSpec, Step, TransportSpec},
};
use crate::account::LiveReport;
use crate::agent::ActivityState;
use std::{
    collections::{BTreeMap, VecDeque},
    io::{Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd},
        unix::{net::UnixStream, process::CommandExt},
    },
    process::{Child, Command as Process, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
mod execution;
mod store;
mod websocket;
use store::Store;

const FRAME_LIMIT: usize = 32 * 1024 * 1024;
const STATE_LIMIT: usize = 1024 * 1024;
const QUEUE_LIMIT: usize = 64 * 1024 * 1024;
const IO_TIMEOUT: Duration = Duration::from_secs(30);
const PRESENT_INTERVAL: Duration = Duration::from_millis(50);
/// Frames parsed per poll iteration: bounded so a provider flood cannot starve
/// the stdin writer or queued UI commands.
const FRAMES_PER_POLL: usize = 256;
/// Queued stdin bytes above which new commands wait for the provider to read.
const QUEUE_BACKPRESSURE: usize = QUEUE_LIMIT / 2;
enum ActorCommand {
    UpdateSource(
        super::Session,
        Option<crate::model::LaunchRecipe>,
        Option<crate::terminal::holder::Record>,
    ),
    Provider {
        run: Option<crate::model::RunId>,
        command: Command,
    },
    CloseIfIdle {
        generation: u64,
    },
    CheckUpdateReadiness(crossbeam_channel::Sender<UpdateReadiness>),
    InterruptRun(crate::model::RunId),
}
type CommandRequest = (ActorCommand, crossbeam_channel::Sender<Result<(), String>>);

/// Why a provider command may not have taken effect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Rejection {
    /// The actor never took the command, or refused it.
    Refused(String),
    /// The actor took the command but did not answer in time; the provider
    /// may still receive it.
    Unacknowledged(String),
}

impl From<Rejection> for String {
    fn from(rejection: Rejection) -> Self {
        match rejection {
            Rejection::Refused(error) | Rejection::Unacknowledged(error) => error,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UpdateReadiness {
    Idle,
    Busy,
    Unready,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Update {
    Execution {
        run: crate::model::RunId,
        update: crate::agent_execution::Update,
    },
    Attention(crate::notifications::Evidence),
    Presentation,
    Observed {
        generation: u64,
    },
    Activity {
        state: ActivityState,
        generation: u64,
    },
    /// The provider began taking prompts, which leaves activity unchanged.
    Ready,
    /// The provider reported on the account the conversation runs on. It
    /// concerns every surface showing the account, whatever this
    /// conversation's visibility, and changes nothing in the snapshot.
    AccountUsage {
        report: LiveReport,
        /// Unix milliseconds at which the runtime read the report.
        observed_at: u64,
    },
}

#[derive(Clone)]
pub struct Handle(Arc<Inner>);

struct OwnedSocket(std::path::PathBuf);
impl Drop for OwnedSocket {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
        if let Some(directory) = self.0.parent() {
            let _ = std::fs::remove_dir(directory);
        }
    }
}

impl Handle {
    pub(crate) fn same_actor(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}
struct Inner {
    commands: crossbeam_channel::Sender<CommandRequest>,
    wake: Mutex<UnixStream>,
    state: Arc<Mutex<Snapshot>>,
    confirmed_session: Arc<Mutex<Option<String>>>,
    visible: Arc<AtomicBool>,
    observe_next: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    completed: Arc<AtomicBool>,
    draining: Arc<AtomicBool>,
    failure: Arc<Mutex<Option<String>>>,
    worker: Mutex<Option<JoinHandle<()>>>,
    store: Arc<Mutex<Store>>,
    notify: Arc<dyn Fn(Update) + Send + Sync>,
    execution_environment: crate::terminal_ipc::ChatExecutionEnvironment,
    recovery_environment: BTreeMap<String, String>,
    preserve_native: Arc<AtomicBool>,
    release_native: Arc<AtomicBool>,
    retain_cleanup_receipt: Arc<AtomicBool>,
    detached_native: Arc<Mutex<Option<super::native_holder::Detached>>>,
    retained_native: Arc<Mutex<Option<super::native_holder::Holder>>>,
    native_installed: Arc<AtomicBool>,
}

enum NativeOwnership {
    New(super::Session, BTreeMap<String, String>),
    Adopted(
        super::native_holder::Holder,
        crate::daemon::holder_registry::Process,
    ),
}
impl Handle {
    #[cfg(test)]
    pub(crate) fn start(
        spec: ProcessSpec,
        adapter: Box<dyn Adapter>,
        initial: Step,
        store: Store,
        session_id: Option<String>,
        notify: impl Fn(Update) + Send + Sync + 'static,
    ) -> Result<Self, String> {
        Self::start_inner(spec, adapter, initial, store, session_id, None, notify)
    }

    pub(crate) fn start_owned(
        spec: ProcessSpec,
        adapter: Box<dyn Adapter>,
        initial: Step,
        store: Store,
        source: super::Session,
        recovery_environment: BTreeMap<String, String>,
        notify: impl Fn(Update) + Send + Sync + 'static,
    ) -> Result<Self, String> {
        let id = source.launch.session_id.clone();
        Self::start_inner(
            spec,
            adapter,
            initial,
            store,
            id,
            Some(NativeOwnership::New(source, recovery_environment)),
            notify,
        )
    }

    pub(crate) fn start_adopted(
        mut spec: ProcessSpec,
        adapter: Box<dyn Adapter>,
        initial: Step,
        store: Store,
        recovery: super::native_holder::Recovery,
        holder: super::native_holder::Holder,
        notify: impl Fn(Update) + Send + Sync + 'static,
    ) -> Result<Self, String> {
        let server = recovery
            .server_process
            .ok_or("Missing retained app-server process identity")?;
        spec.environment = recovery.execution_environment.0;
        Self::start_inner(
            spec,
            adapter,
            initial,
            store,
            recovery.confirmed_thread,
            Some(NativeOwnership::Adopted(holder, server)),
            notify,
        )
    }

    fn start_inner(
        mut spec: ProcessSpec,
        adapter: Box<dyn Adapter>,
        initial: Step,
        store: Store,
        session_id: Option<String>,
        ownership: Option<NativeOwnership>,
        notify: impl Fn(Update) + Send + Sync + 'static,
    ) -> Result<Self, String> {
        spec.remove_unset_environment();
        // Compare the reconstructible launch context, before the daemon adds
        // its transient capability and matched CLI path. Never persist those
        // additions or weaken comparisons of the user's selected environment.
        let recovery_environment = match &ownership {
            Some(NativeOwnership::New(_, environment)) => environment.clone(),
            _ => spec.environment.clone(),
        };
        if !matches!(&ownership, Some(NativeOwnership::Adopted(..)))
            && let Some(context) = &spec.execution
        {
            context.verify_directory(&spec.working_directory)?;
        }
        let mut owned_socket = None;
        if let TransportSpec::OwnedUnixWebSocket { socket } = &spec.transport {
            use std::os::unix::fs::DirBuilderExt;
            let directory = socket
                .parent()
                .ok_or("Provider socket needs a parent directory")?;
            let created = std::fs::DirBuilder::new().mode(0o700).create(directory);
            if let Err(error) = created {
                use std::os::unix::fs::MetadataExt;
                let safe_existing = error.kind() == std::io::ErrorKind::AlreadyExists
                    && std::fs::symlink_metadata(directory).is_ok_and(|metadata| {
                        metadata.is_dir()
                            && metadata.uid() == unsafe { libc::geteuid() }
                            && metadata.mode() & 0o777 == 0o700
                    });
                if !safe_existing {
                    return Err(format!(
                        "Cannot create private provider socket directory: {error}"
                    ));
                }
            }
            if ownership.is_none() {
                owned_socket = Some(OwnedSocket(socket.clone()));
            }
        }
        let expected_native_peer = match &ownership {
            Some(NativeOwnership::Adopted(_, server)) => Some(server.clone()),
            _ => None,
        };
        let adopted_native = expected_native_peer.is_some();
        let native_installed = Arc::new(AtomicBool::new(false));
        let native_holder = match ownership {
            Some(NativeOwnership::New(source, _))
                if matches!(spec.transport, TransportSpec::OwnedUnixWebSocket { .. }) =>
            {
                Some(super::native_holder::spawn(spec.clone(), source)?)
            }
            Some(NativeOwnership::Adopted(holder, _)) => Some(holder),
            _ => None,
        };
        let (wake, reader) = UnixStream::pair().map_err(|e| e.to_string())?;
        nonblocking(&wake)?;
        nonblocking(&reader)?;
        let (commands, incoming) = crossbeam_channel::bounded(64);
        let state = Arc::new(Mutex::new(Snapshot {
            session_id,
            ..Snapshot::default()
        }));
        let visible = Arc::new(AtomicBool::new(false));
        let observe_next = Arc::new(AtomicBool::new(true));
        let stop = Arc::new(AtomicBool::new(false));
        let completed = Arc::new(AtomicBool::new(false));
        let draining = Arc::new(AtomicBool::new(false));
        let failure = Arc::new(Mutex::new(None));
        let store = Arc::new(Mutex::new(store));
        let confirmed_session = Arc::new(Mutex::new(None));
        let preserve_native = Arc::new(AtomicBool::new(false));
        let release_native = Arc::new(AtomicBool::new(false));
        let retain_cleanup_receipt = Arc::new(AtomicBool::new(false));
        let detached_native = Arc::new(Mutex::new(None));
        let retained_native = Arc::new(Mutex::new(None));
        let notify: Arc<dyn Fn(Update) + Send + Sync> = Arc::new(notify);
        let inner = Arc::new(Inner {
            commands,
            wake: Mutex::new(wake),
            state: state.clone(),
            confirmed_session: confirmed_session.clone(),
            visible: visible.clone(),
            observe_next: observe_next.clone(),
            stop: stop.clone(),
            completed: completed.clone(),
            draining: draining.clone(),
            failure: failure.clone(),
            worker: Mutex::new(None),
            store: store.clone(),
            notify: notify.clone(),
            execution_environment: crate::terminal_ipc::ChatExecutionEnvironment(
                spec.environment.clone(),
            ),
            recovery_environment,
            preserve_native: preserve_native.clone(),
            release_native: release_native.clone(),
            retain_cleanup_receipt: retain_cleanup_receipt.clone(),
            detached_native: detached_native.clone(),
            retained_native: retained_native.clone(),
            native_installed: native_installed.clone(),
        });
        let launch_context = spec.execution.clone();
        let (begin, ready_to_begin) = crossbeam_channel::bounded(1);
        let (started, startup) = crossbeam_channel::bounded(1);
        let worker = thread::Builder::new()
            .name("cagents-chat".into())
            .spawn(move || {
                let _owned_socket = owned_socket;
                let mut actor = Actor {
                    launch_context: spec.execution.clone(),
                    working_directory: spec.working_directory.clone(),
                    child: None,
                    native_holder,
                    reconnect_only_reported: false,
                    expected_native_peer,
                    native_peer: None,
                    preserve_native: preserve_native.clone(),
                    native_installed: native_installed.clone(),
                    adapter,
                    state,
                    confirmed_session,
                    store,
                    visible,
                    observe_next,
                    stop,
                    incoming,
                    wake: reader,
                    notify,
                    output: Vec::new(),
                    scanned: 0,
                    pending: VecDeque::new(),
                    offset: 0,
                    queued: 0,
                    stderr: VecDeque::new(),
                    indices: BTreeMap::new(),
                    retained: 0,
                    history_error: None,
                    dirty: false,
                    activity: ActivityState::Idle,
                    ready: false,
                    attention: TurnAttention::default(),
                    execution: execution::Tracker::default(),
                    attention_baselined: false,
                    last_present: Instant::now(),
                    first_frame: false,
                    started: Instant::now(),
                    write_started: None,
                };
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    ready_to_begin
                        .recv()
                        .map_err(|_| "Provider startup was cancelled".to_string())?;
                    if actor.stop.load(Ordering::Acquire) {
                        let _ = started.send(Err("Provider startup was cancelled".to_string()));
                        return Ok(());
                    }
                    let child = if actor.native_holder.is_some() {
                        Ok(None)
                    } else {
                        spawn_provider(&spec, &actor.stop)
                    };
                    let _ = started.send(child.as_ref().map(|_| ()).map_err(Clone::clone));
                    actor.child = child?;
                    match spec.transport {
                        TransportSpec::JsonLines => actor.run(initial),
                        TransportSpec::UnixWebSocket {
                            socket,
                            device,
                            inode,
                        } => actor.run_websocket(initial, &socket, device, inode),
                        TransportSpec::OwnedUnixWebSocket { socket } => {
                            actor.run_launched_websocket(initial, &socket)
                        }
                    }
                }))
                .unwrap_or_else(|_| {
                    Err("Native chat worker panicked; exact provider state is unconfirmed".into())
                });
                // The actor has stopped answering commands, yet teardown below can
                // take seconds. Reject commands now so a caller is told the provider
                // exited instead of waiting out an acknowledgement nobody will send.
                draining.store(true, Ordering::Release);
                let unexpected_loss = result.is_err() && !actor.stop.load(Ordering::Acquire);
                let preserve_holder = actor
                    .native_holder
                    .as_ref()
                    .is_some_and(|holder| holder.is_durable())
                    && ((preserve_native.load(Ordering::Acquire)
                        || (unexpected_loss && native_installed.load(Ordering::Acquire)))
                        && !release_native.load(Ordering::Acquire)
                        || (adopted_native && !native_installed.load(Ordering::Acquire)));
                if let Err(error) = result {
                    // A lost local actor does not prove that the retained
                    // provider turn failed. Recovery must obtain its outcome.
                    if !actor.attention.settled && !preserve_holder {
                        (actor.notify)(Update::Attention(crate::notifications::Evidence {
                            key: actor.attention.key.clone(),
                            kind: crate::notifications::Kind::Failed,
                        }));
                    }
                    *failure.lock().unwrap_or_else(|p| p.into_inner()) = Some(error.clone());
                    let stderr =
                        String::from_utf8_lossy(&actor.stderr.make_contiguous()).into_owned();
                    let message = if stderr.is_empty() {
                        error
                    } else {
                        format!("{error}\nProvider stderr (last 16 KiB): {stderr}")
                    };
                    actor.apply(Event::Error {
                        message,
                        fatal: true,
                    });
                }
                let mut direct_closed = true;
                let code = actor.child.as_mut().and_then(|child| {
                    let code = child
                        .observed_exit_status()
                        .ok()
                        .flatten()
                        .and_then(|s| s.code());
                    if let Err(error) = child.terminate() {
                        direct_closed = false;
                        *failure.lock().unwrap_or_else(|p| p.into_inner()) =
                            Some(format!("Provider cleanup is unconfirmed: {error}"));
                    }
                    code
                });
                let native_closed = if let Some(mut holder) = actor.native_holder.take() {
                    if preserve_holder {
                        *detached_native.lock().unwrap_or_else(|p| p.into_inner()) =
                            Some(holder.preserve());
                        true
                    } else if let Err(error) =
                        holder.close_inner(retain_cleanup_receipt.load(Ordering::Acquire))
                    {
                        *failure.lock().unwrap_or_else(|p| p.into_inner()) = Some(error);
                        *retained_native.lock().unwrap_or_else(|p| p.into_inner()) = Some(holder);
                        false
                    } else {
                        true
                    }
                } else {
                    true
                };
                actor.apply(Event::Exited { code });
                let usage = actor
                    .state
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .usage
                    .clone();
                for (run, update) in actor
                    .execution
                    .ended(actor.stop.load(Ordering::Acquire), usage)
                {
                    (actor.notify)(Update::Execution { run, update });
                }
                actor.publish_activity();
                completed.store(native_closed && direct_closed, Ordering::Release);
                actor.observe_update();
                if actor.visible.load(Ordering::Acquire) {
                    (actor.notify)(Update::Presentation);
                }
            })
            .map_err(|e| e.to_string())?;
        *inner.worker.lock().unwrap_or_else(|p| p.into_inner()) = Some(worker);
        let handle = Self(inner);
        if !adopted_native && let Some(context) = &launch_context {
            context.claim_owner(Arc::new(handle.clone()));
        }
        // No child can exist before its exact actor cleanup capability and
        // join handle are installed. Close joins this worker even if startup
        // races cancellation, so an expired wait cannot permit a late spawn.
        let _ = begin.send(());
        match startup.recv_timeout(IO_TIMEOUT) {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                use super::protocol::LaunchCleanup;
                if handle.cleanup().is_ok() || launch_context.is_some() {
                    return Err(error);
                }
                // Interactive launches have no execution journal. Preserve
                // their exact actor for host publication if cleanup is unknown.
            }
            Err(error) if launch_context.is_some() => {
                return Err(format!(
                    "Provider startup did not finish: {error}; its exact cleanup owner is retained"
                ));
            }
            Err(_) => {} // The host publishes the still-owned interactive actor.
        }
        Ok(handle)
    }
    pub fn command(&self, command: Command) -> Result<(), String> {
        self.submit_command(None, command).map_err(String::from)
    }

    /// Like [`Self::command`], but tells a refusal, after which the command
    /// may be offered again, from a timeout, after which it may still run.
    pub(crate) fn try_command(&self, command: Command) -> Result<(), Rejection> {
        self.submit_command(None, command)
    }

    pub(crate) fn command_for_run(
        &self,
        run: crate::model::RunId,
        command: Command,
    ) -> Result<(), String> {
        self.submit_command(Some(run), command)
            .map_err(String::from)
    }

    fn submit_command(
        &self,
        run: Option<crate::model::RunId>,
        command: Command,
    ) -> Result<(), Rejection> {
        // `completed` is published only once teardown finished; `draining` the moment
        // the actor stopped consuming its command channel.
        let exiting = self.is_completed() || self.0.draining.load(Ordering::Acquire);
        if exiting && matches!(command, Command::LoadEarlier | Command::Refresh) {
            let cached = page_command(&self.0.store, &command).map_err(Rejection::Refused)?;
            if command == Command::LoadEarlier && !cached {
                return Err(Rejection::Refused("Earlier cached history is exhausted; the exited provider must be explicitly resumed to load more".into()));
            }
            self.0
                .state
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .generation += 1;
            if self.0.visible.load(Ordering::Acquire) {
                (self.0.notify)(Update::Presentation);
            }
            return Ok(());
        }
        if exiting {
            return Err(Rejection::Refused(
                "Provider process exited; explicitly recover the exact session".into(),
            ));
        }
        if self.0.stop.load(Ordering::Acquire) {
            return Err(Rejection::Refused("Chat is closing".into()));
        }
        let (reply, response) = crossbeam_channel::bounded(1);
        self.0
            .commands
            .try_send((ActorCommand::Provider { run, command }, reply))
            .map_err(|e| Rejection::Refused(format!("Chat command queue unavailable: {e}")))?;
        self.wake();
        response
            .recv_timeout(Duration::from_secs(10))
            .map_err(|_| {
                Rejection::Unacknowledged(
                    "Provider command acknowledgement timed out; do not retry blindly".into(),
                )
            })?
            .map_err(Rejection::Refused)
    }
    fn ownership_command(&self, command: ActorCommand) -> Result<(), String> {
        if self.is_completed()
            || self.0.draining.load(Ordering::Acquire)
            || self.0.stop.load(Ordering::Acquire)
        {
            return Err("The provider actor is no longer accepting ownership commands".into());
        }
        let (reply, response) = crossbeam_channel::bounded(1);
        self.0
            .commands
            .try_send((command, reply))
            .map_err(|error| format!("Chat command queue unavailable: {error}"))?;
        self.wake();
        response
            .recv_timeout(Duration::from_secs(10))
            .map_err(|_| "Provider ownership command acknowledgement timed out".to_string())?
    }

    pub(crate) fn close_if_idle(&self, generation: u64) -> Result<(), String> {
        self.ownership_command(ActorCommand::CloseIfIdle { generation })?;
        self.close()
    }

    pub(crate) fn close_if_idle_for_resume(&self, generation: u64) -> Result<(), String> {
        // Publish retention before the actor's close fence: it may begin
        // teardown immediately after acknowledging that fence.
        if matches!(
            self.native_continuity(),
            super::NativeContinuity::Durable { .. }
        ) {
            self.0.retain_cleanup_receipt.store(true, Ordering::Release);
        }
        self.ownership_command(ActorCommand::CloseIfIdle { generation })?;
        self.close_for_resume()
    }

    /// Checks readiness on the actor after all previously queued commands.
    /// The caller must hold the host's exclusive control gate until checkpoint
    /// preparation finishes: a snapshot alone cannot fence a command whose
    /// caller timed out while the actor was still admitting it.
    /// This barrier neither interrupts nor stops the provider.
    pub(crate) fn check_update_readiness(&self) -> Result<UpdateReadiness, String> {
        let (reply, response) = crossbeam_channel::bounded(1);
        self.ownership_command(ActorCommand::CheckUpdateReadiness(reply))?;
        response
            .try_recv()
            .map_err(|error| format!("Provider update readiness was not acknowledged: {error}"))
    }

    pub(crate) fn interrupt_run(&self, run: crate::model::RunId) -> Result<(), String> {
        self.ownership_command(ActorCommand::InterruptRun(run))
    }

    fn wake(&self) {
        let _ = self
            .0
            .wake
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .write(&[1]);
    }
    pub(crate) fn update_native_source(
        &self,
        source: super::Session,
        execution_console: Option<crate::model::LaunchRecipe>,
        paired_holder: Option<crate::terminal::holder::Record>,
    ) -> Result<(), String> {
        if !matches!(
            self.native_continuity(),
            super::NativeContinuity::Durable { .. }
        ) {
            return Ok(());
        }
        let (reply, response) = crossbeam_channel::bounded(1);
        self.0
            .commands
            .try_send((
                ActorCommand::UpdateSource(source, execution_console, paired_holder),
                reply,
            ))
            .map_err(|error| format!("Native ownership queue unavailable: {error}"))?;
        self.wake();
        response
            .recv_timeout(Duration::from_secs(10))
            .map_err(|_| {
                "Native ownership acknowledgement timed out; retain the previous descriptor"
                    .to_string()
            })?
    }
    pub fn set_visible(&self, visible: bool) {
        self.0.visible.store(visible, Ordering::Release);
        self.wake();
    }
    /// Rearm provider observation and return its current semantic boundary.
    /// Lock only resident metadata; never hydrate or clone the transcript.
    pub(crate) fn observe_next_update(&self) -> u64 {
        let state = self.0.state.lock().unwrap_or_else(|p| p.into_inner());
        self.0.observe_next.store(true, Ordering::Release);
        state.generation
    }
    pub fn snapshot(&self, after: Option<u64>) -> Option<Snapshot> {
        let mut store = self.0.store.lock().unwrap_or_else(|p| p.into_inner());
        let mut state = self.0.state.lock().unwrap_or_else(|p| p.into_inner());
        if after.is_some_and(|generation| generation >= state.generation) && !store.dirty {
            return None;
        }
        let refresh_publication =
            store.presented_generation != Some(state.generation) || store.dirty;
        if store.dirty {
            match store.window_reusing(&mut state.items) {
                Ok(window) => {
                    state.items = window.items;
                    state.has_earlier = window.has_earlier;
                    state.has_later = window.has_later;
                }
                Err(error) => {
                    state.error = Some(format!("Cannot page owned transcript cache: {error}"))
                }
            }
        }
        if refresh_publication {
            store.presented_generation = Some(state.generation);
            store.presentation = None;
            store.presented_children = state
                .subagents
                .iter()
                .map(|agent| (agent.id.clone(), agent.items.clone()))
                .collect();
        }
        let mut snapshot = state.clone();
        snapshot.sampled_at = observed_at();
        Some(snapshot)
    }
    /// The last publication is retained for other readers of the same revision.
    /// Readers that missed it receive the authoritative full snapshot instead.
    pub(crate) fn presentation(
        &self,
        chat_id: crate::model::TerminalId,
        after: Option<u64>,
    ) -> Option<crate::terminal_ipc::ServerResponse> {
        let mut store = self.0.store.lock().unwrap_or_else(|p| p.into_inner());
        let mut state = self.0.state.lock().unwrap_or_else(|p| p.into_inner());
        if after == Some(state.generation) && !store.dirty {
            return None;
        }
        if store.presented_generation != Some(state.generation) || store.dirty {
            let baseline = store.presented_generation;
            let changes = if store.dirty {
                match store.window_reusing(&mut state.items) {
                    Ok(window) => {
                        state.items = window.items;
                        state.has_earlier = window.has_earlier;
                        state.has_later = window.has_later;
                        window.changes
                    }
                    Err(error) => {
                        state.error = Some(format!("Cannot page owned transcript cache: {error}"));
                        Vec::new()
                    }
                }
            } else {
                Vec::new()
            };
            store.presentation = baseline
                .filter(|generation| *generation < state.generation)
                .map(|base_generation| {
                    let children = state
                        .subagents
                        .iter()
                        .map(|agent| {
                            let previous: BTreeMap<_, _> = store
                                .presented_children
                                .get(&agent.id)
                                .into_iter()
                                .flatten()
                                .map(|item| (item.id.as_str(), item))
                                .collect();
                            crate::chat::presentation::ChildUpdate {
                                id: agent.id.clone(),
                                order: agent.items.iter().map(|item| item.id.clone()).collect(),
                                changes: agent
                                    .items
                                    .iter()
                                    .filter(|item| {
                                        previous
                                            .get(item.id.as_str())
                                            .is_none_or(|old| !Arc::ptr_eq(old, item))
                                    })
                                    .map(|item| {
                                        crate::chat::presentation::ItemChange::between(
                                            previous.get(item.id.as_str()).map(|old| old.as_ref()),
                                            item,
                                        )
                                    })
                                    .collect(),
                            }
                        })
                        .collect();
                    let items = std::mem::take(&mut state.items);
                    let child_items: Vec<_> = state
                        .subagents
                        .iter_mut()
                        .map(|agent| std::mem::take(&mut agent.items))
                        .collect();
                    let metadata = state.clone();
                    state.items = items;
                    for (agent, items) in state.subagents.iter_mut().zip(child_items) {
                        agent.items = items;
                    }
                    crate::chat::presentation::Presentation {
                        base_generation,
                        metadata,
                        order: state.items.iter().map(|item| item.id.clone()).collect(),
                        changes,
                        children,
                    }
                });
            store.presented_generation = Some(state.generation);
            store.presented_children = state
                .subagents
                .iter()
                .map(|agent| (agent.id.clone(), agent.items.clone()))
                .collect();
        }
        if let Some(presentation) = store
            .presentation
            .as_ref()
            .filter(|update| after == Some(update.base_generation))
        {
            let mut update = presentation.clone();
            update.metadata.sampled_at = observed_at();
            Some(crate::terminal_ipc::ServerResponse::ChatUpdate { chat_id, update })
        } else {
            let mut snapshot = state.clone();
            snapshot.sampled_at = observed_at();
            Some(crate::terminal_ipc::ServerResponse::ChatSnapshot {
                chat_id,
                snapshot: Some(snapshot),
            })
        }
    }
    pub fn session_id(&self) -> Option<String> {
        self.0
            .state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .session_id
            .clone()
    }
    /// Identity received from the provider, not the speculative launch selector.
    pub(crate) fn confirmed_session_id(&self) -> Option<String> {
        self.0
            .confirmed_session
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }
    pub(crate) fn execution_environment(&self) -> crate::terminal_ipc::ChatExecutionEnvironment {
        self.0.execution_environment.clone()
    }
    pub(crate) fn recovery_environment_matches(
        &self,
        environment: &BTreeMap<String, String>,
    ) -> bool {
        &self.0.recovery_environment == environment
    }
    pub fn is_completed(&self) -> bool {
        self.0.completed.load(Ordering::Acquire)
    }
    /// The recorded failure, without rebuilding the transcript window. Restore polls
    /// this; it must never pay for a multi-megabyte snapshot.
    pub fn failure(&self) -> Option<String> {
        if let Some(error) = self
            .0
            .failure
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
        {
            return Some(error);
        }
        self.0
            .state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .error
            .clone()
    }
    pub fn owns_work(&self) -> bool {
        !self.is_completed()
            || self
                .0
                .failure
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .is_some()
    }
    /// What the conversation is doing, read without loading its transcript.
    pub(crate) fn activity(&self) -> crate::agent::ActivityState {
        self.0
            .state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .activity()
    }

    /// Update preflights need activity without loading the transcript window.
    pub(crate) fn update_activity(&self) -> (bool, bool) {
        let state = self.0.state.lock().unwrap_or_else(|p| p.into_inner());
        (
            state.is_working() || state.waiting || !state.requests.is_empty(),
            state.ready,
        )
    }
    /// Whether the conversation would take a prompt now without interrupting
    /// a turn or passing over a question it put to the user.
    pub(crate) fn is_idle(&self) -> bool {
        let state = self.0.state.lock().unwrap_or_else(|p| p.into_inner());
        state.ready
            && !state.busy
            && !state.waiting
            && !state.exited
            && state.requests.is_empty()
            && !self.is_completed()
            && !self.0.draining.load(Ordering::Acquire)
    }
    /// Pending questions and approvals, read without loading the transcript.
    pub(crate) fn requests(&self) -> Vec<super::Request> {
        self.0
            .state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .requests
            .clone()
    }
    /// The provider-reported title, read without loading the transcript.
    pub(crate) fn title(&self) -> Option<String> {
        self.0
            .state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .title
            .clone()
    }
    #[cfg(test)]
    pub(crate) fn resource_lifetime(&self) -> impl Fn() -> bool + use<> {
        let inner = Arc::downgrade(&self.0);
        move || inner.upgrade().is_some()
    }
    pub(crate) fn commit_native_installation(&self) {
        self.0.native_installed.store(true, Ordering::Release);
    }

    pub(crate) fn native_continuity(&self) -> super::NativeContinuity {
        self.0
            .state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .native_continuity
            .clone()
    }

    pub(crate) fn preserve_for_restart(&self) -> Result<(), String> {
        if !matches!(
            self.native_continuity(),
            super::NativeContinuity::Durable { .. }
        ) {
            return Err("This native provider has no verified durable owner".into());
        }
        if self.0.release_native.load(Ordering::Acquire) {
            return Err("Native provider has already been explicitly closed".into());
        }
        self.0.preserve_native.store(true, Ordering::Release);
        self.close_inner(true)
    }

    pub(crate) fn close_for_resume(&self) -> Result<(), String> {
        if matches!(
            self.native_continuity(),
            super::NativeContinuity::Durable { .. }
        ) {
            self.0.retain_cleanup_receipt.store(true, Ordering::Release);
        }
        self.close()
    }

    pub fn close(&self) -> Result<(), String> {
        if self.0.native_installed.load(Ordering::Acquire) {
            self.0.release_native.store(true, Ordering::Release);
        }
        self.close_inner(false)
    }

    fn close_inner(&self, preserving: bool) -> Result<(), String> {
        self.0.stop.store(true, Ordering::Release);
        self.wake();
        // Serialize explicit closes through the join. A protocol failure is
        // still useful diagnostic history, but it must not prevent retiring an
        // actor whose process teardown has subsequently completed.
        let mut worker = self.0.worker.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(worker) = worker.take()
            && worker.join().is_err()
        {
            let error =
                "Native chat worker did not shut down cleanly; provider state is unconfirmed"
                    .to_owned();
            // Even if the panic happened after the completion publication,
            // repeated closes must preserve the uncertain teardown outcome.
            self.0.completed.store(false, Ordering::Release);
            *self.0.failure.lock().unwrap_or_else(|p| p.into_inner()) = Some(error.clone());
            return Err(error);
        }
        {
            let mut retained = self
                .0
                .retained_native
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            if let Some(holder) = retained.as_mut() {
                holder.close_inner(self.0.retain_cleanup_receipt.load(Ordering::Acquire))?;
                retained.take();
                self.0.completed.store(true, Ordering::Release);
            }
        }
        if !preserving && self.0.native_installed.load(Ordering::Acquire) {
            let mut detached = self
                .0
                .detached_native
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            if let Some(owner) = detached.as_ref() {
                if self.0.retain_cleanup_receipt.load(Ordering::Acquire) {
                    owner.close_retaining_receipt()?;
                } else {
                    owner.close()?;
                }
                detached.take();
            }
        }
        if self.is_completed() {
            return Ok(());
        }
        match self
            .0
            .failure
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
        {
            Some(error) => Err(error),
            None => {
                Err("Native chat teardown did not complete; provider state is unconfirmed".into())
            }
        }
    }
}

/// Validate local launch and adapter initialization before stopping an existing CLI.
pub(crate) fn preflight(spec: &ProcessSpec, initial: &mut Step) -> Result<Store, String> {
    preflight_inner(spec, initial, false)
}

/// Only an authenticated retained server can restore its read-only connection
/// after the original directory was removed. Authorizing commands still check
/// the retained execution policy in the actor before any provider write.
pub(crate) fn preflight_retained(spec: &ProcessSpec, initial: &mut Step) -> Result<Store, String> {
    if !matches!(spec.transport, TransportSpec::UnixWebSocket { .. }) {
        return Err("Retained preflight cannot launch a provider".into());
    }
    preflight_inner(spec, initial, true)
}

fn preflight_inner(
    spec: &ProcessSpec,
    initial: &mut Step,
    retained: bool,
) -> Result<Store, String> {
    use std::os::unix::ffi::OsStrExt;
    if !retained && !spec.working_directory.is_dir() {
        return Err("Provider working directory does not exist".into());
    }
    if spec.program.is_empty()
        || spec.program.contains('\0')
        || spec.arguments.iter().any(|arg| arg.contains('\0'))
        || spec
            .environment
            .iter()
            .any(|(key, value)| key.is_empty() || key.contains(['\0', '=']) || value.contains('\0'))
    {
        return Err(
            "Provider launch contains invalid executable, argument, or environment data".into(),
        );
    }
    if matches!(
        spec.transport,
        TransportSpec::JsonLines | TransportSpec::OwnedUnixWebSocket { .. }
    ) {
        let paths = if spec.program.contains('/') {
            vec![spec.working_directory.join(&spec.program)]
        } else {
            let path = spec
                .environment
                .get("PATH")
                .cloned()
                .unwrap_or_else(|| "/usr/bin:/bin".into());
            std::env::split_paths(&path)
                .map(|directory| spec.working_directory.join(directory).join(&spec.program))
                .collect()
        };
        if !paths.iter().any(|path| {
            path.is_file()
                && std::ffi::CString::new(path.as_os_str().as_bytes())
                    .is_ok_and(|path| unsafe { libc::access(path.as_ptr(), libc::X_OK) } == 0)
        }) {
            return Err(format!(
                "Provider executable {} is not executable in the preserved launch environment",
                spec.program
            ));
        }
    }
    let mut queued = 0usize;
    for frame in &initial.writes {
        let mut count = ByteCounter(0);
        serde_json::to_writer(&mut count, frame).map_err(|error| error.to_string())?;
        if count.0 > FRAME_LIMIT {
            return Err("Provider initialization frame exceeds 32 MiB".into());
        }
        queued = queued.saturating_add(count.0 + 1);
    }
    if queued > QUEUE_LIMIT {
        return Err("Provider initialization exceeds stdin queue capacity".into());
    }
    let mut store = Store::new()?;
    store.recognize_messages(spec.messages.clone());
    let mut metadata = Vec::new();
    for event in initial.events.drain(..) {
        if let Event::Error {
            message,
            fatal: true,
        } = &event
        {
            return Err(message.clone());
        }
        metadata.extend(store.apply(event)?);
    }
    let mut count = ByteCounter(0);
    serde_json::to_writer(&mut count, &metadata).map_err(|error| error.to_string())?;
    // Child transcripts are bounded presentation items, not long-lived
    // request/model metadata. The actor checks that metadata alone stays
    // under STATE_LIMIT after applying this initial batch.
    if count.0 > 4 * STATE_LIMIT {
        return Err("Provider initial presentation exceeds IPC capacity".into());
    }
    initial.events = metadata;
    Ok(store)
}
impl Drop for Inner {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        let _ = self
            .wake
            .get_mut()
            .unwrap_or_else(|p| p.into_inner())
            .write(&[1]);
        if let Some(worker) = self
            .worker
            .get_mut()
            .unwrap_or_else(|p| p.into_inner())
            .take()
        {
            let _ = worker.join();
        }
    }
}
impl super::protocol::LaunchCleanup for Handle {
    fn cleanup(&self) -> Result<super::protocol::LaunchOutcome, String> {
        self.0.stop.store(true, Ordering::Release);
        self.wake();
        let deadline = Instant::now() + IO_TIMEOUT + super::TERMINATE_GRACE;
        while !self.is_completed()
            && !self.0.worker.try_lock().is_ok_and(|worker| {
                worker
                    .as_ref()
                    .is_none_or(std::thread::JoinHandle::is_finished)
            })
        {
            if Instant::now() >= deadline {
                return Err(
                    "Provider startup has not become quiescent; its cleanup owner is retained"
                        .into(),
                );
            }
            thread::sleep(Duration::from_millis(10));
        }
        self.close()?;
        Ok(super::protocol::LaunchOutcome::TeardownProved)
    }
}

fn spawn_provider(spec: &ProcessSpec, stop: &AtomicBool) -> Result<Option<ChildGuard>, String> {
    if matches!(spec.transport, TransportSpec::UnixWebSocket { .. }) {
        return Ok(None);
    }
    if let Some(context) = &spec.execution {
        context.verify_directory(&spec.working_directory)?;
    }
    let pipes = matches!(spec.transport, TransportSpec::JsonLines);
    let mut process = Process::new(&spec.program);
    process
        .args(&spec.arguments)
        .current_dir(&spec.working_directory)
        .env_clear()
        .envs(&spec.environment)
        .stdin(if pipes { Stdio::piped() } else { Stdio::null() })
        .stdout(if pipes { Stdio::piped() } else { Stdio::null() })
        .stderr(if pipes { Stdio::piped() } else { Stdio::null() })
        .process_group(0);
    crate::host::descriptor_limit::inherit(&mut process);
    if let Some(directory) = spec
        .execution
        .as_ref()
        .and_then(|context| context.directory.clone())
    {
        // SAFETY: fchdir is async-signal-safe; the captured File remains open
        // across fork until the provider execs.
        unsafe {
            process.pre_exec(move || {
                if libc::fchdir(directory.as_raw_fd()) == 0 {
                    Ok(())
                } else {
                    Err(std::io::Error::last_os_error())
                }
            });
        }
    }
    let _account =
        crate::account::registry::authorize_command(&process).map_err(|e| e.to_string())?;
    if stop.load(Ordering::Acquire) {
        return Err("Provider startup was cancelled before process creation".into());
    }
    let child = process
        .spawn()
        .map_err(|e| format!("Cannot start {}: {e}", spec.program))?;
    let child = ChildGuard::new(child)
        .map_err(|e| format!("Cannot retain {} process identity: {e}", spec.program))?;
    // Scope enrollment happens before this child is handed to the actor and
    // therefore before any prompt/tool request. Keep the original Child, pipes,
    // process group and account lease: systemd only relocates its cgroup.
    if let Err(error) = crate::daemon::process_scope::enroll_process(child.child.id()) {
        tracing::warn!(%error, provider = %spec.program, "provider scope isolation unavailable; using existing launch fallback");
    }
    crate::daemon::process_scope::warn_if_shared(child.child.id(), child.child.id());
    crate::daemon::process_scope::warn_if_bootstrap_shared(child.child.id());
    Ok(Some(child))
}

/// Sole reaper of the provider leader. Observations retain its zombie until
/// every group signal is finished, so its numeric PID/PGID cannot be recycled.
struct ChildGuard {
    child: Child,
    identity: OwnedFd,
    terminated: bool,
}
impl ChildGuard {
    /// Capture immediately after spawn, before any code can reap this child.
    fn new(mut child: Child) -> std::io::Result<Self> {
        // SAFETY: pidfd_open obtains a capability for our freshly spawned,
        // unreaped child. Its PID remains reserved even if it has exited.
        let descriptor = unsafe { libc::syscall(libc::SYS_pidfd_open, child.id(), 0) };
        if descriptor < 0 {
            let error = std::io::Error::last_os_error();
            // Startup has not handed the child to another owner. Keep the
            // leader unreaped while cleaning its original process group.
            let group_cleanup = if unsafe { libc::kill(-(child.id() as i32), libc::SIGKILL) } == 0 {
                Ok(())
            } else {
                let error = std::io::Error::last_os_error();
                if error.raw_os_error() == Some(libc::ESRCH) {
                    Ok(())
                } else {
                    Err(std::io::Error::new(
                        error.kind(),
                        format!("process group: {error}"),
                    ))
                }
            };
            let cleanup = child
                .kill()
                .and_then(|()| child.wait().map(|_| ()))
                .and(group_cleanup);
            return Err(match cleanup {
                Ok(()) => error,
                Err(cleanup) => std::io::Error::new(
                    error.kind(),
                    format!("{error}; provider startup cleanup is unconfirmed: {cleanup}"),
                ),
            });
        }
        // SAFETY: successful pidfd_open returned a new descriptor owned here.
        let identity = unsafe { OwnedFd::from_raw_fd(descriptor as i32) };
        Ok(Self {
            child,
            identity,
            terminated: false,
        })
    }

    fn observed_exit_status(&self) -> std::io::Result<Option<std::process::ExitStatus>> {
        use std::os::unix::process::ExitStatusExt;
        let mut information: libc::siginfo_t = unsafe { std::mem::zeroed() };
        loop {
            // SAFETY: the pidfd identifies the original child despite numeric PID
            // reuse and initializes the siginfo buffer. WNOWAIT leaves all
            // reaping to terminate below.
            let result = unsafe {
                libc::waitid(
                    libc::P_PIDFD,
                    self.identity.as_raw_fd() as libc::id_t,
                    &mut information,
                    libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
                )
            };
            if result == 0 {
                break;
            }
            let error = std::io::Error::last_os_error();
            if error.kind() != std::io::ErrorKind::Interrupted {
                return Err(error);
            }
        }
        // SAFETY: successful waitid initialized the child status fields.
        let (pid, status) = unsafe { (information.si_pid(), information.si_status()) };
        if pid == 0 {
            return Ok(None);
        }
        if pid as u32 != self.child.id() {
            return Err(std::io::Error::other(
                "Provider exit observation returned another child",
            ));
        }
        let raw = match information.si_code {
            libc::CLD_EXITED => status << 8,
            libc::CLD_KILLED => status,
            libc::CLD_DUMPED => status | 0x80,
            _ => {
                return Err(std::io::Error::other(
                    "Provider exit observation returned an unexpected status",
                ));
            }
        };
        Ok(Some(std::process::ExitStatus::from_raw(raw)))
    }

    fn terminate(&mut self) -> std::io::Result<()> {
        if self.terminated {
            return Ok(());
        }
        // ECHILD or another observation failure does not establish authority
        // over the old numeric group. Refuse before any signal in that case.
        self.observed_exit_status()?;
        let pid = self.child.id() as i32;
        let signal_group = |signal| {
            // SAFETY: the sole-reaper guard keeps our leader unreaped until
            // after both signals, reserving its numeric group identity.
            if unsafe { libc::kill(-pid, signal) } == 0 {
                return Ok(());
            }
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::ESRCH) {
                Ok(())
            } else {
                Err(error)
            }
        };
        signal_group(libc::SIGTERM)?;
        let deadline = Instant::now() + super::TERMINATE_GRACE;
        while Instant::now() < deadline {
            if self.observed_exit_status()?.is_some() {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        // Signal any remaining original group members while the exited
        // leader still pins the ID, then perform the sole final reap.
        signal_group(libc::SIGKILL)?;
        self.child.wait()?;
        self.terminated = true;
        Ok(())
    }
}
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.terminate();
    }
}
fn next_newline(bytes: &[u8], scanned: &mut usize) -> Option<usize> {
    let start = *scanned;
    match bytes[start..].iter().position(|byte| *byte == b'\n') {
        Some(offset) => Some(start + offset),
        None => {
            *scanned = bytes.len();
            None
        }
    }
}

fn nonblocking(fd: &impl AsRawFd) -> Result<(), String> {
    let raw = fd.as_raw_fd();
    let flags = unsafe { libc::fcntl(raw, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(raw, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    Ok(())
}
struct Actor {
    launch_context: Option<Arc<super::protocol::ExecutionContext>>,
    working_directory: std::path::PathBuf,
    child: Option<ChildGuard>,
    native_holder: Option<super::native_holder::Holder>,
    /// Whether this conversation's lack of durable continuity was logged.
    reconnect_only_reported: bool,
    expected_native_peer: Option<crate::daemon::holder_registry::Process>,
    native_peer: Option<crate::daemon::holder_registry::Process>,
    preserve_native: Arc<AtomicBool>,
    native_installed: Arc<AtomicBool>,
    adapter: Box<dyn Adapter>,
    state: Arc<Mutex<Snapshot>>,
    confirmed_session: Arc<Mutex<Option<String>>>,
    store: Arc<Mutex<Store>>,
    visible: Arc<AtomicBool>,
    observe_next: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    incoming: crossbeam_channel::Receiver<CommandRequest>,
    wake: UnixStream,
    notify: Arc<dyn Fn(Update) + Send + Sync>,
    output: Vec<u8>,
    /// Prefix already checked for a delimiter in the incomplete stdio frame.
    scanned: usize,
    pending: VecDeque<Vec<u8>>,
    offset: usize,
    queued: usize,
    stderr: VecDeque<u8>,
    indices: BTreeMap<String, usize>,
    retained: usize,
    /// Only the active error published by the most recent history failure.
    /// Recovery must not erase an unrelated provider/cache error.
    history_error: Option<String>,
    dirty: bool,
    activity: ActivityState,
    /// Whether the provider was taking prompts when activity was last published.
    ready: bool,
    attention: TurnAttention,
    execution: execution::Tracker,
    attention_baselined: bool,
    last_present: Instant,
    first_frame: bool,
    started: Instant,
    write_started: Option<Instant>,
}

struct TurnAttention {
    key: String,
    settled: bool,
}

impl Default for TurnAttention {
    fn default() -> Self {
        Self {
            key: uuid::Uuid::new_v4().to_string(),
            settled: false,
        }
    }
}

impl TurnAttention {
    fn apply(&mut self, event: &Event) -> Option<crate::notifications::Evidence> {
        match event {
            Event::TurnStarted => *self = Self::default(),
            Event::TurnSettled => self.settled = true,
            Event::TurnFinished { key, outcome } => {
                return Some(crate::notifications::Evidence {
                    key: key.clone().unwrap_or_else(|| self.key.clone()),
                    kind: *outcome,
                });
            }
            _ => {}
        }
        None
    }
}
impl Actor {
    fn run(&mut self, initial: Step) -> Result<(), String> {
        let mut stdin = self
            .child
            .as_mut()
            .ok_or("Provider process unavailable")?
            .child
            .stdin
            .take()
            .ok_or("Provider stdin unavailable")?;
        let mut stdout = self
            .child
            .as_mut()
            .ok_or("Provider process unavailable")?
            .child
            .stdout
            .take()
            .ok_or("Provider stdout unavailable")?;
        let mut stderr = self
            .child
            .as_mut()
            .ok_or("Provider process unavailable")?
            .child
            .stderr
            .take()
            .ok_or("Provider stderr unavailable")?;
        nonblocking(&stdin)?;
        nonblocking(&stdout)?;
        nonblocking(&stderr)?;
        self.step(initial)?;
        let mut stdout_eof = false;
        let mut stderr_eof = false;
        let mut closing = None;
        let mut partial_started = None;
        let mut interrupt_sent = false;
        let mut interrupt_error = String::new();
        loop {
            if self.stop.load(Ordering::Acquire) {
                if closing.is_none() {
                    closing = Some(Instant::now());
                }
                let busy = self.state.lock().unwrap_or_else(|p| p.into_inner()).busy;
                if busy && !interrupt_sent && self.pending.is_empty() {
                    match self.adapter.command(Command::Interrupt) {
                        Ok(step) => {
                            self.step(step)?;
                            interrupt_sent = true;
                        }
                        // A just-submitted turn may not have its provider ID yet.
                        // Continue draining until it can be interrupted or the deadline expires.
                        Err(error) => interrupt_error = error,
                    }
                }
                if self.pending.is_empty()
                    && !self.state.lock().unwrap_or_else(|p| p.into_inner()).busy
                {
                    return Ok(());
                }
                if closing.is_some_and(|at: Instant| at.elapsed() > super::INTERRUPT_TIMEOUT) {
                    return Err(format!(
                        "Provider interruption was not confirmed within 30 seconds ({interrupt_error}); backend work may still be active. Inspect exact provider session before recovery"
                    ));
                }
            }
            self.drain_commands(closing.is_some())?;
            // Parse every frame that already arrived. Gating this on an empty stdin
            // queue froze streaming for a whole turn against any provider that reads
            // stdin only between turns, then killed the session by blaming its
            // framing; a stalled write has its own `write_started` deadline below.
            let mut frames_ready = false;
            let mut parsed = 0usize;
            loop {
                let Some(newline) = next_newline(&self.output, &mut self.scanned) else {
                    break;
                };
                if newline == 0 {
                    self.output.drain(..1);
                    self.scanned = 0;
                    continue;
                }
                if parsed == FRAMES_PER_POLL {
                    frames_ready = true;
                    break;
                }
                let frame = serde_json::from_slice(&self.output[..newline]).map_err(|error| {
                    format!("Unsupported provider stdout (expected JSON-lines): {error}")
                })?;
                self.output.drain(..=newline);
                self.scanned = 0;
                partial_started = None;
                parsed += 1;
                self.first_frame = true;
                let step = self.adapter.receive(frame)?;
                self.provider_step(step)?;
            }
            if self.output.len() > FRAME_LIMIT {
                return Err("Provider stdout frame exceeds 32 MiB".into());
            }
            // Only a genuinely incomplete line is on the framing deadline.
            if frames_ready || self.output.is_empty() {
                partial_started = None;
            } else {
                let since = partial_started.get_or_insert_with(Instant::now);
                if since.elapsed() > IO_TIMEOUT {
                    return Err(
                        "Provider JSON frame remained incomplete or blocked for 30 seconds".into(),
                    );
                }
            }
            if stdout_eof && self.pending.is_empty() {
                if !self.output.is_empty() {
                    return Err("Provider ended with an incomplete JSON line".into());
                }
                if let Some(status) = self
                    .child
                    .as_mut()
                    .ok_or("Provider process unavailable")?
                    .observed_exit_status()
                    .map_err(|error| error.to_string())?
                {
                    if status.success() {
                        return Ok(());
                    }
                    return Err(format!("Provider exited with {status}"));
                }
                return Err("Provider closed its protocol stream; process terminated to avoid an unobservable writer".into());
            }
            if !self.first_frame && self.started.elapsed() > IO_TIMEOUT {
                return Err(
                    "Provider initialization timed out without a JSON protocol frame".into(),
                );
            }
            if self
                .write_started
                .is_some_and(|at| at.elapsed() > IO_TIMEOUT)
            {
                return Err("Provider stdin stalled for 30 seconds".into());
            }
            if self.dirty
                && self.visible.load(Ordering::Acquire)
                && self.last_present.elapsed() >= PRESENT_INTERVAL
            {
                self.dirty = false;
                self.last_present = Instant::now();
                (self.notify)(Update::Presentation);
            }
            let mut fds = [
                libc::pollfd {
                    fd: self.wake.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: if stdout_eof || self.output.len() > FRAME_LIMIT {
                        -1
                    } else {
                        stdout.as_raw_fd()
                    },
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: if stderr_eof { -1 } else { stderr.as_raw_fd() },
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: if self.pending.is_empty() {
                        -1
                    } else {
                        stdin.as_raw_fd()
                    },
                    events: libc::POLLOUT,
                    revents: 0,
                },
            ];
            let timeout = if frames_ready
                || (self.queued <= QUEUE_BACKPRESSURE && !self.incoming.is_empty())
            {
                0
            } else if self.dirty && self.visible.load(Ordering::Acquire) {
                50
            } else if !self.first_frame
                || !self.output.is_empty()
                || self.write_started.is_some()
                || closing.is_some()
            {
                250
            } else {
                -1
            };
            if unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as _, timeout) } < 0 {
                let error = std::io::Error::last_os_error();
                if error.kind() != std::io::ErrorKind::Interrupted {
                    return Err(error.to_string());
                }
            }
            let mut buffer = [0u8; 8192];
            if fds[0].revents != 0 {
                while self.wake.read(&mut buffer).is_ok_and(|n| n != 0) {}
            }
            if fds[3].revents != 0 {
                if let Some(line) = self.pending.front() {
                    match stdin.write(&line[self.offset..]) {
                        Ok(0) => return Err("Provider closed stdin".into()),
                        Ok(n) => {
                            self.offset += n;
                            self.queued -= n;
                            if self.offset == line.len() {
                                self.pending.pop_front();
                                self.offset = 0;
                                self.write_started = if self.pending.is_empty() {
                                    None
                                } else {
                                    Some(Instant::now())
                                };
                            }
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                        Err(e) => return Err(format!("Provider stdin: {e}")),
                    }
                }
            }
            if fds[1].revents != 0 {
                let available = buffer.len().min(FRAME_LIMIT + 1 - self.output.len());
                match stdout.read(&mut buffer[..available]) {
                    Ok(0) => stdout_eof = true,
                    Ok(n) => {
                        reserve_frame(&mut self.output, n)?;
                        self.output.extend_from_slice(&buffer[..n]);
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                    Err(e) => return Err(format!("Provider stdout: {e}")),
                }
            }
            if fds[2].revents != 0 {
                match stderr.read(&mut buffer) {
                    Ok(0) => stderr_eof = true,
                    Ok(n) => {
                        for byte in &buffer[..n] {
                            if self.stderr.len() == 16384 {
                                self.stderr.pop_front();
                            }
                            self.stderr.push_back(*byte);
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                    Err(e) => return Err(format!("Provider stderr: {e}")),
                }
            }
        }
    }
    fn drain_commands(&mut self, closing: bool) -> Result<(), String> {
        for _ in 0..64 {
            // Backpressure applies to queued bytes, not to the mere existence of a
            // queued write: a provider that reads stdin only between turns keeps
            // `pending` non-empty for the whole turn, and refusing to drain there left
            // UI commands unacknowledged until the caller's 10 second timeout.
            if self.queued > QUEUE_BACKPRESSURE {
                break;
            }
            let Ok((command, reply)) = self.incoming.try_recv() else {
                break;
            };
            if closing || self.stop.load(Ordering::Acquire) {
                let _ = reply.send(Err("Chat is closing".into()));
                continue;
            }
            let (run, command) = match command {
                ActorCommand::Provider { run, command } => (run, command),
                ActorCommand::UpdateSource(source, execution_console, paired_holder) => {
                    let result = self
                        .native_holder
                        .as_mut()
                        .ok_or("Native holder is unavailable".to_string())
                        .and_then(|holder| {
                            holder.update_source(source, execution_console, paired_holder)
                        });
                    let failed = result.is_err();
                    let _ = reply.send(result);
                    // A partial control reply must never be reused.
                    if failed {
                        return Err("Native ownership update was not acknowledged".into());
                    }
                    continue;
                }
                ActorCommand::InterruptRun(run) => {
                    if self.execution.active() != Some(run) {
                        let _ = reply.send(Err(
                            "The requested run is no longer the provider's active turn".into(),
                        ));
                        continue;
                    }
                    (None, Command::Interrupt)
                }
                ActorCommand::CloseIfIdle { generation } => {
                    let state = self.state.lock().unwrap_or_else(|p| p.into_inner());
                    let idle = state.generation == generation
                        && self.update_readiness(&state) == UpdateReadiness::Idle;
                    if idle {
                        self.stop.store(true, Ordering::Release);
                    }
                    let _ = reply.send(if idle {
                        Ok(())
                    } else {
                        Err("Provider state changed before conditional idle close".into())
                    });
                    continue;
                }
                ActorCommand::CheckUpdateReadiness(readiness) => {
                    let state = self.state.lock().unwrap_or_else(|p| p.into_inner());
                    let _ = readiness.send(self.update_readiness(&state));
                    let _ = reply.send(Ok(()));
                    continue;
                }
            };
            if matches!(command, Command::LoadEarlier | Command::Refresh) {
                match page_command(&self.store, &command) {
                    Ok(cached) => {
                        self.touch();
                        if cached {
                            self.apply(Event::HistoryRecovered);
                            let _ = reply.send(Ok(()));
                            continue;
                        }
                    }
                    Err(error) => {
                        let _ = reply.send(Err(error));
                        continue;
                    }
                }
            }
            let starts_turn = matches!(&command, Command::Prompt { .. });
            if run.is_some() && starts_turn {
                let state = self.state.lock().unwrap_or_else(|p| p.into_inner());
                if !state.ready || state.busy || state.waiting || !state.requests.is_empty() {
                    let _ = reply.send(Err(
                        "The provider is no longer idle for the accepted run".into()
                    ));
                    continue;
                }
            }
            let authorizes_work = match &command {
                Command::Prompt { .. }
                | Command::Compact
                | Command::Slash { .. }
                | Command::StartQueued { .. }
                | Command::Branch { .. }
                | Command::SetPermissionMode { .. } => true,
                Command::UpdatePermissionSettings { .. } => true,
                Command::Answer { action, .. } => !matches!(
                    action.as_str(),
                    "deny" | "decline" | "reject" | "cancel" | "abort"
                ),
                _ => false,
            };
            if authorizes_work
                && let Some(context) = &self.launch_context
                && let Err(error) = context.verify_directory(&self.working_directory)
            {
                let _ = reply.send(Err(error));
                continue;
            }
            let prepared = match run {
                Some(run) => self.adapter.command_for_run(run, command),
                None => self.adapter.command(command),
            };
            match prepared {
                Ok(step) => {
                    let result = self.step(step);
                    // Busy before the caller hears back, so whoever checks
                    // for an idle conversation next sees this turn.
                    let started = starts_turn && result.is_ok();
                    if started {
                        self.apply(Event::Status {
                            busy: true,
                            detail: "Waiting for provider turn".into(),
                        });
                    }
                    let _ = reply.send(result.clone());
                    result?;
                    if started {
                        self.publish_activity();
                    }
                }
                Err(message) => {
                    self.apply(Event::Error {
                        message: message.clone(),
                        fatal: false,
                    });
                    let _ = reply.send(Err(message));
                }
            }
        }
        Ok(())
    }
    fn update_readiness(&self, state: &Snapshot) -> UpdateReadiness {
        if !state.ready || state.exited || !self.pending.is_empty() || !self.output.is_empty() {
            UpdateReadiness::Unready
        } else if state.is_working() || state.waiting || !state.requests.is_empty() {
            UpdateReadiness::Busy
        } else {
            UpdateReadiness::Idle
        }
    }

    fn observe_update(&self) {
        let state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        let generation = state.generation;
        let observed = self.observe_next.swap(false, Ordering::AcqRel);
        // Host acknowledgement takes host -> state. Never notify under state.
        drop(state);
        if observed {
            (self.notify)(Update::Observed { generation });
        }
    }
    fn provider_step(&mut self, step: Step) -> Result<(), String> {
        // An account report is not output of this conversation, so it does
        // not mark an offscreen pane as having something new to read.
        let observed = step
            .events
            .iter()
            .any(|event| !matches!(event, Event::AccountUsage(_)));
        let identity = step.events.iter().rev().find_map(|event| match event {
            Event::Session { id, .. } => Some(Some(id.clone())),
            Event::Reset { .. } => Some(None),
            _ => None,
        });
        if let Some(Some(thread)) = &identity
            && self.native_holder.is_some()
            && let Some(identity) = self.adapter.server_identity()
            && super::native_holder::verified_codex_version(&identity.user_agent).is_none()
            && !std::mem::replace(&mut self.reconnect_only_reported, true)
        {
            // The holder of an unconfirmed server ends it when the daemon
            // connection is lost, so say which conversation that leaves
            // without the continuity the holder otherwise provides.
            tracing::warn!(
                %thread,
                version = super::native_holder::codex_version(&identity.user_agent)
                    .unwrap_or("unrecognized"),
                user_agent = %identity.user_agent,
                verified = ?super::native_holder::VERIFIED_CODEX_VERSIONS,
                "Codex version is not verified for durable continuity; this conversation's \
                 server ends if the daemon exits or restarts, and the conversation then \
                 resumes from disk without its running turn"
            );
        }
        if let Some(Some(thread)) = &identity
            && let Some(holder) = &mut self.native_holder
            && let Some(identity) = self.adapter.server_identity()
            && let Some(version) =
                super::native_holder::verified_codex_version(&identity.user_agent)
        {
            let process = self
                .native_peer
                .clone()
                .ok_or("Missing verified app-server peer")?;
            holder.confirm(thread.clone(), identity.clone(), process)?;
            // Durable continuity is a claim about this thread. Name it first,
            // so nobody can observe the claim without its conversation.
            *self
                .confirmed_session
                .lock()
                .unwrap_or_else(|p| p.into_inner()) = Some(thread.clone());
            self.state
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .native_continuity = super::NativeContinuity::Durable {
                provider_version: version.to_owned(),
            };
            self.dirty = true;
        }
        self.apply_step(step, identity)?;
        if observed {
            self.observe_update();
        }
        Ok(())
    }
    fn step(&mut self, step: Step) -> Result<(), String> {
        self.apply_step(step, None)
    }
    /// Applies one frame. `confirmed` is the conversation identity the
    /// provider left the frame with, when the frame changed it.
    fn apply_step(&mut self, step: Step, confirmed: Option<Option<String>>) -> Result<(), String> {
        // A new Codex thread is created asynchronously after initialize; check
        // its RPC boundary too. Interrupts, denials and cleanup remain usable
        // even if the approved directory disappears or is replaced.
        if step
            .writes
            .iter()
            .any(|frame| frame["method"] == "thread/start")
            && let Some(context) = &self.launch_context
        {
            context.verify_directory(&self.working_directory)?;
        }
        for value in step.writes {
            let mut frame = BoundedFrame(Vec::new());
            serde_json::to_writer(&mut frame, &value)
                .map_err(|error| format!("Provider JSON request: {error}"))?;
            let mut bytes = frame.0;
            if self.child.is_some() {
                reserve_frame(&mut bytes, 1)?;
                bytes.push(b'\n');
            }
            if self.queued + bytes.len() > QUEUE_LIMIT {
                return Err("Provider stdin queue exceeds 64 MiB; no frames discarded".into());
            }
            self.queued += bytes.len();
            self.pending.push_back(bytes);
            if self.write_started.is_none() {
                self.write_started = Some(Instant::now());
            }
        }
        let execution_scope = step.events.iter().find_map(|event| match event {
            Event::Execution {
                run,
                event: crate::agent_execution::ProviderEvent::Scope,
            } => Some(*run),
            _ => None,
        });
        let mut execution_events = Vec::new();
        for event in step.events {
            if let Event::Execution { run, event } = event {
                if let crate::agent_execution::ProviderEvent::Started { key } = event {
                    let update = self.execution.start(run, key)?;
                    (self.notify)(Update::Execution { run, update });
                } else {
                    execution_events.push((run, event));
                }
                continue;
            }
            if let Some(run) = execution_scope {
                self.execution.observe(run, &event);
            }
            if let Event::AccountUsage(report) = event {
                (self.notify)(Update::AccountUsage {
                    report,
                    observed_at: observed_at().unwrap_or_default(),
                });
                continue;
            }
            let fatal = match &event {
                Event::Error {
                    message,
                    fatal: true,
                } => Some(message.clone()),
                _ => None,
            };
            let events = self
                .store
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .apply(event)?;
            if events.is_empty() {
                self.touch();
            } else {
                for event in events {
                    self.apply(event);
                }
            }
            if self.retained > STATE_LIMIT {
                return Err(
                    "Provider request/model metadata exceeds native IPC presentation capacity"
                        .into(),
                );
            }
            if let Some(message) = fatal {
                return Err(message);
            }
        }
        // An outcome is acted on as soon as it is published, and its reader
        // asks which conversation produced it. Answer before announcing.
        if let Some(confirmed) = confirmed {
            *self
                .confirmed_session
                .lock()
                .unwrap_or_else(|p| p.into_inner()) = confirmed;
        }
        // The provider's final usage/items may follow its settlement in this
        // same frame. Freeze only after applying the whole frame, then publish
        // its final outcome (which may arrive in a much later frame).
        for (run, event) in execution_events {
            match event {
                crate::agent_execution::ProviderEvent::Settled => {
                    let usage = self
                        .state
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .usage
                        .clone();
                    self.execution.settle(run, usage)?;
                }
                crate::agent_execution::ProviderEvent::Finished { outcome } => {
                    let update = self.execution.finish(run, outcome)?;
                    (self.notify)(Update::Execution { run, update });
                }
                crate::agent_execution::ProviderEvent::Started { .. } => unreachable!(),
                crate::agent_execution::ProviderEvent::Scope => {}
            }
        }
        self.publish_activity();
        Ok(())
    }
    fn apply(&mut self, event: Event) {
        if matches!(
            event,
            Event::TurnStarted | Event::TurnSettled | Event::TurnFinished { .. }
        ) {
            if let Some(evidence) = self.attention.apply(&event) {
                (self.notify)(Update::Attention(evidence));
            }
            if !matches!(event, Event::TurnStarted) {
                return;
            }
        }
        if matches!(&event, Event::Reset { .. }) {
            *self
                .confirmed_session
                .lock()
                .unwrap_or_else(|p| p.into_inner()) = None;
        }
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        recover_history_error(&mut state.error, &mut self.history_error, &event);
        apply_event(&mut state, &mut self.indices, event);
        // Only metadata is resident independently of the disk-backed window.
        let items = std::mem::take(&mut state.items);
        let child_items: Vec<_> = state
            .subagents
            .iter_mut()
            .map(|agent| std::mem::take(&mut agent.items))
            .collect();
        let mut counter = ByteCounter(0);
        let _ = serde_json::to_writer(&mut counter, &*state);
        state.items = items;
        for (agent, items) in state.subagents.iter_mut().zip(child_items) {
            agent.items = items;
        }
        self.retained = counter.0;
        state.generation = state.generation.saturating_add(1);
        self.dirty = true;
    }
    fn publish_activity(&mut self) {
        let state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        let activity = state.activity();
        let generation = state.generation;
        let became_ready = state.ready && !self.ready;
        self.ready = state.ready;
        drop(state);
        let baselined = std::mem::replace(&mut self.attention_baselined, true);
        if activity != self.activity {
            self.activity = activity;
            if let Some((run, update)) = self.execution.activity(activity) {
                (self.notify)(Update::Execution { run, update });
            }
            if activity == ActivityState::Waiting && baselined {
                (self.notify)(Update::Attention(crate::notifications::Evidence {
                    key: format!("{}:waiting:{generation}", self.attention.key),
                    kind: crate::notifications::Kind::Waiting,
                }));
            }
            (self.notify)(Update::Activity {
                state: activity,
                generation,
            });
        }
        if became_ready {
            (self.notify)(Update::Ready);
        }
    }
    fn touch(&mut self) {
        self.state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .generation += 1;
        self.dirty = true;
    }
}
fn page_command(store: &Mutex<Store>, command: &Command) -> Result<bool, String> {
    let mut store = store.lock().unwrap_or_else(|p| p.into_inner());
    match command {
        Command::LoadEarlier => store.earlier(),
        Command::Refresh => {
            store.latest();
            Ok(false)
        }
        _ => Ok(false),
    }
}
fn reserve_frame(bytes: &mut Vec<u8>, additional: usize) -> Result<(), String> {
    let required = bytes
        .len()
        .checked_add(additional)
        .ok_or("Provider frame length overflow")?;
    if required > FRAME_LIMIT + 1 {
        return Err("Provider frame exceeds 32 MiB".into());
    }
    if required > bytes.capacity() {
        let capacity = required.next_power_of_two().min(FRAME_LIMIT + 1);
        bytes
            .try_reserve_exact(capacity - bytes.len())
            .map_err(|error| error.to_string())?;
    }
    Ok(())
}
struct BoundedFrame(Vec<u8>);
impl Write for BoundedFrame {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > FRAME_LIMIT.saturating_sub(self.0.len()) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "Provider request exceeds 32 MiB",
            ));
        }
        reserve_frame(&mut self.0, bytes.len()).map_err(std::io::Error::other)?;
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
struct ByteCounter(usize);
impl Write for ByteCounter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0 = self.0.saturating_add(bytes.len());
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
/// Unix milliseconds for the moment the daemon observed an item change, read
/// from the same clock the surface measures an elapsed time against.
pub(super) fn observed_at() -> Option<u64> {
    crate::chat::now_ms()
}
fn recover_history_error(
    error: &mut Option<String>,
    history_error: &mut Option<String>,
    event: &Event,
) {
    match event {
        // A provider accepting a new turn recovers the preceding operation's
        // error. A failed history page has its own recovery contract.
        Event::TurnStarted if history_error.is_none() => *error = None,
        Event::HistoryError(message) => *history_error = Some(message.clone()),
        Event::HistoryRecovered => {
            if history_error
                .take()
                .as_ref()
                .is_some_and(|previous| error.as_ref() == Some(previous))
            {
                *error = None;
            }
        }
        Event::Error { .. } | Event::Reset { .. } | Event::Exited { .. } => *history_error = None,
        _ => {}
    }
}

fn apply_event(state: &mut Snapshot, indices: &mut BTreeMap<String, usize>, event: Event) {
    let latest_history = matches!(&event, Event::LatestHistory { .. });
    match event {
        Event::OutputDelta { id, offset, delta } => {
            if let Some(&index) = indices.get(&id) {
                let item = Arc::make_mut(&mut state.items[index]);
                let body = match &mut item.content {
                    ItemContent::Command { output, .. } | ItemContent::Tool { output, .. } => {
                        Some(output)
                    }
                    _ => None,
                };
                if let Some(body) = body.filter(|body| body.len() == offset) {
                    body.push_str(&delta);
                    let at = observed_at();
                    item.started_at = item.started_at.or(item.at).or(at);
                    item.at = at;
                }
            }
        }
        Event::TurnStarted
        | Event::TurnSettled
        | Event::TurnFinished { .. }
        | Event::Execution { .. } => {}
        Event::ItemsRemoved { ids } => {
            let removed: std::collections::BTreeSet<_> = ids.into_iter().collect();
            state.items.retain(|item| !removed.contains(&item.id));
            indices.clear();
            indices.extend(
                state
                    .items
                    .iter()
                    .enumerate()
                    .map(|(index, item)| (item.id.clone(), index)),
            );
        }
        Event::QueueChanged(messages) => state.queued_messages = messages,
        Event::QueueRemoval(removal) => {
            state.queue_removals.push(removal);
            let excess = state
                .queue_removals
                .len()
                .saturating_sub(crate::chat::QueueRemoval::LIMIT);
            state.queue_removals.drain(..excess);
        }
        Event::HistoryLoading(loading) => state.history_loading = loading,
        Event::LatestHistoryStarted => state.history_loading = true,
        Event::HistoryError(message) => state.error = Some(message),
        Event::HistoryRecovered => {}
        Event::InterruptMode(mode) => state.interrupt_mode = Some(mode),
        Event::BranchPoints(points) => state.branch_points = points,
        Event::Forked(id) => state.forked = Some(id),
        Event::Reset { reason } => {
            state.branch_points.clear();
            state.forked = None;
            state.interrupt_mode = None;
            state.queued_messages.clear();
            state.queue_removals.clear();
            state.items.clear();
            state.subagents.clear();
            indices.clear();
            state.requests.clear();
            state.usage = Default::default();
            state.session_id = None;
            state.title = None;
            state.has_earlier = false;
            state.has_later = false;
            state.history_loading = false;
            state.detail = reason;
            state.busy = false;
            state.waiting = false;
            state.error = None;
            state.plan.clear();
            state.permission_mode = None;
            state.permissions = None;
            state.permission_denials.clear();
        }
        Event::Session { id, title } => {
            state.session_id = Some(id);
            state.title = title;
        }
        Event::Plan { steps } => state.plan = steps,
        // About the account rather than the conversation: `step` forwards it
        // to the surfaces before the store, so it never reaches a snapshot.
        Event::AccountUsage(_) => {}
        Event::Capabilities(value) => state.capabilities = value,
        Event::PermissionMode(mode) => state.permission_mode = mode,
        Event::PermissionSettings(settings) => state.permissions = Some(settings),
        Event::PermissionDenied(mut denial) => {
            if let Some(previous) = state
                .permission_denials
                .iter_mut()
                .find(|old| old.tool_use_id == denial.tool_use_id)
            {
                if denial.tool_input.is_none() {
                    denial.tool_input = previous.tool_input.take();
                }
                if denial.reason.is_empty() {
                    denial.reason = std::mem::take(&mut previous.reason);
                }
                if denial.reason_type.is_none() {
                    denial.reason_type = previous.reason_type.take();
                }
                if denial.agent_id.is_none() {
                    denial.agent_id = previous.agent_id.take();
                }
                *previous = denial;
            } else {
                if state.permission_denials.len() >= super::PermissionDenial::LIMIT {
                    state.permission_denials.remove(0);
                }
                state.permission_denials.push(denial);
            }
        }
        Event::PermissionDenialDismissed { tool_use_id } => state
            .permission_denials
            .retain(|denial| denial.tool_use_id != tool_use_id),
        Event::Models {
            models,
            selected,
            reasoning,
        } => {
            if !models.is_empty() {
                state.models = models;
            }
            if selected.is_some() {
                state.selected_model = selected;
                state.reasoning = reasoning;
            } else if reasoning.is_some() {
                state.reasoning = reasoning;
            }
        }
        Event::Commands(value) => state.commands = value,
        Event::Skills(value) => state.skills = value,
        Event::Item(mut item) => {
            // The adapters do not know wall-clock time, so the item is stamped
            // here, where the daemon actually observes the change.
            item.at = observed_at();
            item.started_at = item.at;
            if let Some(&index) = indices.get(&item.id) {
                item.started_at = state.items[index]
                    .started_at
                    .or(state.items[index].at)
                    .or(item.at);
                if let (
                    ItemContent::Tool { name, input, .. },
                    ItemContent::Tool {
                        name: old_name,
                        input: old_input,
                        ..
                    },
                ) = (&mut item.content, &state.items[index].content)
                {
                    if name.is_empty() {
                        name.clone_from(old_name);
                    }
                    if input.is_empty() {
                        input.clone_from(old_input);
                    }
                }
                state.items[index] = Arc::new(item);
            } else {
                indices.insert(item.id.clone(), state.items.len());
                state.items.push(Arc::new(item));
            }
        }
        Event::Subagent {
            id,
            kind,
            description,
            status,
            background,
        } => {
            let agent = subagent(state, &id);
            if let Some(background) = background {
                agent.background = background;
            }
            if !kind.is_empty() {
                agent.kind = kind;
            }
            if !description.is_empty() {
                agent.description = description;
            }
            if let Some(status) = status {
                agent.status = status;
            }
        }
        Event::SubagentItem { id, mut item } => {
            item.at = observed_at();
            item.started_at = item.at;
            let agent = subagent(state, &id);
            if let Some(old) = agent.items.iter_mut().find(|old| old.id == item.id) {
                item.started_at = old.started_at.or(old.at).or(item.at);
                *old = Arc::new(item);
            } else {
                agent.items.push(Arc::new(item));
            }
            trim_subagent(agent);
        }
        Event::SubagentTextDelta {
            id,
            item_id,
            role,
            thinking,
            delta,
        } => {
            let agent = subagent(state, &id);
            if let Some(item) = agent.items.iter_mut().find(|item| item.id == item_id) {
                let item = Arc::make_mut(item);
                if let ItemContent::Text { text, .. } = &mut item.content {
                    text.push_str(&delta);
                    let at = observed_at();
                    item.started_at = item.started_at.or(item.at).or(at);
                    item.at = at;
                }
            } else {
                let at = observed_at();
                agent.items.push(Arc::new(Item {
                    id: item_id,
                    role,
                    content: ItemContent::Text {
                        text: delta,
                        thinking,
                        phase: Phase::Final,
                    },
                    started_at: at,
                    at,
                }));
            }
            trim_subagent(agent);
        }
        Event::SubagentOutputDelta {
            id,
            item_id,
            offset,
            delta,
        } => {
            let agent = subagent(state, &id);
            if let Some(item) = agent.items.iter_mut().find(|item| item.id == item_id) {
                let item = Arc::make_mut(item);
                if let ItemContent::Command { output, .. } | ItemContent::Tool { output, .. } =
                    &mut item.content
                    && output.len() == offset
                {
                    output.push_str(&delta);
                    let at = observed_at();
                    item.started_at = item.started_at.or(item.at).or(at);
                    item.at = at;
                }
            }
            trim_subagent(agent);
        }
        Event::SubagentItemsRemoved { id, ids } => {
            if let Some(agent) = state.subagents.iter_mut().find(|agent| agent.id == id) {
                agent.items.retain(|item| !ids.contains(&item.id));
            }
        }
        Event::History { items, has_earlier } | Event::LatestHistory { items, has_earlier } => {
            state.history_loading = false;
            let mut run = Vec::new();
            let mut after = None;
            let mut seen = std::collections::BTreeSet::new();
            for item in items {
                if !seen.insert(item.id.clone()) {
                    continue;
                }
                if let Some(index) = state.items.iter().position(|cached| cached.id == item.id) {
                    state
                        .items
                        .splice(index..index, run.drain(..).map(Arc::new));
                    after = Some(item.id);
                } else {
                    run.push(item);
                }
            }
            let insertion = after
                .and_then(|id| {
                    state
                        .items
                        .iter()
                        .position(|item| item.id == id)
                        .map(|index| index + 1)
                })
                .unwrap_or(if latest_history { state.items.len() } else { 0 });
            state
                .items
                .splice(insertion..insertion, run.into_iter().map(Arc::new));
            indices.clear();
            indices.extend(
                state
                    .items
                    .iter()
                    .enumerate()
                    .map(|(index, item)| (item.id.clone(), index)),
            );
            state.has_earlier = has_earlier;
        }
        Event::TextDelta {
            id,
            role,
            thinking,
            delta,
        } => {
            let at = observed_at();
            if let Some(&index) = indices.get(&id) {
                let item = Arc::make_mut(&mut state.items[index]);
                if let ItemContent::Text { text, .. } = &mut item.content {
                    text.push_str(&delta);
                    item.started_at = item.started_at.or(item.at).or(at);
                    item.at = at;
                } else {
                    state.error = Some(format!(
                        "Provider reused non-text item ID {id} for a text delta"
                    ));
                }
            } else {
                indices.insert(id.clone(), state.items.len());
                state.items.push(Arc::new(Item {
                    id,
                    role,
                    // The delta stream carries no phase, so the item created to
                    // receive the text declares the default instead of guessing
                    // that an unfinished message is narration.
                    content: ItemContent::Text {
                        text: delta,
                        thinking,
                        phase: Phase::Final,
                    },
                    started_at: at,
                    at,
                }));
            }
        }
        Event::Request(request) => {
            if let Some(old) = state.requests.iter_mut().find(|r| r.id == request.id) {
                *old = request;
            } else {
                state.requests.push(request);
            }
        }
        Event::RequestResolved { id } => state.requests.retain(|request| request.id != id),
        Event::Waiting(waiting) => state.waiting = waiting,
        Event::Status { busy, detail } => {
            state.busy = busy;
            state.detail = detail;
        }
        Event::Usage(value) => {
            if value.input_tokens != 0 {
                state.usage.input_tokens = value.input_tokens;
            }
            if value.output_tokens != 0 {
                state.usage.output_tokens = value.output_tokens;
            }
            if value.context_tokens != 0 {
                state.usage.context_tokens = value.context_tokens;
            }
            if value.context_limit != 0 {
                state.usage.context_limit = value.context_limit;
            }
            if value.cost_microusd.is_some() {
                state.usage.cost_microusd = value.cost_microusd;
            }
        }
        Event::Composer { text } => state.composer = Some(text),
        Event::OpenUrl { url, instructions } => state.open_url = Some((url, instructions)),
        Event::Error { message, fatal } => {
            state.error = Some(message);
            if fatal {
                state.busy = false;
                state.history_loading = false;
            }
        }
        Event::Exited { .. } => {
            state.exited = true;
            state.waiting = false;
            state.busy = false;
            state.ready = false;
            state.history_loading = false;
        }
        Event::Ready(ready) => state.ready = ready,
    }
}

fn subagent<'a>(state: &'a mut Snapshot, id: &str) -> &'a mut Subagent {
    if let Some(index) = state.subagents.iter().position(|agent| agent.id == id) {
        return &mut state.subagents[index];
    }
    if state.subagents.len() >= 32 {
        let oldest_finished = state
            .subagents
            .iter()
            .position(|agent| agent.status != ToolStatus::Running)
            .unwrap_or(0);
        state.subagents.remove(oldest_finished);
    }
    state.subagents.push(Subagent {
        id: id.into(),
        kind: String::new(),
        description: String::new(),
        status: ToolStatus::Running,
        background: false,
        items: Vec::new(),
    });
    state.subagents.last_mut().expect("subagent was inserted")
}

fn trim_subagent(agent: &mut Subagent) {
    const MAX_BYTES: usize = 64 * 1024;
    const MAX_ITEMS: usize = 64;
    if let Some(last) = agent.items.last_mut()
        && last.content.byte_len() > MAX_BYTES
    {
        Arc::make_mut(last).content = ItemContent::Text {
            text: "[This large item is available in the provider transcript.]".into(),
            thinking: false,
            phase: Phase::Commentary,
        };
    }
    let mut bytes = agent
        .items
        .iter()
        .map(|item| item.content.byte_len())
        .sum::<usize>();
    while agent.items.len() > MAX_ITEMS || bytes > MAX_BYTES {
        let removed = agent.items.remove(0);
        bytes = bytes.saturating_sub(removed.content.byte_len());
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn incomplete_stdio_frames_only_scan_the_new_suffix() {
        let mut bytes = Vec::new();
        let mut scanned = 0;
        for _ in 0..1024 {
            let before = bytes.len();
            bytes.extend_from_slice(&[b'x'; 8192]);
            assert_eq!(scanned, before);
            assert_eq!(super::next_newline(&bytes, &mut scanned), None);
            assert_eq!(scanned, bytes.len());
        }
        bytes.extend_from_slice(b"\n{}\n");
        assert_eq!(
            super::next_newline(&bytes, &mut scanned),
            Some(8 * 1024 * 1024)
        );
        bytes.drain(..=scanned);
        scanned = 0;
        assert_eq!(super::next_newline(&bytes, &mut scanned), Some(2));
    }

    #[test]
    fn update_readiness_fences_a_prompt_whose_caller_stopped_waiting() {
        struct GatedPrompt {
            entered: crossbeam_channel::Sender<()>,
            release: crossbeam_channel::Receiver<()>,
        }
        impl Adapter for GatedPrompt {
            fn initialize(&mut self) -> Result<Step, String> {
                Ok(Step::default())
            }
            fn command(&mut self, command: Command) -> Result<Step, String> {
                match command {
                    Command::Compact => Ok(Step {
                        events: vec![Event::Ready(true)],
                        ..Step::default()
                    }),
                    Command::Prompt { .. } => {
                        self.entered.send(()).map_err(|error| error.to_string())?;
                        self.release
                            .recv_timeout(Duration::from_secs(5))
                            .map_err(|error| error.to_string())?;
                        Ok(Step::default())
                    }
                    Command::Interrupt => Ok(Step {
                        events: vec![Event::Status {
                            busy: false,
                            detail: String::new(),
                        }],
                        ..Step::default()
                    }),
                    _ => Ok(Step::default()),
                }
            }
            fn receive(&mut self, _: Value) -> Result<Step, String> {
                Ok(Step::default())
            }
        }
        let (entered_tx, entered_rx) = crossbeam_channel::bounded(1);
        let (release_tx, release_rx) = crossbeam_channel::bounded(1);
        let spec = ProcessSpec {
            execution: None,
            messages: None,
            unset_environment: Default::default(),
            transport: TransportSpec::JsonLines,
            program: "/bin/cat".into(),
            arguments: Vec::new(),
            working_directory: std::env::temp_dir(),
            environment: BTreeMap::new(),
        };
        let mut initial = Step::default();
        let store = preflight(&spec, &mut initial).unwrap();
        let handle = Handle::start(
            spec,
            Box::new(GatedPrompt {
                entered: entered_tx,
                release: release_rx,
            }),
            initial,
            store,
            None,
            |_| {},
        )
        .unwrap();
        assert_eq!(
            handle.check_update_readiness().unwrap(),
            UpdateReadiness::Unready,
            "initialization is not idle"
        );
        handle.command(Command::Compact).unwrap();
        assert_eq!(
            handle.check_update_readiness().unwrap(),
            UpdateReadiness::Idle
        );
        assert!(
            !handle.0.stop.load(Ordering::Acquire),
            "readiness must not close the actor"
        );

        // The request was accepted into the actor queue, but its caller has
        // stopped waiting for acknowledgment. Its snapshot is still idle.
        let (reply, timed_out) = crossbeam_channel::bounded(1);
        drop(timed_out);
        handle
            .0
            .commands
            .send((
                ActorCommand::Provider {
                    run: None,
                    command: Command::Prompt {
                        text: "accepted work".into(),
                        attachments: Vec::new(),
                        mode: super::super::PromptMode::Send,
                    },
                },
                reply,
            ))
            .unwrap();
        handle.wake();
        entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(!handle.snapshot(None).unwrap().busy);
        let (reply, acknowledgment) = crossbeam_channel::bounded(1);
        let (result, readiness) = crossbeam_channel::bounded(1);
        handle
            .0
            .commands
            .send((ActorCommand::CheckUpdateReadiness(result), reply))
            .unwrap();
        handle.wake();
        assert!(matches!(
            readiness.try_recv(),
            Err(crossbeam_channel::TryRecvError::Empty)
        ));
        release_tx.send(()).unwrap();
        assert_eq!(
            readiness.recv_timeout(Duration::from_secs(5)).unwrap(),
            UpdateReadiness::Busy
        );
        acknowledgment
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .unwrap();
        assert!(handle.snapshot(None).unwrap().busy);
        handle.close().unwrap();
    }

    #[test]
    fn startup_cleanup_joins_the_exact_actor_before_releasing_ownership() {
        use super::super::protocol::{ExecutionContext, LaunchOutcome};
        let directory = tempfile::tempdir().unwrap();
        let context = Arc::new(ExecutionContext::default());
        let spec = ProcessSpec {
            execution: Some(context.clone()),
            messages: None,
            unset_environment: Default::default(),
            transport: TransportSpec::JsonLines,
            program: "/bin/cat".into(),
            arguments: Vec::new(),
            working_directory: directory.path().into(),
            environment: BTreeMap::new(),
        };
        let mut initial = Step::default();
        let store = preflight(&spec, &mut initial).unwrap();
        let handle = Handle::start(spec, Box::new(Echo), initial, store, None, |_| {}).unwrap();
        assert_eq!(context.outcome(), LaunchOutcome::Uncertain);
        assert_eq!(
            context.reconcile_cleanup().unwrap(),
            LaunchOutcome::TeardownProved
        );
        assert!(handle.is_completed());
        assert_eq!(
            context.reconcile_cleanup().unwrap(),
            LaunchOutcome::TeardownProved
        );
        assert!(
            handle.command(Command::Refresh).is_ok(),
            "retired transcript remains readable"
        );
    }

    #[test]
    fn late_outcome_does_not_settle_a_new_turn_with_continuous_activity() {
        use super::*;
        let mut attention = TurnAttention::default();
        let old = attention.key.clone();
        attention.apply(&Event::TurnSettled);
        assert!(
            attention.settled,
            "idle transport loss must not duplicate an outcome"
        );
        // Aggregate activity may remain Working because background tasks run
        // across the result and the next accepted user prompt.
        attention.apply(&Event::TurnStarted);
        let current = attention.key.clone();
        assert_ne!(current, old);
        assert!(!attention.settled);
        let delayed = attention
            .apply(&Event::TurnFinished {
                key: Some(old.clone()),
                outcome: crate::notifications::Kind::Cancelled,
            })
            .unwrap();
        assert_eq!(delayed.key, old);
        assert_eq!(attention.key, current);
        assert!(
            !attention.settled,
            "current transport failure still needs an outcome"
        );
    }

    use super::super::Role;
    use super::*;
    use serde_json::{Value, json};

    fn text(id: &str, value: &str) -> Item {
        Item {
            id: id.into(),
            role: Role::Assistant,
            content: ItemContent::Text {
                text: value.into(),
                thinking: false,
                phase: Phase::Final,
            },
            started_at: None,
            at: None,
        }
    }
    /// Stamps are wall-clock readings, so structural comparisons drop them and
    /// leave the timing contract to the test that is actually about timing.
    fn unstamped(items: &[Arc<Item>]) -> Vec<Item> {
        items
            .iter()
            .map(|item| {
                let mut item = (**item).clone();
                item.at = None;
                item.started_at = None;
                item
            })
            .collect()
    }
    #[test]
    fn child_transcript_is_separate_and_bounded() {
        let mut state = Snapshot::default();
        let mut indices = BTreeMap::new();
        apply_event(
            &mut state,
            &mut indices,
            Event::Subagent {
                id: "agent-tool".into(),
                kind: "Explore".into(),
                description: "Inspect".into(),
                status: None,
                background: Some(true),
            },
        );
        apply_event(
            &mut state,
            &mut indices,
            Event::SubagentItem {
                id: "agent-tool".into(),
                item: text("child-one", "Found a path"),
            },
        );
        apply_event(
            &mut state,
            &mut indices,
            Event::SubagentTextDelta {
                id: "agent-tool".into(),
                item_id: "child-one".into(),
                role: Role::Assistant,
                thinking: false,
                delta: " and a test".into(),
            },
        );
        assert!(state.items.is_empty());
        assert_eq!(
            state.subagents[0].items[0].content,
            text("child-one", "Found a path and a test").content
        );
        for index in 0..80 {
            apply_event(
                &mut state,
                &mut indices,
                Event::SubagentItem {
                    id: "agent-tool".into(),
                    item: text(&format!("child-{index}"), "x"),
                },
            );
        }
        assert_eq!(state.subagents[0].items.len(), 64);
        apply_event(
            &mut state,
            &mut indices,
            Event::Subagent {
                id: "agent-tool".into(),
                kind: String::new(),
                description: String::new(),
                status: Some(ToolStatus::Succeeded),
                background: None,
            },
        );
        assert_eq!(state.subagents[0].status, ToolStatus::Succeeded);
        // An update that does not say leaves what is known in place.
        assert!(state.subagents[0].background);
    }
    /// Readiness is what a sender waits on, so it ends with the provider.
    #[test]
    fn a_snapshot_is_ready_from_its_providers_word_until_it_exits() {
        let mut state = Snapshot::default();
        let mut indices = BTreeMap::new();
        assert!(!state.ready);
        apply_event(&mut state, &mut indices, Event::Ready(true));
        assert!(state.ready);
        apply_event(&mut state, &mut indices, Event::Exited { code: Some(0) });
        assert!(!state.ready);
    }
    #[test]
    fn permission_denials_are_bounded_deduplicated_and_dismissible() {
        let mut state = Snapshot::default();
        let mut indices = BTreeMap::new();
        let denial = |id: &str| super::super::PermissionDenial {
            tool_use_id: id.into(),
            tool_name: "Bash".into(),
            tool_input: Some("{\"command\":\"git push\"}".into()),
            reason: "Classifier denied push".into(),
            reason_type: Some("classifier".into()),
            agent_id: Some("child".into()),
        };
        apply_event(
            &mut state,
            &mut indices,
            Event::PermissionDenied(denial("one")),
        );
        let mut duplicate = denial("one");
        duplicate.reason.clear();
        duplicate.reason_type = None;
        duplicate.agent_id = None;
        apply_event(&mut state, &mut indices, Event::PermissionDenied(duplicate));
        assert_eq!(state.permission_denials.len(), 1);
        assert_eq!(state.permission_denials[0].reason, "Classifier denied push");
        assert_eq!(
            state.permission_denials[0].agent_id.as_deref(),
            Some("child")
        );
        apply_event(&mut state, &mut indices, Event::TurnStarted);
        assert_eq!(state.permission_denials.len(), 1);
        let encoded = postcard::to_stdvec(&state).unwrap();
        assert_eq!(postcard::from_bytes::<Snapshot>(&encoded).unwrap(), state);
        apply_event(
            &mut state,
            &mut indices,
            Event::PermissionDenialDismissed {
                tool_use_id: "one".into(),
            },
        );
        assert!(state.permission_denials.is_empty());
        for id in 0..70 {
            apply_event(
                &mut state,
                &mut indices,
                Event::PermissionDenied(denial(&id.to_string())),
            );
        }
        assert_eq!(
            state.permission_denials.len(),
            super::super::PermissionDenial::LIMIT
        );
        apply_event(
            &mut state,
            &mut indices,
            Event::Reset {
                reason: "Reset".into(),
            },
        );
        assert!(state.permission_denials.is_empty());
    }
    #[test]
    fn asynchronous_history_loading_ends_on_response_reset_or_provider_exit() {
        for completion in [
            Event::HistoryLoading(false),
            Event::History {
                items: Vec::new(),
                has_earlier: false,
            },
            Event::Reset {
                reason: "new conversation".into(),
            },
            Event::Error {
                message: "provider failed".into(),
                fatal: true,
            },
            Event::Exited { code: None },
        ] {
            let mut state = Snapshot::default();
            let mut indices = BTreeMap::new();
            apply_event(&mut state, &mut indices, Event::HistoryLoading(true));
            assert!(state.history_loading);
            apply_event(&mut state, &mut indices, completion);
            assert!(!state.history_loading);
        }
    }

    #[test]
    fn outstanding_decisions_remain_waiting_after_streaming_stops() {
        let mut state = Snapshot::default();
        let mut indices = BTreeMap::new();
        apply_event(
            &mut state,
            &mut indices,
            Event::Status {
                busy: true,
                detail: String::new(),
            },
        );
        assert_eq!(state.activity(), ActivityState::Working);
        apply_event(
            &mut state,
            &mut indices,
            Event::Request(super::super::Request {
                id: "approval".into(),
                title: String::new(),
                description: String::new(),
                fields: Vec::new(),
                actions: Vec::new(),
            }),
        );
        assert_eq!(state.activity(), ActivityState::Waiting);
        apply_event(
            &mut state,
            &mut indices,
            Event::Status {
                busy: false,
                detail: String::new(),
            },
        );
        assert_eq!(state.activity(), ActivityState::Waiting);
        apply_event(
            &mut state,
            &mut indices,
            Event::RequestResolved {
                id: "approval".into(),
            },
        );
        assert_eq!(state.activity(), ActivityState::Idle);
        apply_event(&mut state, &mut indices, Event::Waiting(true));
        assert_eq!(state.activity(), ActivityState::Waiting);
        apply_event(&mut state, &mut indices, Event::Exited { code: None });
        assert_eq!(state.activity(), ActivityState::Unknown);
    }
    #[test]
    fn older_history_cannot_replace_a_live_stream_and_reset_invalidates_identity() {
        let mut state = Snapshot::default();
        let mut indices = BTreeMap::new();
        apply_event(
            &mut state,
            &mut indices,
            Event::Session {
                id: "exact-old".into(),
                title: None,
            },
        );
        apply_event(
            &mut state,
            &mut indices,
            Event::TextDelta {
                id: "live".into(),
                role: Role::Assistant,
                thinking: false,
                delta: "live".into(),
            },
        );
        apply_event(
            &mut state,
            &mut indices,
            Event::History {
                items: vec![
                    text("older", "history"),
                    text("live", "stale"),
                    text("older", "duplicate"),
                ],
                has_earlier: true,
            },
        );
        apply_event(
            &mut state,
            &mut indices,
            Event::TextDelta {
                id: "live".into(),
                role: Role::Assistant,
                thinking: false,
                delta: " update".into(),
            },
        );
        assert_eq!(
            unstamped(&state.items),
            vec![text("older", "history"), text("live", "live update")]
        );
        apply_event(
            &mut state,
            &mut indices,
            Event::Reset {
                reason: "upstream clear".into(),
            },
        );
        assert!(state.session_id.is_none());
        assert!(state.items.is_empty());
        apply_event(
            &mut state,
            &mut indices,
            Event::Item(text("live", "new conversation")),
        );
        assert_eq!(
            unstamped(&state.items),
            vec![text("live", "new conversation")]
        );
    }
    #[test]
    fn replayed_parent_and_child_items_keep_the_first_live_delta_start() {
        let mut state = Snapshot::default();
        let mut indices = BTreeMap::new();
        let tool = Item {
            id: "tool".into(),
            role: Role::Assistant,
            content: ItemContent::Tool {
                name: "exec".into(),
                input: "run".into(),
                output: String::new(),
                status: super::super::ToolStatus::Running,
            },
            started_at: None,
            at: None,
        };
        let replayed = vec![text("text", ""), tool];
        apply_event(
            &mut state,
            &mut indices,
            Event::History {
                items: replayed.clone(),
                has_earlier: false,
            },
        );
        subagent(&mut state, "child").items = replayed.into_iter().map(Arc::new).collect();
        let mut initial_starts = Vec::new();
        for offset in 0..2 {
            for event in [
                Event::TextDelta {
                    id: "text".into(),
                    role: Role::Assistant,
                    thinking: false,
                    delta: "x".into(),
                },
                Event::OutputDelta {
                    id: "tool".into(),
                    offset,
                    delta: "x".into(),
                },
                Event::SubagentTextDelta {
                    id: "child".into(),
                    item_id: "text".into(),
                    role: Role::Assistant,
                    thinking: false,
                    delta: "x".into(),
                },
                Event::SubagentOutputDelta {
                    id: "child".into(),
                    item_id: "tool".into(),
                    offset,
                    delta: "x".into(),
                },
            ] {
                apply_event(&mut state, &mut indices, event);
            }
            let starts: Vec<_> = state
                .items
                .iter()
                .chain(&state.subagents[0].items)
                .map(|item| {
                    item.started_at
                        .expect("the first live delta stamps replayed items")
                })
                .collect();
            if offset == 0 {
                initial_starts = starts;
                for item in state.items.iter_mut().chain(&mut state.subagents[0].items) {
                    let item = Arc::make_mut(item);
                    item.at = Some(item.started_at.unwrap() + 1_000);
                }
            } else {
                assert_eq!(
                    starts, initial_starts,
                    "later output must not move the start"
                );
            }
        }
    }

    #[test]
    fn observed_items_are_stamped_and_replayed_history_is_not() {
        let mut state = Snapshot::default();
        let mut indices = BTreeMap::new();
        apply_event(&mut state, &mut indices, Event::Item(text("live", "first")));
        let created = state.items[0].at.expect("an observed item carries a stamp");
        assert_eq!(state.items[0].started_at, Some(created));
        apply_event(
            &mut state,
            &mut indices,
            Event::TextDelta {
                id: "live".into(),
                role: Role::Assistant,
                thinking: false,
                delta: " more".into(),
            },
        );
        let streamed = state.items[0]
            .at
            .expect("accumulating a delta keeps the item stamped");
        assert!(streamed >= created);
        assert_eq!(state.items[0].started_at, Some(created));
        apply_event(
            &mut state,
            &mut indices,
            Event::History {
                items: vec![text("older", "replay")],
                has_earlier: false,
            },
        );
        assert_eq!(state.items[0].at, None);
        assert_eq!(state.items[1].at, Some(streamed));
    }

    #[test]
    fn confirmed_default_reasoning_clears_override_but_catalog_refresh_does_not() {
        let mut state = Snapshot::default();
        let mut indices = BTreeMap::new();
        apply_event(
            &mut state,
            &mut indices,
            Event::Models {
                models: Vec::new(),
                selected: Some("model".into()),
                reasoning: Some("high".into()),
            },
        );
        apply_event(
            &mut state,
            &mut indices,
            Event::Models {
                models: Vec::new(),
                selected: None,
                reasoning: None,
            },
        );
        assert_eq!(state.reasoning.as_deref(), Some("high"));
        apply_event(
            &mut state,
            &mut indices,
            Event::Models {
                models: Vec::new(),
                selected: Some("model".into()),
                reasoning: None,
            },
        );
        assert_eq!(state.selected_model.as_deref(), Some("model"));
        assert!(state.reasoning.is_none());
    }

    struct Echo;
    #[test]
    fn successful_history_retry_clears_its_error_but_preserves_unrelated_errors() {
        struct HistoryFixture {
            attempts: usize,
        }
        impl Adapter for HistoryFixture {
            fn initialize(&mut self) -> Result<Step, String> {
                Ok(Step {
                    events: vec![
                        Event::Ready(true),
                        Event::History {
                            items: vec![text("current", "current")],
                            has_earlier: true,
                        },
                    ],
                    ..Step::default()
                })
            }
            fn command(&mut self, command: Command) -> Result<Step, String> {
                let events = match command {
                    Command::LoadEarlier => {
                        self.attempts += 1;
                        if self.attempts <= 2 {
                            vec![
                                Event::HistoryLoading(false),
                                Event::HistoryError("page failed".into()),
                            ]
                        } else {
                            vec![Event::History {
                                items: vec![text("older", "older")],
                                has_earlier: false,
                            }]
                        }
                    }
                    Command::SetModel { model, .. } => vec![Event::Error {
                        message: model,
                        fatal: false,
                    }],
                    Command::Compact => vec![Event::TurnStarted],
                    _ => return Err("unexpected fixture command".into()),
                };
                Ok(Step {
                    events,
                    ..Step::default()
                })
            }
            fn receive(&mut self, _: Value) -> Result<Step, String> {
                Ok(Step::default())
            }
        }
        for unrelated in [None, Some("different failure"), Some("page failed")] {
            let spec = ProcessSpec {
                execution: None,
                messages: None,
                unset_environment: Default::default(),
                transport: TransportSpec::JsonLines,
                program: "/bin/cat".into(),
                arguments: Vec::new(),
                working_directory: std::env::temp_dir(),
                environment: BTreeMap::new(),
            };
            let mut adapter: Box<dyn Adapter> = Box::new(HistoryFixture { attempts: 0 });
            let mut initial = adapter.initialize().unwrap();
            let store = preflight(&spec, &mut initial).unwrap();
            let handle = Handle::start(spec, adapter, initial, store, None, |_| {}).unwrap();
            until(&handle, |snapshot| snapshot.ready);
            for _ in 0..2 {
                handle.command(Command::LoadEarlier).unwrap();
                assert_eq!(
                    handle.snapshot(None).unwrap().error.as_deref(),
                    Some("page failed")
                );
            }
            // Starting work cannot repair a failed history page.
            handle.command(Command::Compact).unwrap();
            assert_eq!(
                handle.snapshot(None).unwrap().error.as_deref(),
                Some("page failed")
            );
            if let Some(message) = unrelated {
                handle
                    .command(Command::SetModel {
                        model: message.into(),
                        reasoning: None,
                    })
                    .unwrap();
            }
            handle.command(Command::LoadEarlier).unwrap();
            let snapshot = handle.snapshot(None).unwrap();
            assert_eq!(snapshot.error.as_deref(), unrelated);
            assert_eq!(snapshot.items[0].id, "older");
            handle.command(Command::Compact).unwrap();
            assert!(handle.snapshot(None).unwrap().error.is_none());
            handle.close().unwrap();
        }
    }
    impl Adapter for Echo {
        fn initialize(&mut self) -> Result<Step, String> {
            Ok(Step {
                writes: vec![json!({"text": "hello"})],
                events: Vec::new(),
            })
        }
        fn command(&mut self, command: Command) -> Result<Step, String> {
            if command != Command::Refresh {
                return Err("fixture only supports refresh".into());
            }
            Ok(Step {
                writes: vec![json!({"text": " world"})],
                events: Vec::new(),
            })
        }
        fn receive(&mut self, frame: Value) -> Result<Step, String> {
            Ok(Step {
                writes: Vec::new(),
                events: vec![Event::TextDelta {
                    id: "echo".into(),
                    role: Role::Assistant,
                    thinking: false,
                    delta: frame["text"].as_str().ok_or("missing echo text")?.into(),
                }],
            })
        }
    }
    fn until(handle: &Handle, predicate: impl Fn(&Snapshot) -> bool) -> Snapshot {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let snapshot = handle.snapshot(None).unwrap();
            if predicate(&snapshot) {
                return snapshot;
            }
            assert!(Instant::now() < deadline, "fixture timed out: {snapshot:?}");
            thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn presentation_serves_multiple_readers_and_resyncs_missed_revisions() {
        struct Updates;
        impl Adapter for Updates {
            fn initialize(&mut self) -> Result<Step, String> {
                Ok(Step {
                    events: vec![
                        Event::Item(text("fixed", &"x".repeat(128 * 1024))),
                        Event::Item(text("live", "hello")),
                        Event::Ready(true),
                    ],
                    ..Step::default()
                })
            }
            fn command(&mut self, _: Command) -> Result<Step, String> {
                Ok(Step {
                    events: vec![Event::TextDelta {
                        id: "live".into(),
                        role: Role::Assistant,
                        thinking: false,
                        delta: " world".into(),
                    }],
                    ..Step::default()
                })
            }
            fn receive(&mut self, _: Value) -> Result<Step, String> {
                Ok(Step::default())
            }
        }
        let spec = ProcessSpec {
            execution: None,
            messages: None,
            unset_environment: Default::default(),
            transport: TransportSpec::JsonLines,
            program: "/bin/cat".into(),
            arguments: Vec::new(),
            working_directory: std::env::temp_dir(),
            environment: BTreeMap::new(),
        };
        let mut adapter: Box<dyn Adapter> = Box::new(Updates);
        let mut initial = adapter.initialize().unwrap();
        let store = preflight(&spec, &mut initial).unwrap();
        let handle = Handle::start(spec, adapter, initial, store, None, |_| {}).unwrap();
        let baseline = until(&handle, |snapshot| snapshot.ready);
        let fixed = baseline.items[0].clone();
        let id = crate::model::TerminalId::new();
        handle.command(Command::Refresh).unwrap();
        let Some(crate::terminal_ipc::ServerResponse::ChatUpdate { update, .. }) =
            handle.presentation(id, Some(baseline.generation))
        else {
            panic!("expected incremental update");
        };
        assert!(postcard::to_stdvec(&update).unwrap().len() < 4096);
        let mut first = baseline.clone();
        update.clone().apply(&mut first).unwrap();
        assert!(
            Arc::ptr_eq(&fixed, &first.items[0]),
            "unchanged payload is shared across snapshots"
        );
        // A console/full-snapshot reader at the same revision must not
        // erase the incremental patch another subscriber is about to read.
        assert_eq!(handle.snapshot(None).unwrap().generation, first.generation);
        let Some(crate::terminal_ipc::ServerResponse::ChatUpdate {
            update: mut second, ..
        }) = handle.presentation(id, Some(baseline.generation))
        else {
            panic!("second reader lost its update");
        };
        // The transcript patch is retained; its source clock is freshly
        // sampled for each reader, including readers of cached revisions.
        assert!(update.metadata.sampled_at.is_some());
        assert!(second.metadata.sampled_at.is_some());
        second.metadata.sampled_at = update.metadata.sampled_at;
        assert_eq!(update, second);
        handle.command(Command::Refresh).unwrap();
        assert!(matches!(
            handle.presentation(id, Some(baseline.generation)),
            Some(crate::terminal_ipc::ServerResponse::ChatSnapshot {
                snapshot: Some(_),
                ..
            })
        ));
        assert!(matches!(
            handle.presentation(id, None),
            Some(crate::terminal_ipc::ServerResponse::ChatSnapshot {
                snapshot: Some(_),
                ..
            })
        ));
        handle.close().unwrap();
    }

    #[test]
    fn native_child_receives_only_the_preserved_source_environment() {
        let environment = BTreeMap::from([
            ("HOME".into(), "/source-account".into()),
            ("ANTHROPIC_API_KEY".into(), "explicit-fixture-key".into()),
            ("LC_ALL".into(), "C.UTF-8".into()),
        ]);
        let spec = ProcessSpec {
            execution: None,
            messages: None,
            unset_environment: Default::default(),
            transport: TransportSpec::JsonLines,
            program: "/usr/bin/python3".into(),
            arguments: vec!["-c".into(), "import json,os,sys; sys.stdin.readline(); print(json.dumps({'text':json.dumps(dict(os.environ))}),flush=True); sys.stdin.read()".into()],
            working_directory: std::env::temp_dir(),
            environment: environment.clone(),
        };
        let mut adapter: Box<dyn Adapter> = Box::new(Echo);
        let mut initial = adapter.initialize().unwrap();
        let store = preflight(&spec, &mut initial).unwrap();
        let handle = Handle::start(spec, adapter, initial, store, None, |_| {}).unwrap();
        let snapshot = until(&handle, |snapshot| !snapshot.items.is_empty());
        let ItemContent::Text { text, .. } = &snapshot.items[0].content else {
            panic!("environment fixture did not return text");
        };
        assert_eq!(
            serde_json::from_str::<BTreeMap<String, String>>(text).unwrap(),
            environment
        );
        handle.close().unwrap();
    }
    #[test]
    fn selected_account_removals_win_at_the_native_child_boundary() {
        let spec = ProcessSpec {
            execution: None,
            messages: None,
            transport: TransportSpec::JsonLines,
            program: "/bin/sh".into(),
            arguments: vec!["-c".into(), "read line; [ -z \"${ANTHROPIC_API_KEY+x}\" ] && [ \"$CLAUDE_CONFIG_DIR\" = /selected ] && printf '%s\\n' '{\"text\":\"isolated\"}'; cat >/dev/null".into()],
            working_directory: std::env::temp_dir(),
            environment: BTreeMap::from([
                ("ANTHROPIC_API_KEY".into(), "fixture-merged-credential".into()),
                ("CLAUDE_CONFIG_DIR".into(), "/selected".into()),
                ("PATH".into(), "/usr/bin:/bin".into()),
            ]),
            unset_environment: ["ANTHROPIC_API_KEY".into()].into(),
        };
        let mut adapter: Box<dyn Adapter> = Box::new(Echo);
        let mut initial = adapter.initialize().unwrap();
        let store = preflight(&spec, &mut initial).unwrap();
        let handle = Handle::start(spec, adapter, initial, store, None, |_| {}).unwrap();
        let snapshot = until(&handle, |snapshot| !snapshot.items.is_empty());
        assert!(
            matches!(&snapshot.items[0].content, ItemContent::Text { text, .. } if text == "isolated")
        );
        assert!(
            !handle
                .execution_environment()
                .0
                .contains_key("ANTHROPIC_API_KEY")
        );
        handle.close().unwrap();
    }

    #[test]
    fn legacy_managed_recipe_removes_daemon_credentials_when_restarted() {
        const CHILD: &str = "CAGENTS_LEGACY_ACCOUNT_ENV_TEST_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let directory = tempfile::tempdir().unwrap();
            assert!(std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "chat::runtime::tests::legacy_managed_recipe_removes_daemon_credentials_when_restarted", "--nocapture"])
                .env_clear().env(CHILD, "1").env("HOME", directory.path())
                .env("XDG_DATA_HOME", directory.path().join("data"))
                .env("PATH", "/usr/bin:/bin").env("ANTHROPIC_API_KEY", "daemon-fixture")
                .status().unwrap().success());
            return;
        }
        let home = crate::instance::project_dirs()
            .unwrap()
            .data_dir()
            .join("accounts/claude/12345678-1234-4234-8234-123456789abc");
        crate::account::registry::Store::installation()
            .unwrap()
            .update(|registry| {
                registry.begin(
                    crate::agent::Provider::Claude,
                    uuid::Uuid::parse_str("12345678-1234-4234-8234-123456789abc").unwrap(),
                    1,
                );
            })
            .unwrap();
        let legacy = serde_json::json!({
            "program": "/bin/sh",
            "arguments": ["-c", "read line; [ -z \"${ANTHROPIC_API_KEY+x}\" ] && printf '%s\\n' '{\"text\":\"isolated\"}'; cat >/dev/null"],
            "working_directory": std::env::temp_dir(),
            "environment": {"CLAUDE_CONFIG_DIR": home}
        });
        let recipe: crate::model::LaunchRecipe = serde_json::from_value(legacy).unwrap();
        assert!(
            recipe.unset_environment.is_empty(),
            "fixture must predate explicit policy"
        );
        let mut spec = ProcessSpec::from(&recipe);
        spec.environment = crate::model::agent_recovery::fresh_native_environment(
            crate::agent::Provider::Claude,
            false,
        );
        spec.environment.extend(recipe.environment.clone());
        let mut adapter: Box<dyn Adapter> = Box::new(Echo);
        let mut initial = adapter.initialize().unwrap();
        let store = preflight(&spec, &mut initial).unwrap();
        let handle = Handle::start(spec, adapter, initial, store, None, |_| {}).unwrap();
        let snapshot = until(&handle, |snapshot| !snapshot.items.is_empty());
        assert!(
            matches!(&snapshot.items[0].content, ItemContent::Text { text, .. } if text == "isolated")
        );
        assert_eq!(
            handle.execution_environment().0["CLAUDE_CONFIG_DIR"],
            home.to_str().unwrap()
        );
        assert!(
            !handle
                .execution_environment()
                .0
                .contains_key("ANTHROPIC_API_KEY")
        );
        assert!(
            recipe.unset_environment.is_empty(),
            "spawn must not rewrite live descriptor evidence"
        );
        handle.close().unwrap();
    }
    #[test]
    fn owned_listener_startup_failure_is_reported_and_releases_its_private_directory() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("server").join("control.sock");
        let spec = ProcessSpec {
            execution: None,
            messages: None,
            unset_environment: Default::default(),
            transport: TransportSpec::OwnedUnixWebSocket {
                socket: socket.clone(),
            },
            program: "/bin/false".into(),
            arguments: Vec::new(),
            working_directory: directory.path().into(),
            environment: BTreeMap::new(),
        };
        let mut initial = Step::default();
        let store = preflight(&spec, &mut initial).unwrap();
        let handle = Handle::start(spec, Box::new(Echo), initial, store, None, |_| {}).unwrap();
        let snapshot = until(&handle, |snapshot| snapshot.exited);
        assert!(
            snapshot
                .error
                .as_deref()
                .unwrap()
                .contains("before opening its local socket")
        );
        let _ = handle.close();
        drop(handle);
        assert!(!socket.parent().unwrap().exists());
    }
    #[test]
    fn an_account_report_reaches_the_surfaces_without_touching_the_conversation() {
        struct Reporter;
        impl Adapter for Reporter {
            fn initialize(&mut self) -> Result<Step, String> {
                Ok(Step {
                    writes: vec![json!({})],
                    events: Vec::new(),
                })
            }
            fn command(&mut self, _: Command) -> Result<Step, String> {
                Err("fixture takes no commands".into())
            }
            fn receive(&mut self, _: Value) -> Result<Step, String> {
                Ok(Step {
                    writes: Vec::new(),
                    events: vec![Event::AccountUsage(report())],
                })
            }
        }
        fn report() -> LiveReport {
            LiveReport::Claude(crate::account::claude::LiveUpdate {
                windows: Vec::new(),
                status: None,
            })
        }
        let spec = ProcessSpec {
            execution: None,
            messages: None,
            unset_environment: Default::default(),
            transport: TransportSpec::JsonLines,
            program: "/bin/cat".into(),
            arguments: Vec::new(),
            working_directory: std::env::temp_dir(),
            environment: BTreeMap::new(),
        };
        let mut adapter: Box<dyn Adapter> = Box::new(Reporter);
        let mut initial = adapter.initialize().unwrap();
        let store = preflight(&spec, &mut initial).unwrap();
        let generation = |handle: &Handle| {
            handle
                .0
                .state
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .generation
        };
        let (notify, notified) = crossbeam_channel::bounded(8);
        let handle = Handle::start(spec, adapter, initial, store, None, move |update| {
            let _ = notify.try_send(update);
        })
        .unwrap();
        let before = generation(&handle);
        let Update::AccountUsage {
            report: reported,
            observed_at,
        } = notified.recv_timeout(Duration::from_secs(2)).unwrap()
        else {
            panic!("the report was not forwarded");
        };
        assert_eq!(reported, report());
        assert!(observed_at > 0);
        // Neither the pane nor its snapshot learns of it: a report is not
        // output an offscreen conversation would have to be read for.
        assert!(notified.recv_timeout(Duration::from_millis(200)).is_err());
        assert_eq!(generation(&handle), before);
        assert!(handle.snapshot(None).unwrap().items.is_empty());
        handle.close().unwrap();
    }
    /// The host records a run's outcome the moment it is announced and then
    /// asks the handle which conversation produced it.
    #[test]
    fn a_frame_confirms_its_conversation_before_announcing_its_outcome() {
        use crate::agent_execution::ProviderEvent;
        struct WholeTurn(crate::model::RunId);
        impl Adapter for WholeTurn {
            fn initialize(&mut self) -> Result<Step, String> {
                Ok(Step {
                    writes: vec![json!({})],
                    events: Vec::new(),
                })
            }
            fn command(&mut self, _: Command) -> Result<Step, String> {
                Err("fixture takes no commands".into())
            }
            fn receive(&mut self, _: Value) -> Result<Step, String> {
                let execution = |event| Event::Execution { run: self.0, event };
                Ok(Step {
                    writes: Vec::new(),
                    events: vec![
                        Event::Session {
                            id: "confirmed".into(),
                            title: None,
                        },
                        execution(ProviderEvent::Started {
                            key: "request".into(),
                        }),
                        execution(ProviderEvent::Settled),
                        execution(ProviderEvent::Finished {
                            outcome: crate::notifications::Kind::Succeeded,
                        }),
                    ],
                })
            }
        }
        let spec = ProcessSpec {
            execution: None,
            messages: None,
            unset_environment: Default::default(),
            transport: TransportSpec::JsonLines,
            program: "/bin/cat".into(),
            arguments: Vec::new(),
            working_directory: std::env::temp_dir(),
            environment: BTreeMap::new(),
        };
        let mut adapter: Box<dyn Adapter> = Box::new(WholeTurn(crate::model::RunId::new()));
        let mut initial = adapter.initialize().unwrap();
        let store = preflight(&spec, &mut initial).unwrap();
        let (started, actor) = crossbeam_channel::bounded::<Handle>(1);
        let (observe, observed) = crossbeam_channel::bounded(1);
        let handle = Handle::start(spec, adapter, initial, store, None, move |update| {
            if let Update::Execution {
                update: crate::agent_execution::Update::Finished { .. },
                ..
            } = update
            {
                let handle = actor.recv_timeout(Duration::from_secs(5)).unwrap();
                let _ = observe.try_send(handle.confirmed_session_id());
            }
        })
        .unwrap();
        started.send(handle.clone()).unwrap();
        assert_eq!(
            observed
                .recv_timeout(Duration::from_secs(5))
                .expect("the outcome was never announced")
                .as_deref(),
            Some("confirmed")
        );
        handle.close().unwrap();
    }
    #[test]
    fn real_json_lines_pipe_keeps_hidden_state_without_presentation() {
        let spec = ProcessSpec {
            execution: None,
            messages: None,
            unset_environment: Default::default(),
            transport: TransportSpec::JsonLines,
            program: "/bin/cat".into(),
            arguments: Vec::new(),
            working_directory: std::env::temp_dir(),
            environment: BTreeMap::new(),
        };
        let mut adapter: Box<dyn Adapter> = Box::new(Echo);
        let mut initial = adapter.initialize().unwrap();
        let store = preflight(&spec, &mut initial).unwrap();
        let (notify, notified) = crossbeam_channel::bounded(8);
        let handle = Handle::start(spec, adapter, initial, store, None, move |update| {
            let _ = notify.try_send(update);
        })
        .unwrap();
        assert!(matches!(
            notified.recv_timeout(Duration::from_secs(2)).unwrap(),
            Update::Observed { .. }
        ));
        let generation = handle
            .0
            .state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .generation;
        handle.command(Command::Refresh).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while handle
            .0
            .state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .generation
            < generation + 2
        {
            assert!(Instant::now() < deadline, "hidden echo did not arrive");
            thread::sleep(Duration::from_millis(10));
        }
        assert!(notified.try_recv().is_err(), "hidden deltas must coalesce");
        // Acknowledging an offscreen pane rearms activity, not presentation.
        handle.observe_next_update();
        handle.command(Command::Refresh).unwrap();
        assert!(matches!(
            notified.recv_timeout(Duration::from_secs(2)).unwrap(),
            Update::Observed { .. }
        ));
        while handle
            .0
            .state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .generation
            < generation + 4
        {
            assert!(
                Instant::now() < deadline,
                "acknowledged echo did not arrive"
            );
            thread::sleep(Duration::from_millis(10));
        }
        assert!(
            handle
                .0
                .state
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .items
                .is_empty()
        );
        assert!(
            handle
                .0
                .store
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .dirty
        );
        handle.set_visible(true);
        assert_eq!(
            notified.recv_timeout(Duration::from_secs(2)).unwrap(),
            Update::Presentation
        );
        assert_eq!(
            unstamped(&handle.snapshot(None).unwrap().items),
            vec![text("echo", "hello world world")]
        );
        handle.close().unwrap();
        assert!(handle.is_completed());
        assert_eq!(
            unstamped(&handle.snapshot(None).unwrap().items),
            vec![text("echo", "hello world world")]
        );
    }
    struct WebSocketFixture;
    impl Adapter for WebSocketFixture {
        fn initialize(&mut self) -> Result<Step, String> {
            Echo.initialize()
        }
        fn command(&mut self, command: Command) -> Result<Step, String> {
            if command == Command::Interrupt {
                Ok(Step {
                    writes: vec![json!({"interrupt": true})],
                    events: Vec::new(),
                })
            } else {
                Echo.command(command)
            }
        }
        fn receive(&mut self, frame: Value) -> Result<Step, String> {
            let mut step = Echo.receive(frame.clone())?;
            step.events.push(Event::Status {
                busy: frame["busy"].as_bool().unwrap_or(false),
                detail: String::new(),
            });
            Ok(step)
        }
    }

    #[test]
    fn real_unix_websocket_preserves_hidden_history_pongs_and_interrupts_before_close() {
        use std::os::unix::{fs::MetadataExt, net::UnixListener};
        use tungstenite::Message;
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("codex.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let metadata = std::fs::metadata(&path).unwrap();
        let spec = ProcessSpec {
            execution: None,
            messages: None,
            unset_environment: Default::default(),
            transport: TransportSpec::UnixWebSocket {
                socket: path,
                device: metadata.dev(),
                inode: metadata.ino(),
            },
            // This executable does not exist: a socket connection must never spawn it.
            program: "/nonexistent/codex-websocket-fixture".into(),
            arguments: Vec::new(),
            working_directory: directory.path().into(),
            environment: BTreeMap::new(),
        };
        let (pong, pong_received) = crossbeam_channel::bounded(1);
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            stream
                .set_write_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut socket = tungstenite::accept_hdr(
                stream,
                |request: &tungstenite::handshake::server::Request, response| {
                    assert_eq!(request.uri().path(), "/");
                    assert_eq!(request.headers()["host"], "localhost");
                    Ok(response)
                },
            )
            .unwrap();
            let request = socket.read().unwrap();
            assert!(matches!(&request, Message::Text(_)));
            assert_eq!(
                serde_json::from_str::<Value>(request.to_text().unwrap()).unwrap(),
                json!({"text": "hello"})
            );
            socket
                .send(Message::Text(
                    json!({"text": "hello", "busy": true}).to_string().into(),
                ))
                .unwrap();
            socket
                .send(Message::Ping(b"exact-ping".to_vec().into()))
                .unwrap();
            assert_eq!(
                socket.read().unwrap(),
                Message::Pong(b"exact-ping".to_vec().into())
            );
            pong.send(()).unwrap();
            let request = socket.read().unwrap();
            assert_eq!(
                serde_json::from_str::<Value>(request.to_text().unwrap()).unwrap(),
                json!({"interrupt": true})
            );
            socket
                .send(Message::Text(
                    json!({"text": " stopped", "busy": false})
                        .to_string()
                        .into(),
                ))
                .unwrap();
            assert!(matches!(socket.read().unwrap(), Message::Close(_)));
            // Match the real Codex server: it closes the connection without
            // echoing our close frame after confirming the turn is idle.
        });
        let mut adapter: Box<dyn Adapter> = Box::new(WebSocketFixture);
        let mut initial = adapter.initialize().unwrap();
        let store = preflight(&spec, &mut initial).unwrap();
        let (notify, notified) = crossbeam_channel::bounded(8);
        let handle = Handle::start(spec, adapter, initial, store, None, move |update| {
            let _ = notify.try_send(update);
        })
        .unwrap();
        until(&handle, |snapshot| {
            snapshot.busy && unstamped(&snapshot.items) == vec![text("echo", "hello")]
        });
        pong_received.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(matches!(
            notified.recv_timeout(Duration::from_secs(2)).unwrap(),
            Update::Activity {
                state: ActivityState::Working,
                ..
            }
        ));
        assert!(matches!(
            notified.recv_timeout(Duration::from_secs(2)).unwrap(),
            Update::Observed { .. }
        ));
        assert!(notified.try_recv().is_err());
        let attached = handle.clone();
        attached.set_visible(false);
        drop(attached);
        assert!(!handle.is_completed());
        handle.close().unwrap();
        server.join().unwrap();
        let transitions: Vec<_> = notified
            .try_iter()
            .filter_map(|update| match update {
                Update::Activity { state, .. } => Some(state),
                Update::Presentation => panic!("hidden close must not request presentation"),
                Update::Observed { .. }
                | Update::AccountUsage { .. }
                | Update::Attention(_)
                | Update::Ready
                | Update::Execution { .. } => None,
            })
            .collect();
        assert_eq!(
            transitions,
            vec![ActivityState::Idle, ActivityState::Unknown]
        );
        let snapshot = handle.snapshot(None).unwrap();
        assert!(snapshot.exited);
        assert!(!snapshot.busy);
        assert_eq!(
            unstamped(&snapshot.items),
            vec![text("echo", "hello stopped")]
        );
    }

    #[test]
    fn unix_websocket_rejects_oversized_frame_before_receiving_its_payload() {
        use std::os::unix::{fs::MetadataExt, net::UnixListener};
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("codex.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let metadata = std::fs::metadata(&path).unwrap();
        let spec = ProcessSpec {
            execution: None,
            messages: None,
            unset_environment: Default::default(),
            transport: TransportSpec::UnixWebSocket {
                socket: path,
                device: metadata.dev(),
                inode: metadata.ino(),
            },
            program: "unused".into(),
            arguments: Vec::new(),
            working_directory: directory.path().into(),
            environment: BTreeMap::new(),
        };
        let (release, released) = crossbeam_channel::bounded(1);
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            stream
                .set_write_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut socket = tungstenite::accept(stream).unwrap();
            socket.read().unwrap();
            // An unmasked final text frame with only its oversized length.
            socket.get_mut().write_all(&[0x81, 127]).unwrap();
            socket
                .get_mut()
                .write_all(&((FRAME_LIMIT + 1) as u64).to_be_bytes())
                .unwrap();
            // Keep the connection open: rejection cannot rely on EOF or timeout.
            released.recv_timeout(Duration::from_secs(5)).unwrap();
        });
        let mut adapter: Box<dyn Adapter> = Box::new(Echo);
        let mut initial = adapter.initialize().unwrap();
        let store = preflight(&spec, &mut initial).unwrap();
        let handle = Handle::start(spec, adapter, initial, store, None, |_| {}).unwrap();
        let snapshot = until(&handle, |snapshot| snapshot.exited);
        assert!(snapshot.error.is_some());
        assert!(snapshot.items.is_empty());
        handle
            .close()
            .expect("failed protocol actor can be retired after teardown");
        assert!(
            handle.failure().is_some(),
            "closing retains the failure diagnosis"
        );
        release.send(()).unwrap();
        server.join().unwrap();
    }

    #[test]
    fn non_json_stdout_fails_visibly_instead_of_scraping_ansi() {
        let spec = ProcessSpec {
            execution: None,
            messages: None,
            unset_environment: Default::default(),
            transport: TransportSpec::JsonLines,
            program: "/usr/bin/printf".into(),
            arguments: vec!["not-json\\n".into()],
            working_directory: std::env::temp_dir(),
            environment: BTreeMap::new(),
        };
        let mut initial = Step::default();
        let store = preflight(&spec, &mut initial).unwrap();
        let handle = Handle::start(spec, Box::new(Echo), initial, store, None, |_| {}).unwrap();
        let snapshot = until(&handle, |snapshot| snapshot.exited);
        assert!(
            snapshot
                .error
                .as_deref()
                .unwrap()
                .contains("Unsupported provider stdout")
        );
        handle
            .close()
            .expect("failed provider can be explicitly retired");
        handle.close().expect("completed close is idempotent");
        assert!(
            handle.failure().is_some(),
            "closing retains the failure diagnosis"
        );
    }

    #[test]
    fn a_panicked_join_cannot_be_retired_even_after_a_completion_publication() {
        let spec = ProcessSpec {
            execution: None,
            messages: None,
            unset_environment: Default::default(),
            transport: TransportSpec::JsonLines,
            program: "/usr/bin/printf".into(),
            arguments: vec!["not-json\\n".into()],
            working_directory: std::env::temp_dir(),
            environment: BTreeMap::new(),
        };
        let mut initial = Step::default();
        let store = preflight(&spec, &mut initial).unwrap();
        let handle = Handle::start(spec, Box::new(Echo), initial, store, None, |_| {}).unwrap();
        until(&handle, |snapshot| snapshot.exited);
        handle.close().unwrap();
        // Model a panic after the worker published completion, such as a
        // teardown notification failing. Join outcome must take precedence.
        *handle.0.worker.lock().unwrap() = Some(thread::spawn(|| panic!("fixture join failure")));
        assert!(handle.close().unwrap_err().contains("unconfirmed"));
        assert!(!handle.is_completed());
        assert!(
            handle.close().is_err(),
            "a retry cannot erase uncertain teardown"
        );
    }
    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "requires permitted user/PID namespaces and writable namespace ns_last_pid; run explicitly on Linux"]
    fn direct_provider_teardown_never_signals_a_reused_process_group() {
        use std::os::fd::{FromRawFd, OwnedFd};
        const MARKER: &str = "CAGENTS_AUDIT_PGID_REUSE_CHILD";
        if std::env::var_os(MARKER).is_none() {
            for scenario in ["already-reaped", "retained-leader"] {
                let output = Process::new("/usr/bin/unshare")
                    .args(["--user", "--map-root-user", "--pid", "--fork", "--mount-proc"])
                    .arg(std::env::current_exe().unwrap())
                    .args(["--exact", "chat::runtime::tests::direct_provider_teardown_never_signals_a_reused_process_group", "--nocapture", "--ignored"])
                    .env(MARKER, scenario).output().unwrap();
                let stdout = String::from_utf8_lossy(&output.stdout);
                assert!(
                    stdout.contains("owned PID-reuse fixture started"),
                    "the exact child test must run: {stdout}; {}",
                    String::from_utf8_lossy(&output.stderr)
                );
                assert!(
                    output.status.success(),
                    "owned PID namespace {scenario} regression failed: {stdout}; {}",
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            return;
        }
        let scenario = std::env::var(MARKER).unwrap();
        let already_reaped = scenario == "already-reaped";
        println!("owned PID-reuse fixture started");
        assert_eq!(
            std::process::id(),
            1,
            "only the isolated namespace init may reset PID allocation"
        );
        std::fs::write("/proc/sys/kernel/ns_last_pid", b"199").unwrap();
        let mut original = Process::new("/bin/true");
        // SAFETY: only the async-signal-safe setsid syscall runs after fork.
        unsafe {
            original.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut provider = ChildGuard::new(original.spawn().unwrap()).unwrap();
        let pid = provider.child.id();
        assert_eq!(pid, 200);
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            // The retained case uses the same non-reaping observer as both
            // actual actor paths. The reaped case deliberately models old
            // callers and must refuse group signals after losing ownership.
            let status = if already_reaped {
                provider.child.try_wait().unwrap()
            } else {
                provider.observed_exit_status().unwrap()
            };
            if let Some(status) = status {
                assert_eq!(status.code(), Some(0));
                break;
            }
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(5));
        }
        std::fs::write("/proc/sys/kernel/ns_last_pid", (pid - 1).to_string()).unwrap();
        let mut command = Process::new("/bin/sleep");
        command.arg("30");
        // SAFETY: only the async-signal-safe setsid syscall runs after fork.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let replacement = command.spawn().unwrap();
        if already_reaped {
            assert_eq!(
                replacement.id(),
                pid,
                "the kernel must actually reuse the exited PID"
            );
        } else {
            assert_ne!(
                replacement.id(),
                pid,
                "the observer must keep the exited leader's PID reserved"
            );
        }
        assert_eq!(
            unsafe { libc::getpgid(replacement.id() as i32) },
            replacement.id() as i32
        );
        let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, replacement.id(), 0) } as i32;
        assert!(raw >= 0);
        struct Replacement {
            child: Child,
            pidfd: OwnedFd,
        }
        impl Drop for Replacement {
            fn drop(&mut self) {
                // SAFETY: the pidfd pins only our namespace replacement child.
                unsafe {
                    libc::syscall(
                        libc::SYS_pidfd_send_signal,
                        self.pidfd.as_raw_fd(),
                        libc::SIGKILL,
                        std::ptr::null::<libc::siginfo_t>(),
                        0,
                    );
                }
                let _ = self.child.wait();
            }
        }
        let mut replacement = Replacement {
            child: replacement,
            // SAFETY: pidfd_open returned a new owned descriptor.
            pidfd: unsafe { OwnedFd::from_raw_fd(raw) },
        };
        if already_reaped {
            let cleanup = provider.terminate().unwrap_err();
            assert_eq!(cleanup.raw_os_error(), Some(libc::ECHILD));
        } else {
            provider.terminate().unwrap();
            assert_eq!(provider.child.try_wait().unwrap().unwrap().code(), Some(0));
        }
        let mut poll = libc::pollfd {
            fd: replacement.pidfd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let exited = unsafe { libc::poll(&mut poll, 1, 100) };
        let status = replacement.child.try_wait().unwrap();
        println!(
            "verified reused PID/PGID={pid}; replacement readiness={exited}; status={status:?}"
        );
        assert_eq!(
            exited, 0,
            "closing the original provider signalled its unrelated replacement process group"
        );
        assert!(status.is_none());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn direct_provider_group_cleanup_retains_descendants_and_refuses_lost_ownership() {
        use std::os::fd::{FromRawFd, OwnedFd};
        const MARKER: &str = "CAGENTS_PROVIDER_GROUP_FIXTURE_CHILD";
        if std::env::var_os(MARKER).is_none() {
            let output = Process::new(std::env::current_exe().unwrap())
                .args(["--exact", "chat::runtime::tests::direct_provider_group_cleanup_retains_descendants_and_refuses_lost_ownership", "--nocapture"])
                .env(MARKER, "1").output().unwrap();
            let stdout = String::from_utf8_lossy(&output.stdout);
            assert!(
                stdout.contains("owned provider subreaper fixture started"),
                "exact fixture must run: {stdout}"
            );
            assert!(
                output.status.success(),
                "owned provider cleanup fixture failed: {stdout}; {}",
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        // SAFETY: this isolated one-test process becomes the sole reaper of
        // its own fixture descendants, without changing the parent test runner.
        assert_eq!(
            unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) },
            0
        );
        println!("owned provider subreaper fixture started");
        for already_reaped in [false, true] {
            use std::io::{BufRead, BufReader};
            let script = "import os,signal,sys,time\npid=os.fork()\nif pid==0:\n signal.signal(signal.SIGTERM,signal.SIG_DFL if sys.argv[1]=='default' else signal.SIG_IGN)\n print(os.getpid(),flush=True)\n time.sleep(30)\n os._exit(0)\nsys.stdin.readline()\nos._exit(7)\n";
            let child = Process::new("/usr/bin/python3")
                .args([
                    "-c",
                    script,
                    if already_reaped { "default" } else { "ignore" },
                ])
                .process_group(0)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .unwrap();
            let mut provider = ChildGuard::new(child).unwrap();
            let mut line = String::new();
            BufReader::new(provider.child.stdout.take().unwrap())
                .read_line(&mut line)
                .unwrap();
            let descendant: i32 = line.trim().parse().unwrap();
            assert_eq!(
                unsafe { libc::getpgid(descendant) },
                provider.child.id() as i32
            );
            let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, descendant, 0) } as i32;
            assert!(raw >= 0);
            struct Descendant {
                pid: i32,
                pidfd: OwnedFd,
                reaped: bool,
            }
            impl Drop for Descendant {
                fn drop(&mut self) {
                    if self.reaped {
                        return;
                    }
                    // SAFETY: only the fixture child's pidfd is signalled; this
                    // isolated subreaper can reap that exact adopted child afterward.
                    unsafe {
                        libc::syscall(
                            libc::SYS_pidfd_send_signal,
                            self.pidfd.as_raw_fd(),
                            libc::SIGKILL,
                            std::ptr::null::<libc::siginfo_t>(),
                            0,
                        );
                        libc::waitpid(self.pid, std::ptr::null_mut(), 0);
                    }
                }
            }
            let mut descendant = Descendant {
                pid: descendant,
                reaped: false,
                // SAFETY: pidfd_open returned a new owned descriptor.
                pidfd: unsafe { OwnedFd::from_raw_fd(raw) },
            };
            provider
                .child
                .stdin
                .as_mut()
                .unwrap()
                .write_all(b"exit\n")
                .unwrap();
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                if let Some(status) = if already_reaped {
                    provider.child.try_wait().unwrap()
                } else {
                    provider.observed_exit_status().unwrap()
                } {
                    assert_eq!(status.code(), Some(7));
                    break;
                }
                assert!(Instant::now() < deadline);
                thread::sleep(Duration::from_millis(5));
            }
            if already_reaped {
                assert_eq!(
                    provider.terminate().unwrap_err().raw_os_error(),
                    Some(libc::ECHILD)
                );
                let mut poll = libc::pollfd {
                    fd: descendant.pidfd.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                };
                assert_eq!(
                    unsafe { libc::poll(&mut poll, 1, 100) },
                    0,
                    "lost leader ownership must refuse every group signal"
                );
                continue;
            }
            provider.terminate().unwrap();
            assert_eq!(provider.child.try_wait().unwrap().unwrap().code(), Some(7));
            let mut poll = libc::pollfd {
                fd: descendant.pidfd.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            assert_eq!(
                unsafe { libc::poll(&mut poll, 1, 1000) },
                1,
                "cleanup must retain its existing SIGKILL escalation for original group descendants"
            );
            let mut status = 0;
            assert_eq!(
                unsafe { libc::waitpid(descendant.pid, &mut status, 0) },
                descendant.pid
            );
            descendant.reaped = true;
            assert_eq!(libc::WTERMSIG(status), libc::SIGKILL);
        }
    }

    #[test]
    fn direct_provider_non_reaping_observation_preserves_exit_status() {
        for script in ["exit 7", "kill -TERM $$"] {
            let child = Process::new("/bin/sh")
                .args(["-c", script])
                .process_group(0)
                .spawn()
                .unwrap();
            let mut provider = ChildGuard::new(child).unwrap();
            let deadline = Instant::now() + Duration::from_secs(5);
            let observed = loop {
                if let Some(status) = provider.observed_exit_status().unwrap() {
                    break status;
                }
                assert!(Instant::now() < deadline);
                thread::sleep(Duration::from_millis(5));
            };
            provider.terminate().unwrap();
            assert_eq!(
                provider.child.try_wait().unwrap().unwrap(),
                observed,
                "non-reaping observation must match the real Child exit status for {script}"
            );
        }
    }

    #[test]
    fn direct_provider_scope_preserves_pipes_and_process_group() {
        use std::io::{BufRead, Write};
        let spec = ProcessSpec {
            execution: None,
            messages: None,
            transport: TransportSpec::JsonLines,
            program: "/bin/cat".into(),
            arguments: Vec::new(),
            working_directory: std::env::current_dir().unwrap(),
            environment: Default::default(),
            unset_environment: Default::default(),
        };
        let mut provider = spawn_provider(&spec, &AtomicBool::new(false))
            .unwrap()
            .unwrap();
        let pid = provider.child.id();
        crate::daemon::process_scope::tests::assert_separate(pid, pid);
        // SAFETY: querying our unreaped child, never changing its process group.
        assert_eq!(unsafe { libc::getpgid(pid as i32) }, pid as i32);
        provider
            .child
            .stdin
            .as_mut()
            .unwrap()
            .write_all(b"scope transport preserved\n")
            .unwrap();
        let mut reply = String::new();
        std::io::BufReader::new(provider.child.stdout.take().unwrap())
            .read_line(&mut reply)
            .unwrap();
        assert_eq!(reply, "scope transport preserved\n");
        provider.terminate().unwrap();
    }
}
