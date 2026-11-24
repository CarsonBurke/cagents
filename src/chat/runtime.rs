//! Daemon-owned, non-PTY provider actor. Detaching never stops its transport.
use super::{
    AgentAddress, Command, Event, Item, ItemContent, Phase, Snapshot, Subagent, ToolStatus,
    protocol::{Adapter, HeldStop, MetadataWake, ProcessSpec, SetupError, Step, TransportSpec},
};
use crate::account::LiveReport;
use crate::agent::ActivityState;
use crate::daemon::owned_children::DetachedWork;
#[cfg(test)]
use std::sync::atomic::AtomicUsize;
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
        atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
mod child_diagnostics;
mod child_text_copy;
#[cfg(test)]
pub(crate) mod courier_tests;
mod delivery;
use crate::chat::DeliveryKey;
pub(crate) use delivery::Deliveries;
mod execution;
mod handle;
pub use handle::Handle;
pub(crate) use handle::ReadFailure;
#[cfg(test)]
pub(crate) use handle::RetainedInputOutcome;
/// Process evidence only, without the reader lease: no retirement authority.
pub(crate) use retained_actor::execution_exit::abandoned as retained_actor_abandoned;
pub(crate) use retained_actor::execution_exit::codex_abandoned as codex_execution_abandoned;
#[cfg(test)]
pub(crate) use retained_actor::journal::ContinuityLoss;
pub(crate) use retained_actor::journal::OutboxEntry;
#[cfg(test)]
pub(crate) use retained_actor::transport::scripted_helper::{Script, ScriptedHelper};
pub(crate) use retained_actor::transport::{
    ActorIdentity, RetirementInspection, VerifiedRetirement,
};
mod retained_actor;
/// Synthetic retained-actor receipts for the daemon's collection tests.
#[cfg(test)]
pub(crate) use retained_actor::receipt_fixture as retained_receipt_fixture;
mod store;
pub use store::Unread;
mod websocket;
pub(crate) use store::Store;

/// How long a caller waits for the actor to acknowledge a command before its
/// outcome is unknown.
const COMMAND_ACKNOWLEDGEMENT: Duration = Duration::from_secs(10);

/// A command the actor did not acknowledge in time, which it may still run.
fn unacknowledged_command() -> Rejection {
    Rejection::Unacknowledged(
        "Provider command acknowledgement timed out; do not retry blindly".into(),
    )
}

const FRAME_LIMIT: usize = 32 * 1024 * 1024;
/// A settled provider has no partial frame to preserve. Keep enough space for
/// ordinary protocol lines, but do not make every retained actor keep the
/// largest tool result it has ever received.
const IDLE_FRAME_BUFFER_CAPACITY: usize = 64 * 1024;
const STATE_LIMIT: usize = 1024 * 1024;
const QUEUE_LIMIT: usize = 64 * 1024 * 1024;
const IO_TIMEOUT: Duration = Duration::from_secs(30);
/// How long provider frames can keep extending an unfinished initialization.
/// Hooks, MCP servers and a large resume can legitimately outlast `IO_TIMEOUT`.
const STARTUP_LIMIT: Duration = Duration::from_secs(300);
// Pipe EOF may precede the kernel's child-exit publication. Do not turn that
// short interval into an uncertain provider owner, or wait for a live writer.
const PROTOCOL_EXIT_GRACE: Duration = Duration::from_millis(100);
const PRESENT_INTERVAL: Duration = Duration::from_millis(50);
/// Frames parsed per poll iteration: bounded so a provider flood cannot starve
/// the stdin writer or queued UI commands.
const FRAMES_PER_POLL: usize = 256;
/// Queued stdin bytes above which new commands wait for the provider to read.
const QUEUE_BACKPRESSURE: usize = QUEUE_LIMIT / 2;

/// History readers to hold before their first frame, by working directory.
/// Start returns once the worker is running, before it applies that frame, so
/// a caller can observe a reader with nothing applied yet. Holding one makes
/// that interleaving certain rather than occasional.
#[cfg(test)]
static HELD_HISTORY_READERS: Mutex<Vec<(std::path::PathBuf, std::sync::mpsc::Receiver<()>)>> =
    Mutex::new(Vec::new());

/// Hold the next history reader started in `directory` before it applies its
/// initial frame, until the returned sender sends or is dropped.
#[cfg(test)]
pub(crate) fn hold_history_reader(directory: &std::path::Path) -> std::sync::mpsc::Sender<()> {
    let (release, held) = std::sync::mpsc::channel();
    HELD_HISTORY_READERS
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .push((directory.to_owned(), held));
    release
}

#[cfg(test)]
fn await_history_release(directory: &std::path::Path) {
    let held = {
        let mut held = HELD_HISTORY_READERS
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        held.iter()
            .position(|(held, _)| held == directory)
            .map(|index| held.remove(index).1)
    };
    if let Some(held) = held {
        let _ = held.recv();
    }
}

/// The refusal of a decline-only answer whose action the provider doesn't
/// read as turning the request down, or whose request is no longer pending.
pub(crate) const NOT_A_DECLINE: &str = "A settled run may only decline a current pending request";

enum ActorCommand {
    RetainedProvider {
        run: Option<crate::model::RunId>,
        command: Command,
        decline_only: bool,
        effects: Arc<AtomicU8>,
    },
    /// Boxed: rare, and several times the size of every other command.
    UpdateSource(Box<SourceUpdate>),
    Provider {
        run: Option<crate::model::RunId>,
        command: Command,
        decline_only: bool,
    },
    /// Courier input must not pass over a request arriving after its idle scan.
    IdlePrompt(String),
    CloseIfIdle {
        generation: u64,
        /// What a granted close leaves running, decided as it is granted.
        /// A close that decides it also refuses while the provider reports
        /// live background tasks, which it would end.
        detached: Option<DetachedWork>,
        closed: crossbeam_channel::Sender<bool>,
    },
    CheckUpdateReadiness(crossbeam_channel::Sender<UpdateReadiness>),
    PinnedLaunchDirectory(
        crossbeam_channel::Sender<
            Result<
                (
                    crate::agent_execution::DirectoryIdentity,
                    Arc<std::fs::File>,
                ),
                String,
            >,
        >,
    ),
    Interrupt {
        run: Option<crate::model::RunId>,
        including_subagents: bool,
    },
}

struct SourceUpdate {
    source: super::Session,
    execution_console: Option<crate::model::LaunchRecipe>,
    paired_holder: Option<crate::terminal::holder::Record>,
}

/// A refusal before a Step is prepared proves no provider effects; after a
/// Step is admitted, an uncertain acknowledgement never authorizes replay.
struct InputEffects(Option<Arc<AtomicU8>>);
impl InputEffects {
    fn possible(&self) {
        if let Some(effects) = &self.0 {
            effects.store(2, Ordering::Release);
        }
    }
}
impl Drop for InputEffects {
    fn drop(&mut self) {
        if let Some(effects) = &self.0 {
            let _ = effects.compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire);
        }
    }
}

type CommandRequest = (
    ActorCommand,
    crossbeam_channel::Sender<Result<(), Rejection>>,
);

/// Why a provider command may not have taken effect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Rejection {
    /// The actor never took the command, or refused it before effects were possible.
    Refused(String),
    /// The actor did not answer in time, or failed after effects became possible;
    /// the provider may still receive it. Never replay automatically.
    Unacknowledged(String),
}

impl From<Rejection> for String {
    fn from(rejection: Rejection) -> Self {
        match rejection {
            Rejection::Refused(error) | Rejection::Unacknowledged(error) => error,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) enum UpdateReadiness {
    Idle,
    Busy,
    Unready,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub(crate) enum Update {
    /// Helper outbox has uncommitted semantic evidence; wake the host broker.
    RetainedOutcomes,
    Execution {
        run: crate::model::RunId,
        update: crate::agent_execution::Update,
    },
    Attention(crate::notifications::Evidence),
    /// Immutable same-frame context; optional only when the frame context
    /// budget is exhausted. Never resample a later Snapshot for this outcome.
    AttentionContext {
        evidence: crate::notifications::Evidence,
        content: Option<crate::notifications::Content>,
    },
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
    /// The parent can accept a new prompt, independently of running children.
    PromptAvailable,
    /// The provider reported on the account the conversation runs on. It
    /// concerns every surface showing the account, whatever this
    /// conversation's visibility, and changes nothing in the snapshot.
    AccountUsage {
        report: LiveReport,
        /// Unix milliseconds at which the runtime read the report.
        observed_at: u64,
    },
    /// The provider said how it addresses another of the conversation's
    /// subagents. Every address known so far, which only accumulate; the
    /// snapshot, whose layout older peers share, holds none.
    AgentAddresses(Vec<AgentAddress>),
    ActiveModel {
        revision: u64,
        model: Option<super::ActiveModel>,
    },
    ConversationNameChanged,
}

pub(crate) struct ConversationNaming {
    pub session: Option<String>,
    pub provider: Option<String>,
    pub provider_cleared: bool,
    pub opening: Option<super::OpeningTitle>,
}

impl ConversationNaming {
    pub(crate) fn title(&self) -> Option<(String, crate::model::ChannelTitleSource)> {
        use crate::model::ChannelTitleSource::{Prompt, Provider};
        self.provider
            .as_deref()
            .and_then(crate::workspace_model::channel_title)
            .map(|title| (title, Provider))
            .or_else(|| {
                self.opening
                    .as_ref()
                    .map(|title| (title.text().to_owned(), Prompt))
            })
    }
}

/// The conversation's prompt cache activity, which the actor records and
/// its handle reads. Like the agent addresses, it stays out of the snapshot,
/// whose layout older peers share.
#[derive(Clone, Default)]
struct PromptCache(Arc<Mutex<crate::chat::prompt_cache::Tracker>>);

impl PromptCache {
    fn observe(&self, evidence: crate::chat::prompt_cache::Evidence) -> bool {
        // Without a clock nothing says when the request started.
        let Some(now) = observed_at() else {
            return false;
        };
        self.0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .observe(evidence, now)
    }

    fn activity(&self) -> crate::chat::prompt_cache::Activity {
        self.0.lock().unwrap_or_else(|p| p.into_inner()).activity()
    }

    fn model(&self, model: Option<&str>) {
        self.0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .model(model);
    }
}

#[derive(Clone)]
struct LocalHandle(Arc<Inner>);

struct OwnedSocket(std::path::PathBuf);
impl Drop for OwnedSocket {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
        if let Some(directory) = self.0.parent() {
            let _ = std::fs::remove_dir(directory);
        }
    }
}

impl LocalHandle {
    pub(crate) fn same_actor(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum TitleUnavailable {
    Busy,
    Poisoned,
}

/// How the provider addresses a conversation's subagents, shared between its
/// actor, which learns them, and its handle, which reports them.
#[derive(Clone, Default)]
struct AgentAddresses(Arc<Mutex<BTreeMap<String, String>>>);

impl AgentAddresses {
    /// Records an address, and says whether it was news. An address keeps
    /// the agent it first named.
    fn learn(&self, address: AgentAddress) -> bool {
        let mut known = self.0.lock().unwrap_or_else(|p| p.into_inner());
        if known.contains_key(&address.address) {
            return false;
        }
        known.insert(address.address, address.subagent);
        true
    }

    fn list(&self) -> Vec<AgentAddress> {
        self.0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .map(|(address, subagent)| AgentAddress {
                address: address.clone(),
                subagent: subagent.clone(),
            })
            .collect()
    }
}

struct Inner {
    commands: crossbeam_channel::Sender<CommandRequest>,
    wake: Mutex<UnixStream>,
    state: Arc<Mutex<Snapshot>>,
    confirmed_session: Arc<Mutex<Option<String>>>,
    agent_addresses: AgentAddresses,
    prompt_cache: PromptCache,
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
    /// What closing does with the work the provider already detached, once
    /// its owner decides. See [`DetachedDecision`].
    detached_work: Arc<DetachedDecision>,
    detached_native: Arc<Mutex<Option<super::native_holder::Detached>>>,
    retained_native: Arc<Mutex<Option<super::native_holder::Holder>>>,
    native_installed: Arc<AtomicBool>,
    history_only: bool,
    provider_process: Arc<Mutex<Option<crate::daemon::holder_registry::Process>>>,
    /// Milliseconds a launched server spent blocked behind another process's
    /// file lock before accepting. See [`Handle::startup_lock_wait`].
    startup_lock_wait: Arc<AtomicU64>,
    launch_account_scope: Result<crate::account::scope::LaunchAccountScope, String>,
    #[cfg(test)]
    idle_frame_buffer_capacity: Arc<AtomicUsize>,
}

enum NativeOwnership {
    New(Box<super::Session>, BTreeMap<String, String>),
    Adopted(
        Box<super::native_holder::Holder>,
        crate::daemon::holder_registry::Process,
    ),
}
/// Why the provider actor did not queue a command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum QueueRefusal {
    /// The actor has finished or is shutting down.
    Closed,
    /// Its command queue is full for now.
    Full,
}
impl From<QueueRefusal> for String {
    fn from(refusal: QueueRefusal) -> Self {
        match refusal {
            QueueRefusal::Closed => "Provider actor is no longer accepting commands",
            QueueRefusal::Full => "Chat command queue is full",
        }
        .into()
    }
}

impl LocalHandle {
    #[cfg(test)]
    fn idle_frame_buffer_capacity(&self) -> usize {
        self.0.idle_frame_buffer_capacity.load(Ordering::Acquire)
    }

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

    /// Retain an actual native holder after closing the fixture's local actor.
    /// Cleanup still requires the real holder protocol and receipt.
    #[cfg(test)]
    pub(crate) fn fixture_retained_native_holder(
        &self,
        holder: super::native_holder::Holder,
        reason: String,
    ) {
        assert!(self.is_completed(), "fixture requires a closed actor");
        assert!(self.0.worker.lock().unwrap().is_none());
        let mut retained = self.0.retained_native.lock().unwrap();
        assert!(retained.is_none());
        *retained = Some(holder);
        *self.0.failure.lock().unwrap() = Some(reason);
        self.0.completed.store(false, Ordering::Release);
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
            Some(NativeOwnership::New(Box::new(source), recovery_environment)),
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
            Some(NativeOwnership::Adopted(Box::new(holder), server)),
            notify,
        )
    }

    fn start_inner(
        spec: ProcessSpec,
        adapter: Box<dyn Adapter>,
        initial: Step,
        store: Store,
        session_id: Option<String>,
        ownership: Option<NativeOwnership>,
        notify: impl Fn(Update) + Send + Sync + 'static,
    ) -> Result<Self, String> {
        Self::start_with_scope(
            spec,
            adapter,
            initial,
            store,
            session_id,
            ownership,
            None,
            None,
            Default::default(),
            notify,
        )
    }

    /// The helper captures its immutable physical account before spawning.
    /// `notify` also receives the conversation the provider had confirmed
    /// when it published each update. A frame's confirmation is installed
    /// before the outcomes it settles are published.
    pub(super) fn start_retained(
        spec: ProcessSpec,
        adapter: Box<dyn Adapter>,
        initial: Step,
        store: Store,
        source: super::Session,
        notify: impl Fn(Update, Option<String>) + Send + Sync + 'static,
        resident_changed: impl Fn() + Send + Sync + 'static,
    ) -> Result<Self, String> {
        if source.launch.provider != crate::agent::Provider::Claude {
            return Err("Retained stream actors require Claude".into());
        }
        let scope = crate::account::scope::LaunchAccountScope::resolve(
            source.launch.provider,
            &spec.working_directory,
            |name| spec.environment.get(name).map(std::ffi::OsString::from),
        )?;
        let confirmed_session = Arc::new(Mutex::new(None));
        let followed = confirmed_session.clone();
        Self::start_with_scope(
            spec,
            adapter,
            initial,
            store,
            source.launch.session_id,
            None,
            Some(scope),
            Some(Arc::new(resident_changed)),
            confirmed_session,
            move |update| {
                let confirmed = followed.lock().unwrap_or_else(|p| p.into_inner()).clone();
                notify(update, confirmed)
            },
        )
    }

    fn start_with_scope(
        mut spec: ProcessSpec,
        adapter: Box<dyn Adapter>,
        initial: Step,
        store: Store,
        session_id: Option<String>,
        ownership: Option<NativeOwnership>,
        launch_account_scope: Option<crate::account::scope::LaunchAccountScope>,
        resident_changed: Option<Arc<dyn Fn() + Send + Sync>>,
        confirmed_session: Arc<Mutex<Option<String>>>,
        notify: impl Fn(Update) + Send + Sync + 'static,
    ) -> Result<Self, String> {
        spec.withhold_environment();
        // Compare the reconstructible launch context, before the daemon adds
        // its transient capability and matched CLI path. Never persist those
        // additions or weaken comparisons of the user's selected environment.
        let recovery_environment = match &ownership {
            Some(NativeOwnership::New(_, environment)) => environment.clone(),
            _ => spec.environment.clone(),
        };
        if !matches!(spec.transport, TransportSpec::History)
            && !matches!(&ownership, Some(NativeOwnership::Adopted(..)))
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
            // The allocation root under the daemon's runtime directory exists
            // only once a listener has been allocated there. Only the
            // conversation's own directory below it must be new or private.
            if let Some(root) = directory.parent() {
                std::fs::DirBuilder::new()
                    .recursive(true)
                    .mode(0o700)
                    .create(root)
                    .map_err(|error| {
                        format!(
                            "Cannot create provider socket directory {}: {error}",
                            root.display()
                        )
                    })?;
            }
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
        let provider_process = Arc::new(Mutex::new(None));
        let startup_lock_wait = Arc::new(AtomicU64::new(0));
        let startup_budget = StartupBudget::current();
        #[cfg(test)]
        let idle_frame_buffer_capacity = Arc::new(AtomicUsize::new(usize::MAX));
        let native_holder = match ownership {
            Some(NativeOwnership::New(source, _))
                if matches!(spec.transport, TransportSpec::OwnedUnixWebSocket { .. }) =>
            {
                Some(super::native_holder::spawn(spec.clone(), *source)?)
            }
            Some(NativeOwnership::Adopted(holder, _)) => Some(*holder),
            _ => None,
        };
        let (wake, reader) = UnixStream::pair().map_err(|e| e.to_string())?;
        nonblocking(&wake)?;
        nonblocking(&reader)?;
        // Optional: without it a stopped command is reported at the next wake.
        let scope_wake = wake.try_clone().ok();
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
        let agent_addresses = AgentAddresses::default();
        let prompt_cache = PromptCache::default();
        let preserve_native = Arc::new(AtomicBool::new(false));
        let release_native = Arc::new(AtomicBool::new(false));
        let retain_cleanup_receipt = Arc::new(AtomicBool::new(false));
        let detached_work = Arc::new(DetachedDecision::default());
        let detached_native = Arc::new(Mutex::new(None));
        let retained_native = Arc::new(Mutex::new(None));
        let notify: Arc<dyn Fn(Update) + Send + Sync> = Arc::new(notify);
        let inner = Arc::new(Inner {
            commands,
            wake: Mutex::new(wake),
            state: state.clone(),
            confirmed_session: confirmed_session.clone(),
            agent_addresses: agent_addresses.clone(),
            prompt_cache: prompt_cache.clone(),
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
            detached_work: detached_work.clone(),
            detached_native: detached_native.clone(),
            retained_native: retained_native.clone(),
            native_installed: native_installed.clone(),
            history_only: matches!(spec.transport, TransportSpec::History),
            provider_process: provider_process.clone(),
            startup_lock_wait: startup_lock_wait.clone(),
            launch_account_scope: launch_account_scope
                .clone()
                .ok_or_else(|| "This local actor has no retained account scope".to_owned()),
            #[cfg(test)]
            idle_frame_buffer_capacity: idle_frame_buffer_capacity.clone(),
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
                    launch_account_scope,
                    working_directory: spec.working_directory.clone(),
                    child: None,
                    native_holder,
                    startup_lock_wait,
                    reconnect_only_reported: false,
                    command_stops: 0,
                    expected_native_peer,
                    native_peer: None,
                    preserve_native: preserve_native.clone(),
                    native_installed: native_installed.clone(),
                    adapter,
                    state,
                    confirmed_session,
                    agent_addresses,
                    prompt_cache,
                    store,
                    visible,
                    observe_next,
                    stop,
                    detached_work: detached_work.clone(),
                    incoming,
                    wake: reader,
                    notify,
                    resident_changed,
                    resident_dirty: false,
                    output: Vec::new(),
                    scanned: 0,
                    pending: VecDeque::new(),
                    offset: 0,
                    queued: 0,
                    stderr: VecDeque::new(),
                    indices: BTreeMap::new(),
                    retained: 0,
                    history_error: None,
                    login_recovery: None,
                    dirty: false,
                    activity: ActivityState::Idle,
                    ready: false,
                    prompt_available: false,
                    attention: TurnAttention::default(),
                    execution: execution::Tracker::default(),
                    attention_baselined: false,
                    waiting_fingerprint: None,
                    last_present: Instant::now(),
                    first_frame: false,
                    stdout_noise: spec.stdout_noise,
                    started: Instant::now(),
                    startup: startup_budget,
                    write_started: None,
                    #[cfg(test)]
                    idle_frame_buffer_capacity,
                };
                let result = {
                    let _recovered = crate::instance::recovered_panic_scope();
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
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
                            spawn_provider(
                                &spec,
                                &actor.stop,
                                scope_wake,
                                actor.adapter.needs_private_temp(),
                            )
                        };
                        let child = child.and_then(|child| {
                            if let Some(child) = &child {
                                *provider_process.lock().unwrap_or_else(|p| p.into_inner()) = Some(
                                    crate::daemon::holder_registry::Process::read(
                                        child.child.id() as i32
                                    )
                                    .map_err(|error| error.to_string())?,
                                );
                            }
                            Ok(child)
                        });
                        let _ = started.send(child.as_ref().map(|_| ()).map_err(Clone::clone));
                        actor.child = child?;
                        match spec.transport {
                            TransportSpec::History => {
                                #[cfg(test)]
                                await_history_release(&spec.working_directory);
                                actor.run_history(initial)
                            }
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
                }
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
                    let attention = (!actor.attention.settled && !preserve_holder).then(|| {
                        crate::notifications::Evidence {
                            key: actor.attention.key.clone(),
                            kind: crate::notifications::Kind::Interrupted,
                        }
                    });
                    *failure.lock().unwrap_or_else(|p| p.into_inner()) = Some(error.clone());
                    let stderr =
                        String::from_utf8_lossy(actor.stderr.make_contiguous()).into_owned();
                    let message = if stderr.is_empty() {
                        error
                    } else {
                        format!("{error}\nProvider stderr (last 16 KiB): {stderr}")
                    };
                    actor.apply(Event::Error {
                        message,
                        fatal: true,
                    });
                    // Publish the bounded diagnosis before attention samples
                    // it, while keeping retained native outcomes unconfirmed.
                    if let Some(evidence) = attention {
                        actor.publish_current_attention(evidence);
                    }
                }
                let mut direct_closed = true;
                let code = actor.child.as_mut().and_then(|child| {
                    let code = child
                        .observed_exit_status()
                        .ok()
                        .flatten()
                        .and_then(|s| s.code());
                    child.stop_detached = detached_work.get() == Some(DetachedWork::Stop);
                    let result = child.terminate();
                    if child.force_stopped == Some(true) {
                        detached_work.force_stopped();
                    }
                    if let Err(error) = result {
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
                    } else if let Err(error) = holder.close_inner(
                        retain_cleanup_receipt.load(Ordering::Acquire),
                        detached_work.holder(),
                    ) {
                        *failure.lock().unwrap_or_else(|p| p.into_inner()) = Some(error);
                        *retained_native.lock().unwrap_or_else(|p| p.into_inner()) = Some(holder);
                        false
                    } else {
                        true
                    }
                } else {
                    true
                };
                // Nothing reports the end of a step the provider left running.
                // A preserved holder's provider is still running its steps.
                if !preserve_holder {
                    if let Err(error) = actor
                        .store
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .stop_running_steps()
                    {
                        tracing::warn!(%error, "steps left running could not be marked stopped");
                    }
                    stop_snapshot_steps(&mut actor.state.lock().unwrap_or_else(|p| p.into_inner()));
                }
                actor.apply(Event::Exited { code });
                let usage = actor
                    .state
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .usage
                    .clone();
                for (run, update) in
                    actor
                        .execution
                        .ended(actor.stop.load(Ordering::Acquire), code, usage)
                {
                    (actor.notify)(Update::Execution { run, update });
                }
                actor.publish_activity();
                completed.store(native_closed && direct_closed, Ordering::Release);
                if native_closed && direct_closed {
                    // Completion can leave a detached daemon with no other
                    // event. Publish once after the ownership flag, even when
                    // its hidden observation was already consumed. Reuse the
                    // existing helper-compatible metadata notification.
                    actor.observe_next.store(true, Ordering::Release);
                }
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
    pub(super) fn pinned_launch_directory(
        &self,
    ) -> Result<
        (
            crate::agent_execution::DirectoryIdentity,
            Arc<std::fs::File>,
        ),
        String,
    > {
        let (reply, response) = crossbeam_channel::bounded(1);
        let _acknowledgement = self.enqueue_actor(ActorCommand::PinnedLaunchDirectory(reply))?;
        response
            .recv_timeout(Duration::from_secs(5))
            .map_err(|_| "Provider launch-directory owner did not respond".to_owned())?
    }
    pub(super) fn provider_process(&self) -> Option<crate::daemon::holder_registry::Process> {
        self.0
            .provider_process
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }
    pub(super) fn launch_account_scope(
        &self,
    ) -> Result<&crate::account::scope::LaunchAccountScope, &str> {
        self.0.launch_account_scope.as_ref().map_err(String::as_str)
    }
    /// Queue admission is bounded; only the helper owns waiting and receipts.
    pub(super) fn enqueue_actor(
        &self,
        command: ActorCommand,
    ) -> Result<crossbeam_channel::Receiver<Result<(), Rejection>>, QueueRefusal> {
        if self.is_completed()
            || self.0.draining.load(Ordering::Acquire)
            || self.0.stop.load(Ordering::Acquire)
        {
            return Err(QueueRefusal::Closed);
        }
        let (reply, response) = crossbeam_channel::bounded(1);
        self.0
            .commands
            .try_send((command, reply))
            .map_err(|error| match error {
                crossbeam_channel::TrySendError::Full(_) => QueueRefusal::Full,
                crossbeam_channel::TrySendError::Disconnected(_) => QueueRefusal::Closed,
            })?;
        self.wake();
        Ok(response)
    }

    #[cfg(test)]
    pub fn command(&self, command: Command) -> Result<(), String> {
        self.submit_command(None, command, false)
            .map_err(String::from)
    }

    pub(crate) fn try_command(&self, command: Command) -> Result<(), Rejection> {
        self.submit_command(None, command, false)
    }
    /// Rechecks courier eligibility on the actor, immediately before preparing
    /// the prompt. A caller's earlier idle observation is only a hint. Refusal
    /// permits retry; an unacknowledged command may already have run.
    pub(crate) fn try_idle_prompt(&self, text: String) -> Result<(), Rejection> {
        self.submit_actor_command(ActorCommand::IdlePrompt(text))
    }

    pub(crate) fn command_for_run(
        &self,
        run: crate::model::RunId,
        command: Command,
    ) -> Result<(), String> {
        self.submit_command(Some(run), command, false)
            .map_err(String::from)
    }

    /// Resolve only a current refusal, checked by the provider actor itself.
    pub(crate) fn decline_answer(&self, command: Command) -> Result<(), String> {
        self.submit_command(None, command, true)
            .map_err(String::from)
    }

    fn submit_command(
        &self,
        run: Option<crate::model::RunId>,
        command: Command,
        decline_only: bool,
    ) -> Result<(), Rejection> {
        if matches!(
            &command,
            Command::LoadFileChanges { .. }
                | Command::LoadFileChangePatches { .. }
                | Command::ReadTextCopy { .. }
        ) {
            return Err(Rejection::Refused(
                "File cache reads require a requester-specific reply".into(),
            ));
        }
        // `completed` is published only once teardown finished; `draining` the moment
        // the actor stopped consuming its command channel.
        let exiting = self.is_completed() || self.0.draining.load(Ordering::Acquire);
        if exiting
            && matches!(
                command,
                Command::LoadEarlier | Command::LoadLater | Command::Refresh
            )
        {
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
        self.submit_actor_command(ActorCommand::Provider {
            run,
            command,
            decline_only,
        })
    }

    fn deliver(
        &self,
        deliveries: &Deliveries,
        chat: crate::model::TerminalId,
        command: Command,
        key: DeliveryKey,
        given_work: impl FnOnce(),
    ) -> Result<(), Rejection> {
        deliveries.deliver(
            chat,
            key,
            COMMAND_ACKNOWLEDGEMENT,
            unacknowledged_command,
            || {
                given_work();
                self.enqueue_actor_command(ActorCommand::Provider {
                    run: None,
                    command,
                    decline_only: false,
                })
            },
        )
    }

    fn submit_actor_command(&self, command: ActorCommand) -> Result<(), Rejection> {
        self.enqueue_actor_command(command)?
            .recv_timeout(COMMAND_ACKNOWLEDGEMENT)
            .map_err(|_| unacknowledged_command())?
    }

    fn enqueue_actor_command(&self, command: ActorCommand) -> Result<delivery::Reply, Rejection> {
        if self.is_completed() || self.0.draining.load(Ordering::Acquire) {
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
            .try_send((command, reply))
            .map_err(|e| Rejection::Refused(format!("Chat command queue unavailable: {e}")))?;
        self.wake();
        Ok(response)
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
            .map_err(String::from)
    }

    fn claim_idle_close(
        &self,
        generation: u64,
        detached: Option<DetachedWork>,
    ) -> Result<bool, String> {
        let (closed, response) = crossbeam_channel::bounded(1);
        self.ownership_command(ActorCommand::CloseIfIdle {
            generation,
            detached,
            closed,
        })?;
        response
            .try_recv()
            .map_err(|error| format!("Provider idle close was not acknowledged: {error}"))
    }

    /// False proves the actor was idle but its generation changed. No close
    /// occurred; the caller may take a new snapshot and retry its exact owner.
    /// Nonidle state and uncertain acknowledgement remain errors.
    pub(crate) fn try_close_if_idle(&self, generation: u64) -> Result<bool, String> {
        if !self.claim_idle_close(generation, None)? {
            return Ok(false);
        }
        self.close()?;
        Ok(true)
    }

    /// As [`Self::try_close_if_idle`], with `detached` deciding what the
    /// provider leaves running, and refused while it reports live background
    /// tasks. The actor decides in the step that grants the close, so a
    /// refused, stale or unacknowledged claim decides nothing, and teardown,
    /// which may begin at once, finds the decision made.
    pub(crate) fn try_stop_if_idle(
        &self,
        generation: u64,
        detached: DetachedWork,
    ) -> Result<bool, String> {
        if !self.claim_idle_close(generation, Some(detached))? {
            return Ok(false);
        }
        self.close()?;
        Ok(true)
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
        if !self.claim_idle_close(generation, None)? {
            return Err("Provider state changed before conditional idle close".into());
        }
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

    pub(crate) fn interrupt_owned(
        &self,
        run: Option<crate::model::RunId>,
        including_subagents: bool,
    ) -> Result<(), String> {
        self.ownership_command(ActorCommand::Interrupt {
            run,
            including_subagents,
        })
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
                ActorCommand::UpdateSource(Box::new(SourceUpdate {
                    source,
                    execution_console,
                    paired_holder,
                })),
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
            .map_err(String::from)
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
    pub(crate) fn file_query(
        &self,
        command: &Command,
    ) -> Result<Option<super::FileChangeReply>, String> {
        let asked = Instant::now();
        let store = self.0.store.lock().unwrap_or_else(|p| p.into_inner());
        // The store is shared with persistence; a query that waits long for
        // it can miss its caller's deadline.
        let waited = asked.elapsed();
        if waited >= Duration::from_secs(1) {
            tracing::warn!(
                waited_ms = waited.as_millis() as u64,
                "a changed files query waited for the conversation store"
            );
        }
        store.file_query(command)
    }
    pub(crate) fn text_copy(
        &self,
        source: &super::TextCopySource,
        offset: u64,
    ) -> Result<super::TextCopyPage, String> {
        match &source.scope {
            super::TextCopyScope::Conversation => self
                .0
                .store
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .text_copy(source, offset),
            super::TextCopyScope::Subagent { id } => {
                // Keep the existing store-before-state lock order. Resident
                // authority is checked before disk IO; no database read holds
                // the state lock, and guessed child/item IDs grant no access.
                let mut store = self.0.store.lock().unwrap_or_else(|p| p.into_inner());
                let state = self.0.state.lock().unwrap_or_else(|p| p.into_inner());
                let item = state
                    .text_copy_sources
                    .children
                    .get(id)
                    .and_then(|sources| {
                        sources
                            .iter()
                            .position(|current| current.as_ref() == Some(source))
                    })
                    .and_then(|index| {
                        state
                            .subagents
                            .iter()
                            .find(|agent| agent.id == *id)
                            .and_then(|agent| agent.items.get(index))
                    })
                    .filter(|item| child_diagnostics::marked(item))
                    .map(|item| item.id.clone());
                if let Some(item) = item {
                    drop(state);
                    let result = store.diagnostics.read(id, &item, source, offset);
                    if result.is_err() && store.diagnostics.is_empty() {
                        let mut state = self.0.state.lock().unwrap_or_else(|p| p.into_inner());
                        store.diagnostics.publish(&mut state);
                        state.generation = state.generation.saturating_add(1);
                    }
                    result
                } else {
                    child_text_copy::read(&state, source, offset)
                }
            }
        }
    }
    /// Resident provider usage only: never opens the transcript store.
    pub(crate) fn usage(&self) -> super::Usage {
        self.0
            .state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .usage
            .clone()
    }
    /// The provider-confirmed permission mode, without publishing a snapshot.
    pub(crate) fn permission_mode(&self) -> Option<String> {
        self.0
            .state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .permission_mode
            .clone()
    }
    /// Bounded resident metadata only; never reads or hydrates transcript pages.
    #[cfg(test)]
    pub(crate) fn notification_content(
        &self,
        kind: crate::notifications::Kind,
    ) -> crate::notifications::Content {
        self.0
            .state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .notification_content(kind)
    }
    /// The items changed since the last call, whole, for a reader that
    /// follows the conversation without presenting it; `again` names items
    /// to serve once more. See [`store::Unread`].
    pub fn unread(&self, again: &[String]) -> Result<(store::Unread, bool), String> {
        let mut store = self.0.store.lock().unwrap_or_else(|p| p.into_inner());
        let unread = store.unread(again)?;
        let busy = self.0.state.lock().unwrap_or_else(|p| p.into_inner()).busy;
        Ok((unread, busy))
    }
    pub fn snapshot(&self, after: Option<u64>) -> Option<Snapshot> {
        let mut store = self.0.store.lock().unwrap_or_else(|p| p.into_inner());
        let mut state = self.0.state.lock().unwrap_or_else(|p| p.into_inner());
        if after.is_some_and(|generation| generation >= state.generation) && !store.dirty {
            return None;
        }
        if let Err(error) = store.publish_files(&mut state) {
            state.error = Some(format!("Cannot read reported edits: {error}"));
        }
        let refresh_publication =
            store.presented_generation != Some(state.generation) || store.dirty;
        let mut fill = false;
        if store.dirty {
            match store.window_reusing(&mut state.items) {
                Ok(window) => {
                    state.items = window.items;
                    state.has_earlier = window.has_earlier;
                    state.has_later = window.has_later;
                    fill = store.wants_fill();
                }
                Err(error) => {
                    state.error = Some(format!("Cannot page owned transcript cache: {error}"))
                }
            }
        }
        if refresh_publication {
            store.publish_copy_sources(&mut state);
            store.presented_generation = Some(state.generation);
            store.presented_file_changes = Some((
                state.file_changes.revision,
                state.file_changes.coverage.clone(),
            ));
            store.presentation = None;
            store.presented_children = state
                .subagents
                .iter()
                .map(|agent| (agent.id.clone(), agent.items.clone()))
                .collect();
        }
        let mut snapshot = state.clone();
        snapshot.sampled_at = observed_at();
        drop(state);
        drop(store);
        if fill && self.0.visible.load(Ordering::Acquire) {
            self.wake();
        }
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
        let mut fill = false;
        if store.presented_generation != Some(state.generation) || store.dirty {
            if let Err(error) = store.publish_files(&mut state) {
                state.error = Some(format!("Cannot read reported edits: {error}"));
            }
            let baseline = store.presented_generation;
            let changes = if store.dirty {
                match store.window_reusing(&mut state.items) {
                    Ok(window) => {
                        state.items = window.items;
                        state.has_earlier = window.has_earlier;
                        state.has_later = window.has_later;
                        fill = store.wants_fill();
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
            store.publish_copy_sources(&mut state);
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
                    let file_changes_unchanged = store.presented_file_changes.as_ref().is_some_and(
                        |(revision, coverage)| {
                            *revision == state.file_changes.revision
                                && coverage == &state.file_changes.coverage
                        },
                    );
                    let retained_files = if file_changes_unchanged {
                        Some(std::mem::take(&mut state.file_changes))
                    } else {
                        None
                    };
                    let metadata = state.clone();
                    if let Some(files) = retained_files {
                        state.file_changes = files;
                    }
                    state.items = items;
                    for (agent, items) in state.subagents.iter_mut().zip(child_items) {
                        agent.items = items;
                    }
                    crate::chat::presentation::Presentation {
                        file_changes_unchanged,
                        base_generation,
                        metadata,
                        order: state.items.iter().map(|item| item.id.clone()).collect(),
                        changes,
                        children,
                    }
                });
            store.presented_generation = Some(state.generation);
            store.presented_file_changes = Some((
                state.file_changes.revision,
                state.file_changes.coverage.clone(),
            ));
            store.presented_children = state
                .subagents
                .iter()
                .map(|agent| (agent.id.clone(), agent.items.clone()))
                .collect();
        }
        let response = if let Some(presentation) = store
            .presentation
            .as_ref()
            .filter(|update| after == Some(update.base_generation))
        {
            let mut update = presentation.clone();
            update.metadata.sampled_at = observed_at();
            crate::terminal_ipc::ServerResponse::ChatUpdate { chat_id, update }
        } else {
            let mut snapshot = state.clone();
            snapshot.sampled_at = observed_at();
            crate::terminal_ipc::ServerResponse::ChatSnapshot {
                chat_id,
                snapshot: Some(snapshot),
            }
        };
        drop(state);
        drop(store);
        if fill && self.0.visible.load(Ordering::Acquire) {
            self.wake();
        }
        Some(response)
    }
    pub fn session_id(&self) -> Option<String> {
        self.0
            .state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .session_id
            .clone()
    }
    /// Whether the provider answered its initialize handshake.
    pub(crate) fn is_ready(&self) -> bool {
        self.0.state.lock().unwrap_or_else(|p| p.into_inner()).ready
    }
    /// How long the server this conversation launched has so far waited
    /// behind another process's file lock instead of starting, such as Codex
    /// queueing app-server startups that share one `CODEX_HOME`. Callers that
    /// bound the whole startup extend their deadline by it.
    pub(crate) fn startup_lock_wait(&self) -> Duration {
        Duration::from_millis(self.0.startup_lock_wait.load(Ordering::Acquire))
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
    /// When the latest turn and its delegated work finished, in daemon Unix
    /// milliseconds; absent while a turn runs or before the first one ends.
    /// Like [`Self::failure`], this reads no transcript.
    pub fn finished_at(&self) -> Option<u64> {
        self.0
            .state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .finished_at
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
    pub(crate) fn history_is_covered_by(&self, reloaded: &Store) -> Result<bool, String> {
        self.0
            .store
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .is_covered_by(reloaded)
    }

    pub(crate) fn is_read_only_history(&self) -> bool {
        self.0.history_only
    }

    pub fn owns_work(&self) -> bool {
        !self.0.history_only
            && (!self.is_completed()
                || self
                    .0
                    .failure
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .is_some())
    }
    /// What the conversation is doing, read without loading its transcript.
    pub(crate) fn activity(&self) -> crate::agent::ActivityState {
        self.0
            .state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .activity()
    }

    /// Whether the conversation's own turn is under way or awaiting the user,
    /// apart from its background children: the states in which it takes no
    /// prompt.
    pub(crate) fn turn_accepted(&self) -> bool {
        self.0
            .state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .turn_accepted()
    }

    /// Update preflights need activity without loading the transcript window.
    pub(crate) fn update_activity(&self) -> (bool, bool) {
        let state = self.0.state.lock().unwrap_or_else(|p| p.into_inner());
        (state.holds_work(), state.ready)
    }
    /// Whether the conversation would take a prompt now without interrupting
    /// a turn or passing over a question it put to the user.
    pub(crate) fn is_idle(&self) -> bool {
        let state = self.0.state.lock().unwrap_or_else(|p| p.into_inner());
        accepts_prompt(&state) && !self.is_completed() && !self.0.draining.load(Ordering::Acquire)
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
    pub(crate) fn work_hold(&self) -> super::WorkHold {
        self.0
            .state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .work_hold()
    }
    /// Provider-confirmed model status without hydrating the transcript.
    pub(crate) fn active_model(&self) -> (u64, Option<super::ActiveModel>) {
        let state = self.0.state.lock().unwrap_or_else(|p| p.into_inner());
        (state.active_model_revision, state.active_model.clone())
    }
    /// What the runtime observed of the conversation's prompt cache.
    pub(crate) fn prompt_cache(&self) -> crate::chat::prompt_cache::Activity {
        self.0.prompt_cache.activity()
    }
    /// The conversation's context as of its latest usage reading.
    pub(crate) fn context_tokens(&self) -> u64 {
        self.0
            .state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .usage
            .context_tokens
    }
    /// How the provider addresses this conversation's subagents, where that
    /// is not by the IDs its snapshot lists them under.
    pub(crate) fn agent_addresses(&self) -> Vec<AgentAddress> {
        self.0.agent_addresses.list()
    }
    pub(crate) fn background_subagents(&self) -> Vec<String> {
        let state = self.0.state.lock().unwrap_or_else(|p| p.into_inner());
        state
            .subagents
            .iter()
            .filter(|agent| agent.background && agent.status.is_live())
            .map(|agent| agent.id.clone())
            .collect()
    }

    /// See [`super::Snapshot::background_tasks`].
    pub(crate) fn background_tasks(&self) -> usize {
        self.0
            .state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .background_tasks
    }

    pub(crate) fn interrupt_subagents(&self) -> Vec<String> {
        let state = self.0.state.lock().unwrap_or_else(|p| p.into_inner());
        state
            .subagents
            .iter()
            .filter(|agent| agent.stopped_by_interrupt(&state.capabilities))
            .map(|agent| agent.id.clone())
            .collect()
    }
    /// A coherent resident naming projection. Never opens the transcript Store,
    /// pages history or touches the filesystem; safe at host admission points.
    #[cfg(test)]
    pub(crate) fn conversation_title(
        &self,
    ) -> Result<(Option<String>, Option<String>), TitleUnavailable> {
        self.conversation_naming()
            .map(|naming| (naming.session, naming.provider))
    }

    pub(crate) fn conversation_naming(&self) -> Result<ConversationNaming, TitleUnavailable> {
        let state = self.0.state.try_lock().map_err(|error| match error {
            std::sync::TryLockError::WouldBlock => TitleUnavailable::Busy,
            std::sync::TryLockError::Poisoned(_) => TitleUnavailable::Poisoned,
        })?;
        Ok(ConversationNaming {
            session: state.session_id.clone(),
            provider: state.title.clone(),
            provider_cleared: state.provider_title_cleared,
            opening: state.opening_title.clone(),
        })
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

    /// Decides, before the close, whether it leaves running the work the
    /// provider had already detached from itself. See [`DetachedWork`].
    pub(crate) fn decide_detached_work(&self, detached: DetachedWork) {
        self.0.detached_work.decide(detached);
    }

    /// Whether teardown found the provider killed by something outside
    /// cagents. Known once the provider's teardown has run.
    pub(crate) fn provider_was_force_stopped(&self) -> bool {
        self.0.detached_work.was_force_stopped()
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
                holder.close_inner(
                    self.0.retain_cleanup_receipt.load(Ordering::Acquire),
                    self.0.detached_work.holder(),
                )?;
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
                owner.close_with(
                    self.0.retain_cleanup_receipt.load(Ordering::Acquire),
                    self.0.detached_work.holder(),
                )?;
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
    preflight_for_start(spec, initial).map_err(|error| error.to_string())
}

pub(crate) fn preflight_for_start(
    spec: &ProcessSpec,
    initial: &mut Step,
) -> Result<Store, SetupError> {
    preflight_inner(spec, initial, false)
}

/// Only an authenticated retained server can restore its read-only connection
/// after the original directory was removed. Authorizing commands still check
/// the retained execution policy in the actor before any provider write.
pub(crate) fn preflight_retained_for_start(
    spec: &ProcessSpec,
    initial: &mut Step,
) -> Result<Store, SetupError> {
    if !matches!(spec.transport, TransportSpec::UnixWebSocket { .. }) {
        return Err("Retained preflight cannot launch a provider".into());
    }
    preflight_inner(spec, initial, true)
}

fn preflight_inner(
    spec: &ProcessSpec,
    initial: &mut Step,
    retained: bool,
) -> Result<Store, SetupError> {
    use std::os::unix::ffi::OsStrExt;
    if !retained
        && !matches!(spec.transport, TransportSpec::History)
        && !spec.working_directory.is_dir()
    {
        return Err(SetupError::Unavailable(
            "Provider working directory does not exist".into(),
        ));
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
            return Err(SetupError::Unavailable(format!(
                "Provider executable {} is not executable in the preserved launch environment",
                spec.program
            )));
        }
    }
    let mut queued = 0usize;
    for frame in &initial.writes {
        let mut count = ByteCounter(0);
        serde_json::to_writer(&mut count, frame)
            .map_err(|error| SetupError::Invalid(error.to_string()))?;
        if count.0 > FRAME_LIMIT {
            return Err("Provider initialization frame exceeds 32 MiB".into());
        }
        queued = queued.saturating_add(count.0 + 1);
    }
    if queued > QUEUE_LIMIT {
        return Err("Provider initialization exceeds stdin queue capacity".into());
    }
    let mut store = Store::new().map_err(SetupError::Unavailable)?;
    if spec.followed {
        store.follow();
    }
    store.file_cwd = Some(spec.working_directory.clone());
    store.recognize_messages(spec.messages.clone());
    let mut metadata = Vec::new();
    for event in initial.events.drain(..) {
        if let Event::Error {
            message,
            fatal: true,
        } = &event
        {
            return Err(SetupError::Invalid(message.clone()));
        }
        metadata.extend(store.apply_for_start(event)?);
    }
    let mut count = ByteCounter(0);
    serde_json::to_writer(&mut count, &metadata)
        .map_err(|error| SetupError::Invalid(error.to_string()))?;
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
            // The worker's own final notification can release the last
            // handle, for example when the host retires its record. That
            // thread is already finishing, and joining itself panics.
            && worker.thread().id() != thread::current().id()
        {
            let _ = worker.join();
        }
    }
}
impl super::protocol::LaunchCleanup for LocalHandle {
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

/// `wake` is written when the provider's scope reports a command stopped for
/// memory, so the actor can say so.
fn spawn_provider(
    spec: &ProcessSpec,
    stop: &AtomicBool,
    wake: Option<UnixStream>,
    private_temp: bool,
) -> Result<Option<ChildGuard>, String> {
    if matches!(
        spec.transport,
        TransportSpec::History | TransportSpec::UnixWebSocket { .. }
    ) {
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
        // Callers withhold these already; no path may hand a provider the
        // session its daemon or a saved recipe came from.
        .envs(
            spec.environment
                .iter()
                .filter(|(name, _)| !crate::agent::is_session_variable(name)),
        )
        .stdin(if pipes { Stdio::piped() } else { Stdio::null() })
        .stdout(if pipes { Stdio::piped() } else { Stdio::null() })
        .stderr(if pipes { Stdio::piped() } else { Stdio::null() })
        .process_group(0);
    crate::host::descriptor_limit::inherit(&mut process);
    let original = spec
        .execution
        .as_ref()
        .map(|context| {
            let descriptor = context.directory_descriptor();
            if descriptor.is_none() && context.directory_identity()?.is_some() {
                return Err(
                    "Admitted provider context has no original directory descriptor".to_owned(),
                );
            }
            Ok(descriptor)
        })
        .transpose()?
        .flatten();
    let directory = match original {
        Some(directory) => directory,
        None => {
            use std::os::unix::fs::OpenOptionsExt;
            Arc::new(
                std::fs::OpenOptions::new()
                    .read(true)
                    .custom_flags(libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC)
                    .open(&spec.working_directory)
                    .map_err(|error| error.to_string())?,
            )
        }
    };
    {
        let directory = directory.clone();
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
    let mut temp = if private_temp {
        use crate::daemon::codex_temp::{PrivateDir, TEMP_ENV_KEYS};

        let inherited = spec.environment.get("TMPDIR").map(std::ffi::OsStr::new);
        let cwd = std::path::PathBuf::from(format!("/proc/self/fd/{}", directory.as_raw_fd()));
        let temporary = PrivateDir::create(inherited, Some(&cwd))
            .map_err(|error| format!("Cannot create private Codex temporary directory: {error}"))?;
        process.envs(TEMP_ENV_KEYS.map(|key| (key, temporary.path())));
        Some(temporary)
    } else {
        None
    };
    // A retained actor sweeps its unregistered exited children; this
    // registration keeps the provider's status for its ChildGuard.
    let (child, registration) = crate::daemon::adopted_orphans::spawn_owned(&mut process)
        .map_err(|e| format!("Cannot start {}: {e}", spec.program))?;
    if let Some(temporary) = &mut temp {
        temporary.launched();
    }
    let mut child = ChildGuard::new(child, registration)
        .map_err(|e| format!("Cannot retain {} process identity: {e}", spec.program))?;
    child.directory = Some(directory);
    child._temp = temp;
    // Scope enrollment happens before this child is handed to the actor and
    // therefore before any prompt/tool request. Keep the original Child, pipes,
    // process group and account lease: systemd only relocates its cgroup.
    match crate::daemon::process_scope::enroll_provider(child.child.id(), wake) {
        Ok(scope) => child.scope = scope,
        Err(error) => {
            tracing::warn!(%error, provider = %spec.program, "provider scope isolation unavailable; using existing launch fallback");
        }
    }
    crate::daemon::process_scope::warn_if_shared(child.child.id(), child.child.id());
    crate::daemon::process_scope::warn_if_bootstrap_shared(child.child.id());
    Ok(Some(child))
}

/// Sole reaper of the provider leader. Observations retain its zombie until
/// every group signal is finished, so its numeric PID/PGID cannot be recycled.
/// An owner's decision about the work its provider detached. Until it
/// decides, each teardown keeps what it did before decisions existed: a
/// direct provider's leaves that work running unless the provider was
/// force-stopped, and a native holder stops it with the provider.
#[derive(Default)]
struct DetachedDecision(std::sync::atomic::AtomicU8, AtomicBool);
impl DetachedDecision {
    /// Something outside cagents killed the provider, such as the system
    /// for memory: what it left would hold that memory, so nothing is spared.
    fn force_stopped(&self) {
        self.1.store(true, Ordering::Release);
    }
    fn was_force_stopped(&self) -> bool {
        self.1.load(Ordering::Acquire)
    }
    const STOP: u8 = 1;
    const SPARE: u8 = 2;
    fn decide(&self, detached: DetachedWork) {
        self.0.store(
            match detached {
                DetachedWork::Stop => Self::STOP,
                DetachedWork::Spare => Self::SPARE,
            },
            Ordering::Release,
        );
    }
    fn get(&self) -> Option<DetachedWork> {
        match self.0.load(Ordering::Acquire) {
            Self::STOP => Some(DetachedWork::Stop),
            Self::SPARE => Some(DetachedWork::Spare),
            _ => None,
        }
    }
    fn holder(&self) -> DetachedWork {
        self.get().unwrap_or(DetachedWork::Stop)
    }
}
struct ChildGuard {
    child: Child,
    identity: OwnedFd,
    terminated: bool,
    directory: Option<Arc<std::fs::File>>,
    /// The provider's cgroup scope, when the OS user manager provided one.
    scope: Option<crate::daemon::process_scope::ProviderScope>,
    /// Whether the provider had been force-stopped, settled by the first
    /// teardown: a retry must not take cagents' own `SIGKILL` for one.
    force_stopped: Option<bool>,
    /// Whether teardown also ends the commands the provider detached, as
    /// when its owner decided to stop them.
    stop_detached: bool,
    // Dropped after `Drop::drop` has run the final reap in `terminate`.
    _registration: crate::daemon::adopted_orphans::Registration,
    _temp: Option<crate::daemon::codex_temp::PrivateDir>,
}
impl ChildGuard {
    /// Wait on the original child capability without reaping it or releasing
    /// the process-group identity needed by teardown.
    fn observe_exit_within(
        &self,
        grace: Duration,
    ) -> std::io::Result<Option<std::process::ExitStatus>> {
        let deadline = Instant::now() + grace;
        loop {
            if let Some(status) = self.observed_exit_status()? {
                return Ok(Some(status));
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Ok(None);
            }
            let mut descriptor = libc::pollfd {
                fd: self.identity.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            let timeout = remaining
                .as_nanos()
                .div_ceil(1_000_000)
                .min(i32::MAX as u128) as i32;
            // SAFETY: descriptor points to one initialized pollfd and the
            // owned pidfd stays open throughout this non-reaping observation.
            if unsafe { libc::poll(&mut descriptor, 1, timeout) } < 0 {
                let error = std::io::Error::last_os_error();
                if error.kind() != std::io::ErrorKind::Interrupted {
                    return Err(error);
                }
            } else if descriptor.revents & (libc::POLLNVAL | libc::POLLERR) != 0 {
                return Err(std::io::Error::other(
                    "Provider exit capability became unavailable",
                ));
            }
        }
    }

    /// Capture immediately after spawn, before any code can reap this child.
    fn new(
        mut child: Child,
        registration: crate::daemon::adopted_orphans::Registration,
    ) -> std::io::Result<Self> {
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
            force_stopped: None,
            stop_detached: false,
            directory: None,
            scope: None,
            _registration: registration,
            _temp: None,
        })
    }

    /// Test fixtures spawn through the same ownership registration.
    #[cfg(test)]
    fn spawn(command: &mut Process) -> Self {
        let (child, registration) = crate::daemon::adopted_orphans::spawn_owned(command).unwrap();
        Self::new(child, registration).unwrap()
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
        let ended = self.observed_exit_status()?;
        // Something outside cagents force-stopped the provider, such as the
        // system for memory: a closed or finished chat is not, whether or
        // not it ignores the signals below.
        let force_stopped = *self.force_stopped.get_or_insert_with(|| {
            ended.is_some_and(|status| {
                std::os::unix::process::ExitStatusExt::signal(&status) == Some(libc::SIGKILL)
            })
        });
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
        // Commands in process groups of their own outlive that signal. A
        // force-stopped provider's would hold memory the system needed, with
        // no agent left to use them, so they end with it, as do those of an
        // owner that decided to stop them. Otherwise they outlive the chat,
        // as a process the agent detached asked to.
        if (force_stopped || self.stop_detached)
            && let Some(scope) = self.scope.as_mut()
            && let Err(error) = scope.end()
        {
            tracing::warn!(%error, "commands the provider left could not be ended");
        }
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

/// One serialized frame waiting for the provider's stdin.
struct QueuedWrite {
    bytes: Vec<u8>,
    /// See [`Adapter::expendable_write`].
    expendable: bool,
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
    launch_account_scope: Option<crate::account::scope::LaunchAccountScope>,
    launch_context: Option<Arc<super::protocol::ExecutionContext>>,
    working_directory: std::path::PathBuf,
    child: Option<ChildGuard>,
    native_holder: Option<super::native_holder::Holder>,
    startup_lock_wait: Arc<AtomicU64>,
    /// Whether this conversation's lack of durable continuity was logged.
    reconnect_only_reported: bool,
    /// Commands memory exhaustion stopped while this actor ran.
    command_stops: usize,
    expected_native_peer: Option<crate::daemon::holder_registry::Process>,
    native_peer: Option<crate::daemon::holder_registry::Process>,
    preserve_native: Arc<AtomicBool>,
    native_installed: Arc<AtomicBool>,
    adapter: Box<dyn Adapter>,
    state: Arc<Mutex<Snapshot>>,
    confirmed_session: Arc<Mutex<Option<String>>>,
    agent_addresses: AgentAddresses,
    prompt_cache: PromptCache,
    store: Arc<Mutex<Store>>,
    visible: Arc<AtomicBool>,
    observe_next: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    /// What teardown leaves running; see [`ActorCommand::CloseIfIdle`].
    detached_work: Arc<DetachedDecision>,
    incoming: crossbeam_channel::Receiver<CommandRequest>,
    wake: UnixStream,
    notify: Arc<dyn Fn(Update) + Send + Sync>,
    /// Retained helpers observe semantic metadata independently of visible
    /// presentation and the once-per-unread-epoch output observation.
    resident_changed: Option<Arc<dyn Fn() + Send + Sync>>,
    resident_dirty: bool,
    output: Vec<u8>,
    /// Prefix already checked for a delimiter in the incomplete stdio frame.
    scanned: usize,
    pending: VecDeque<QueuedWrite>,
    offset: usize,
    queued: usize,
    stderr: VecDeque<u8>,
    indices: BTreeMap<String, usize>,
    retained: usize,
    /// Only the active error published by the most recent history failure.
    /// Recovery must not erase an unrelated provider/cache error.
    history_error: Option<String>,
    login_recovery: Option<String>,
    dirty: bool,
    activity: ActivityState,
    /// Whether the provider was taking prompts when activity was last published.
    ready: bool,
    prompt_available: bool,
    attention: TurnAttention,
    execution: execution::Tracker,
    attention_baselined: bool,
    waiting_fingerprint: Option<String>,
    last_present: Instant,
    first_frame: bool,
    stdout_noise: Option<fn(&[u8]) -> bool>,
    started: Instant,
    startup: StartupBudget,
    write_started: Option<Instant>,
    #[cfg(test)]
    idle_frame_buffer_capacity: Arc<AtomicUsize>,
}

/// How long an unfinished initialization may go without a valid frame, and
/// how long frames can keep extending it.
#[derive(Clone, Copy, Debug)]
pub(super) struct StartupBudget {
    pub(super) renewal: Duration,
    pub(super) limit: Duration,
}

impl StartupBudget {
    pub(super) const PRODUCTION: Self = Self {
        renewal: IO_TIMEOUT,
        limit: STARTUP_LIMIT,
    };

    /// Each valid frame earns a fresh `renewal`, up to `limit` overall.
    fn expired(self, started: Instant, last_progress: Instant, now: Instant) -> bool {
        now.duration_since(last_progress) > self.renewal || now.duration_since(started) > self.limit
    }

    #[cfg(not(test))]
    fn current() -> Self {
        Self::PRODUCTION
    }

    /// The budget a test scoped to its own thread, so real-pipe deadline
    /// tests need not wait out the production deadlines.
    #[cfg(test)]
    fn current() -> Self {
        TEST_STARTUP_BUDGET.get()
    }

    /// Apply this budget to protocol actors started on this thread until
    /// the guard drops.
    #[cfg(test)]
    #[must_use = "the budget applies only while the guard lives"]
    pub(super) fn scope(self) -> impl Drop {
        struct Restore(StartupBudget);
        impl Drop for Restore {
            fn drop(&mut self) {
                TEST_STARTUP_BUDGET.set(self.0);
            }
        }
        Restore(TEST_STARTUP_BUDGET.replace(self))
    }
}

#[cfg(test)]
thread_local! {
    static TEST_STARTUP_BUDGET: std::cell::Cell<StartupBudget> =
        const { std::cell::Cell::new(StartupBudget::PRODUCTION) };
}

/// A provider that exited without being asked to, naming memory exhaustion
/// when the provider's cgroup shows the system ended it for that.
fn exit_report(child: &ChildGuard, status: std::process::ExitStatus) -> String {
    use crate::daemon::process_scope::Killer;
    use std::os::unix::process::ExitStatusExt;
    let exited = format!("{}{status}", super::failure::PROVIDER_EXITED);
    let Some(scope) = child.scope.as_ref() else {
        return exited;
    };
    let killer = (status.signal() == Some(libc::SIGKILL))
        .then(|| scope.provider_killer())
        .flatten();
    let out_of_memory = super::failure::PROVIDER_OUT_OF_MEMORY;
    match killer {
        // A shared cgroup is killed whole.
        Some(Killer::Oomd) if scope.keeps_commands_apart() => {
            format!("{out_of_memory} systemd-oomd ended it. {exited}")
        }
        Some(Killer::Oomd) => format!(
            "{out_of_memory} systemd-oomd ended it and the commands it was running. {exited}"
        ),
        Some(Killer::Kernel) => {
            format!("{out_of_memory} The kernel's out-of-memory killer ended it. {exited}")
        }
        None => exited,
    }
}

/// One notice for the commands memory exhaustion stopped since the last
/// check. `earlier` were stopped before them since the agent started: a retried
/// command stopped again still gets a notice of its own.
fn command_stop_report(
    stops: &[crate::daemon::process_scope::CommandStop],
    earlier: usize,
) -> String {
    use crate::daemon::process_scope::Killer;
    let mut killers = Vec::new();
    for stop in stops {
        let by = match stop.killer {
            Killer::Oomd => "systemd-oomd",
            Killer::Kernel => "the kernel's out-of-memory killer",
        };
        if !killers.contains(&by) {
            killers.push(by);
        }
    }
    // It opens a sentence; systemd-oomd keeps its own lower case.
    let by = killers.join(" and ");
    let by = match by.strip_prefix("the ") {
        Some(rest) => format!("The {rest}"),
        None => by,
    };
    let what = match stops.len() {
        1 => "it".to_owned(),
        count => format!("{count} commands"),
    };
    let processes = match stops.iter().map(|stop| stop.processes).sum::<Option<u64>>() {
        Some(1) => " (1 process)".to_owned(),
        Some(count) => format!(" ({count} processes)"),
        None => String::new(),
    };
    let mut report = format!(
        "{} {by} ended {what}{processes}; the agent is still running.",
        super::failure::COMMAND_OUT_OF_MEMORY
    );
    if earlier > 0 {
        report.push_str(&format!(
            " Commands stopped for memory since the agent started: {}.",
            earlier + stops.len()
        ));
    }
    report
}

/// Append the most recent skipped stdout line, which often names the real cause.
fn with_ignored_output(message: &str, ignored: &Option<String>) -> String {
    match ignored {
        Some(preview) => format!("{message}. Last ignored output: {preview}"),
        None => message.to_owned(),
    }
}

struct TurnAttention {
    key: String,
    settled: bool,
    started: bool,
}

impl Default for TurnAttention {
    fn default() -> Self {
        Self {
            key: uuid::Uuid::new_v4().to_string(),
            settled: false,
            started: false,
        }
    }
}

impl TurnAttention {
    fn apply(&mut self, event: &Event) -> Option<crate::notifications::Evidence> {
        match event {
            Event::TurnStarted => {
                *self = Self::default();
                self.started = true;
            }
            Event::Reset { .. } => *self = Self::default(),
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

    fn finish(&self, state: &mut Snapshot, at: Option<u64>) -> bool {
        let activity = state.activity();
        if activity == ActivityState::Working || activity.waits_on_user() {
            // Providers can announce a background task in a later frame than
            // the parent's settlement. Its eventual finish must replace the
            // earlier idle time, rather than merely reveal it again.
            return state.finished_at.take().is_some();
        }
        if self.started
            && self.settled
            && state.activity() == ActivityState::Idle
            && state.finished_at.is_none()
        {
            state.finished_at = at;
            return at.is_some();
        }
        false
    }
}
/// Why an interrupt that would also stop background subagents was refused.
fn interrupt_refusal(affected: &super::Held<'_>) -> String {
    format!(
        "Interrupting would also stop {}. {} Use --including-subagents to permit this, or stop {} in the app first.",
        affected.count(),
        affected.detail(),
        affected.where_to_stop()
    )
}

/// Why a close that requires an idle conversation left it open. Subagents
/// that hold a conversation whose turn has ended leave it looking finished,
/// so they are named along with how to let them go.
fn idle_close_refusal(readiness: UpdateReadiness, state: &Snapshot) -> String {
    let hold = state.work_hold();
    match readiness {
        UpdateReadiness::Busy if !hold.turn && !hold.subagents.is_empty() => {
            let held = hold.subagents();
            format!(
                "{} Stop {} in the conversation to let it close.",
                held.detail(),
                held.where_to_stop()
            )
        }
        UpdateReadiness::Busy => {
            "The conversation is busy or waiting for an answer, so it was left open.".into()
        }
        UpdateReadiness::Unready | UpdateReadiness::Idle => {
            "The conversation's provider still has work in flight, so it was left open. Try again in a moment.".into()
        }
    }
}

/// Parent admission deliberately excludes aggregate background-child activity.
fn accepts_prompt(state: &Snapshot) -> bool {
    state.ready && !state.busy && !state.waiting && !state.exited && state.requests.is_empty()
}

impl Actor {
    fn metadata_poll(&self, timeout: i32) -> (libc::pollfd, i32) {
        let mut watch = libc::pollfd {
            fd: -1,
            events: libc::POLLIN,
            revents: 0,
        };
        match self.adapter.metadata_wake() {
            Some(MetadataWake::Changed(fd)) => watch.fd = fd.as_raw_fd(),
            Some(MetadataWake::RetryAt(at)) => {
                let delay = at.saturating_duration_since(Instant::now());
                let millis = delay.as_nanos().div_ceil(1_000_000);
                let retry = i32::try_from(millis).unwrap_or(i32::MAX);
                return (
                    watch,
                    if timeout < 0 {
                        retry
                    } else {
                        timeout.min(retry)
                    },
                );
            }
            None => {}
        }
        (watch, timeout)
    }

    fn refresh_metadata(&mut self, changed: bool) -> Result<(), String> {
        if (changed
            || matches!(self.adapter.metadata_wake(), Some(MetadataWake::RetryAt(at)) if at <= Instant::now()))
            && let Some(step) = self.adapter.refresh_metadata()
        {
            self.step(step)?;
        }
        Ok(())
    }

    /// A history reader owns no process, network connection, or execution
    /// authority. It keeps the ordinary bounded store and paging contract.
    fn run_history(&mut self, initial: Step) -> Result<(), String> {
        if !initial.writes.is_empty() {
            return Err("History initialization attempted a provider write".into());
        }
        self.step(initial)?;
        while !self.stop.load(Ordering::Acquire) {
            self.drain_commands(false)?;
            if !self.pending.is_empty() {
                return Err("History reader attempted a provider write".into());
            }
            if self.dirty && self.visible.load(Ordering::Acquire) {
                self.dirty = false;
                (self.notify)(Update::Presentation);
            }
            self.publish_resident_metadata();
            if !self.incoming.is_empty() {
                continue;
            }
            let (watch, timeout) = self.metadata_poll(-1);
            let mut fds = [
                libc::pollfd {
                    fd: self.wake.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
                watch,
            ];
            // SAFETY: poll receives initialized entries held by this actor.
            if unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as _, timeout) } < 0 {
                let error = std::io::Error::last_os_error();
                if error.kind() != std::io::ErrorKind::Interrupted {
                    return Err(error.to_string());
                }
            }
            let mut buffer = [0; 64];
            while self.wake.read(&mut buffer).is_ok_and(|n| n != 0) {}
            self.refresh_metadata(fds[1].revents != 0)?;
        }
        Ok(())
    }
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
        // Every read and write of the provider's pipes below pokes the scope.
        if let Some(scope) = self.child.as_ref().and_then(|c| c.scope.as_ref()) {
            scope.reports_activity();
        }
        nonblocking(&stdin)?;
        nonblocking(&stdout)?;
        nonblocking(&stderr)?;
        self.step(initial)?;
        let mut stdout_eof = false;
        let mut stderr_eof = false;
        // A provider that exits closes its stdin before its exit is observable.
        // Keep the write error only as a fallback: the exit explains the failure.
        let mut stdin_closed: Option<String> = None;
        let mut closing = None;
        let mut partial_started = None;
        let mut interrupt_sent = false;
        let mut interrupt_held = false;
        let mut interrupt_error = String::new();
        let mut garbage_lines = 0usize;
        let mut garbage_total = 0u64;
        let mut last_ignored = None;
        let mut startup_progress = self.started;
        loop {
            if self.stop.load(Ordering::Acquire) {
                if closing.is_none() {
                    closing = Some(Instant::now());
                }
                let busy = self.state.lock().unwrap_or_else(|p| p.into_inner()).busy;
                if busy && !interrupt_sent && self.pending.is_empty() {
                    match self.adapter.command(Command::Interrupt) {
                        Ok(step) => {
                            // A provider that names a turn only once it has
                            // started holds the stop until then, and sends it
                            // as it drains.
                            interrupt_held = self.adapter.held_stop() == HeldStop::Waiting;
                            self.step(step)?;
                            interrupt_sent = true;
                        }
                        // A just-submitted turn may not have its provider ID yet.
                        // Continue draining until it can be interrupted or the deadline expires.
                        Err(error) => interrupt_error = error,
                    }
                }
                // A result that arrives before the interrupt's acknowledgement
                // settles the turn, but only that acknowledgement says whether
                // it was cancelled. Returning before it drops the outcome.
                let settled = self.pending.is_empty()
                    && !self.state.lock().unwrap_or_else(|p| p.into_inner()).busy;
                if settled && !self.adapter.outcome_pending() {
                    return Ok(());
                }
                if closing.is_some_and(|at: Instant| at.elapsed() > super::INTERRUPT_TIMEOUT) {
                    if settled {
                        return Err("The provider settled its turn but did not acknowledge the interruption within 30 seconds, so whether the turn was cancelled is unconfirmed".into());
                    }
                    let reason = match (interrupt_held, self.adapter.held_stop()) {
                        (true, HeldStop::Waiting) => {
                            " (the provider had not yet started the turn to stop)".to_owned()
                        }
                        (true, HeldStop::Sent) => {
                            " (the stop was sent once the turn started)".to_owned()
                        }
                        (true, _) => {
                            " (no stop was sent: the provider refused or had already finished the turn)"
                                .to_owned()
                        }
                        (false, _) if interrupt_error.is_empty() => String::new(),
                        (false, _) => format!(" ({interrupt_error})"),
                    };
                    return Err(format!(
                        "Provider interruption was not confirmed within 30 seconds{reason}; backend work may still be active. Inspect exact provider session before recovery"
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
            let mut consumed = 0;
            loop {
                let unread = &self.output[consumed..];
                let Some(newline) = next_newline(unread, &mut self.scanned) else {
                    break;
                };
                if newline == 0 {
                    consumed += 1;
                    self.scanned = 0;
                    continue;
                }
                if parsed == FRAMES_PER_POLL {
                    frames_ready = true;
                    break;
                }
                let line = &unread[..newline];
                let frame = match serde_json::from_slice(line) {
                    Ok(frame) => {
                        garbage_lines = 0;
                        Some(frame)
                    }
                    Err(error) => {
                        let Some(hides_frame) = self.stdout_noise else {
                            return Err(format!(
                                "Unsupported provider stdout (expected JSON-lines): {error}"
                            ));
                        };
                        let preview = super::protocol::diagnostic_preview(line, 200);
                        if hides_frame(line) {
                            return Err(format!(
                                "The agent printed other text inside one of its conversation messages, so the conversation could not continue. This usually comes from a wrapper script or shell startup file. Output: {preview}"
                            ));
                        }
                        garbage_lines += 1;
                        garbage_total = garbage_total.saturating_add(1);
                        if garbage_total <= 3 || garbage_total.is_power_of_two() {
                            tracing::warn!(
                                consecutive = garbage_lines,
                                ignored_total = garbage_total,
                                "Skipping non-JSON provider stdout line"
                            );
                        }
                        if garbage_lines >= 8 {
                            return Err(format!(
                                "The agent printed 8 lines in a row that are not part of the conversation, so it was stopped. This usually comes from a wrapper script or shell startup file. Last output: {preview}"
                            ));
                        }
                        last_ignored = Some(preview);
                        None
                    }
                };
                consumed += newline + 1;
                self.scanned = 0;
                // Let a prompt-ready step release the allocation immediately
                // when it consumes the last frame. Clearing moves no bytes.
                if consumed == self.output.len() {
                    self.output.clear();
                    consumed = 0;
                }
                partial_started = None;
                parsed += 1;
                let Some(frame) = frame else {
                    continue;
                };
                self.first_frame = true;
                let step = self.adapter.receive(frame)?;
                self.provider_step(step)?;
                startup_progress = Instant::now();
            }
            // Move any unread suffix once per poll. `scanned` is already
            // relative to this suffix, including when it holds a partial line.
            if consumed != 0 {
                self.output.drain(..consumed);
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
                    return Err(with_ignored_output(
                        "Provider JSON frame remained incomplete or blocked for 30 seconds",
                        &last_ignored,
                    ));
                }
            }
            // A parsing budget can leave complete lines queued even after
            // the pipe closes. Keep fair poll passes until those drain. Input
            // the provider stopped reading no longer holds the actor open.
            if stdout_eof && (self.pending.is_empty() || stdin_closed.is_some()) && !frames_ready {
                if !self.output.is_empty() {
                    return Err(with_ignored_output(
                        "Provider ended with an incomplete JSON line",
                        &last_ignored,
                    ));
                }
                if let Some(status) = self
                    .child
                    .as_mut()
                    .ok_or("Provider process unavailable")?
                    .observe_exit_within(PROTOCOL_EXIT_GRACE)
                    .map_err(|error| error.to_string())?
                {
                    if status.success() {
                        if closing.is_none() && self.adapter.initialization_pending() {
                            return Err(with_ignored_output(
                                "Provider exited before completing initialization",
                                &last_ignored,
                            ));
                        }
                        // A clean exit still lost any input it never read,
                        // unless all it left unread was the adapter's own
                        // requests for display data.
                        return match stdin_closed {
                            Some(error) if self.pending.iter().any(|write| !write.expendable) => {
                                Err(error)
                            }
                            _ => Ok(()),
                        };
                    }
                    let report = exit_report(
                        self.child.as_ref().ok_or("Provider process unavailable")?,
                        status,
                    );
                    return Err(with_ignored_output(&report, &last_ignored));
                }
                return Err(with_ignored_output(
                    "Provider closed its protocol stream; process terminated to avoid an unobservable writer",
                    &last_ignored,
                ));
            }
            if (!self.first_frame || self.adapter.initialization_pending())
                && self
                    .startup
                    .expired(self.started, startup_progress, Instant::now())
            {
                return Err(with_ignored_output(
                    if self.first_frame {
                        "Provider initialization timed out before the provider reported it was ready"
                    } else {
                        "Provider initialization timed out without a JSON protocol frame"
                    },
                    &last_ignored,
                ));
            }
            if self
                .write_started
                .is_some_and(|at| at.elapsed() > IO_TIMEOUT)
            {
                return Err(
                    stdin_closed.unwrap_or_else(|| "Provider stdin stalled for 30 seconds".into())
                );
            }
            if self.dirty
                && self.visible.load(Ordering::Acquire)
                && self.last_present.elapsed() >= PRESENT_INTERVAL
            {
                self.dirty = false;
                self.last_present = Instant::now();
                (self.notify)(Update::Presentation);
            }
            self.publish_resident_metadata();
            let timeout = if frames_ready
                || (self.queued <= QUEUE_BACKPRESSURE && !self.incoming.is_empty())
            {
                0
            } else if self.dirty && self.visible.load(Ordering::Acquire) {
                50
            } else if !self.first_frame
                || self.adapter.initialization_pending()
                || !self.output.is_empty()
                || self.write_started.is_some()
                || closing.is_some()
            {
                250
            } else {
                -1
            };
            let (watch, poll_timeout) = self.metadata_poll(timeout);
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
                    fd: if self.pending.is_empty() || stdin_closed.is_some() {
                        -1
                    } else {
                        stdin.as_raw_fd()
                    },
                    events: libc::POLLOUT,
                    revents: 0,
                },
                watch,
            ];
            // The -1 branch proves the parser drained every complete frame
            // and no partial line remains. It also covers a conversation
            // parked on a permission request, which cannot become prompt-ready.
            if timeout == -1 {
                self.reclaim_idle_frame_buffer();
            }
            if unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as _, poll_timeout) } < 0 {
                let error = std::io::Error::last_os_error();
                if error.kind() != std::io::ErrorKind::Interrupted {
                    return Err(error.to_string());
                }
            }
            let mut buffer = [0u8; 8192];
            if fds[0].revents != 0 {
                while self.wake.read(&mut buffer).is_ok_and(|n| n != 0) {}
            }
            self.report_command_stops();
            if fds[3].revents != 0
                && let Some(QueuedWrite { bytes: line, .. }) = self.pending.front()
            {
                match stdin.write(&line[self.offset..]) {
                    Ok(0) => return Err("Provider closed stdin".into()),
                    Ok(n) => {
                        // Input can start a command before any output,
                        // such as a hook run on a submitted prompt.
                        self.poke_scope();
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
                    // The provider closed its input, ordinarily by exiting.
                    // Drain its output to EOF so its exit status, and any
                    // final frames, explain the failure instead of EPIPE.
                    Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => {
                        stdin_closed = Some(format!("Provider stdin: {e}"));
                    }
                    Err(e) => return Err(format!("Provider stdin: {e}")),
                }
            }
            if fds[1].revents != 0 {
                let available = buffer.len().min(FRAME_LIMIT + 1 - self.output.len());
                match stdout.read(&mut buffer[..available]) {
                    Ok(0) => stdout_eof = true,
                    Ok(n) => {
                        reserve_frame(&mut self.output, n)?;
                        self.output.extend_from_slice(&buffer[..n]);
                        // Output often announces a command: move it soon.
                        self.poke_scope();
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                    Err(e) => return Err(format!("Provider stdout: {e}")),
                }
            }
            if fds[2].revents != 0 {
                match stderr.read(&mut buffer) {
                    Ok(0) => stderr_eof = true,
                    Ok(n) => {
                        self.poke_scope();
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
            self.refresh_metadata(fds[4].revents != 0)?;
        }
    }
    /// The provider read input or wrote output, so it may be about to start
    /// a command: move it to a leaf of its own soon.
    fn poke_scope(&self) {
        if let Some(scope) = self.child.as_ref().and_then(|c| c.scope.as_ref()) {
            scope.poke();
        }
    }

    /// Say so when the system stopped one of the provider's commands
    /// because memory ran out. The provider is still running.
    fn report_command_stops(&mut self) {
        let stops = match self.child.as_ref().and_then(|child| child.scope.as_ref()) {
            Some(scope) => scope.take_command_stops(),
            None => return,
        };
        if stops.is_empty() {
            return;
        }
        let message = command_stop_report(&stops, self.command_stops);
        self.command_stops += stops.len();
        self.apply(Event::Error {
            message,
            fatal: false,
        });
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
            let (command, effects) = match command {
                ActorCommand::RetainedProvider {
                    run,
                    command,
                    decline_only,
                    effects,
                } => (
                    ActorCommand::Provider {
                        run,
                        command,
                        decline_only,
                    },
                    Some(effects),
                ),
                command => (command, None),
            };
            let effects = InputEffects(effects);
            let refuse = |error| reply.send(Err(Rejection::Refused(error)));
            if closing || self.stop.load(Ordering::Acquire) {
                let _ = refuse("Chat is closing".into());
                continue;
            }
            let (run, command) = match command {
                ActorCommand::RetainedProvider { .. } => {
                    unreachable!("retained inputs normalized above")
                }
                ActorCommand::PinnedLaunchDirectory(response) => {
                    let result = if let Some(holder) = self.native_holder.as_mut() {
                        holder.pinned_launch_directory()
                    } else if let Some(child) = self.child.as_ref() {
                        match child.observed_exit_status() {
                            Ok(None) => child
                                .directory
                                .as_ref()
                                .ok_or(
                                    "This provider has no original launch-directory descriptor"
                                        .to_owned(),
                                )
                                .and_then(|directory| {
                                    let identity =
                                        crate::agent_execution::DirectoryIdentity::from_metadata(
                                            &directory
                                                .metadata()
                                                .map_err(|error| error.to_string())?,
                                        );
                                    super::protocol::validate_launch_directory(
                                        directory, identity,
                                    )?;
                                    Ok((identity, directory.clone()))
                                }),
                            Ok(Some(_)) => Err("Original provider process has exited".into()),
                            Err(error) => {
                                Err(format!("Cannot prove original provider is live: {error}"))
                            }
                        }
                    } else {
                        Err("This provider has no live owned launch-directory descriptor".into())
                    };
                    let _ = response.send(result);
                    let _ = reply.send(Ok(()));
                    continue;
                }
                ActorCommand::Provider {
                    run,
                    command,
                    decline_only,
                } => {
                    if decline_only {
                        let state = self.state.lock().unwrap_or_else(|p| p.into_inner());
                        let valid = match &command {
                            Command::Answer {
                                request_id, action, ..
                            } => {
                                state.requests.iter().any(|request| {
                                    request.id == *request_id
                                        && request.actions.iter().any(|choice| choice.id == *action)
                                }) && self.adapter.answer_declines(request_id, action)
                            }
                            _ => false,
                        };
                        if !valid {
                            let _ = refuse(NOT_A_DECLINE.into());
                            continue;
                        }
                    }
                    (run, command)
                }
                ActorCommand::IdlePrompt(text) => {
                    if !accepts_prompt(&self.state.lock().unwrap_or_else(|p| p.into_inner())) {
                        let _ = refuse(
                            "The conversation is no longer idle for courier delivery".into(),
                        );
                        continue;
                    }
                    (
                        None,
                        Command::Prompt {
                            text,
                            attachments: Vec::new(),
                            mode: super::PromptMode::Send,
                        },
                    )
                }
                ActorCommand::UpdateSource(update) => {
                    let SourceUpdate {
                        source,
                        execution_console,
                        paired_holder,
                    } = *update;
                    let result = self
                        .native_holder
                        .as_mut()
                        .ok_or("Native holder is unavailable".to_string())
                        .and_then(|holder| {
                            holder.update_source(source, execution_console, paired_holder)
                        });
                    let failed = result.is_err();
                    let _ = reply.send(result.map_err(Rejection::Unacknowledged));
                    // A partial control reply must never be reused.
                    if failed {
                        return Err("Native ownership update was not acknowledged".into());
                    }
                    continue;
                }
                ActorCommand::Interrupt {
                    run,
                    including_subagents,
                } => {
                    if run.is_some_and(|run| self.execution.active() != Some(run)) {
                        let _ = refuse(
                            "The requested run is no longer the provider's active turn".into(),
                        );
                        continue;
                    }
                    if !including_subagents {
                        let state = self.state.lock().unwrap_or_else(|p| p.into_inner());
                        let affected: Vec<_> = state
                            .subagents
                            .iter()
                            .filter(|agent| agent.stopped_by_interrupt(&state.capabilities))
                            .map(super::HeldSubagent::of)
                            .collect();
                        if !affected.is_empty() {
                            let _ = refuse(interrupt_refusal(&affected.iter().collect()));
                            continue;
                        }
                    }
                    (None, Command::Interrupt)
                }
                ActorCommand::CloseIfIdle {
                    generation,
                    detached,
                    closed,
                } => {
                    let state = self.state.lock().unwrap_or_else(|p| p.into_inner());
                    // A stale generation must never hide new requests, child
                    // work or pending transport. Only a proved idle actor can
                    // return the retryable generation-change receipt.
                    let readiness = self.update_readiness(&state);
                    if readiness != UpdateReadiness::Idle {
                        let _ = refuse(idle_close_refusal(readiness, &state));
                        continue;
                    }
                    if detached.is_some() && state.background_tasks != 0 {
                        let _ = refuse(
                            "The provider reports background tasks that closing would end".into(),
                        );
                        continue;
                    }
                    let unchanged = state.generation == generation;
                    if unchanged {
                        if let Some(detached) = detached {
                            self.detached_work.decide(detached);
                        }
                        self.stop.store(true, Ordering::Release);
                    }
                    let _ = closed.send(unchanged);
                    let _ = reply.send(Ok(()));
                    continue;
                }
                ActorCommand::CheckUpdateReadiness(readiness) => {
                    let state = self.state.lock().unwrap_or_else(|p| p.into_inner());
                    let _ = readiness.send(self.update_readiness(&state));
                    let _ = reply.send(Ok(()));
                    continue;
                }
            };
            if matches!(
                command,
                Command::LoadEarlier | Command::LoadLater | Command::Refresh
            ) {
                match page_command(&self.store, &command) {
                    Ok(cached) => {
                        self.touch();
                        if cached {
                            self.apply(Event::HistoryRecovered);
                            let _ = reply.send(Ok(()));
                            continue;
                        }
                        // An earlier page asked for to fill the window is
                        // still on its way, and it answers the reader too:
                        // a second request would be refused as a second load.
                        if command == Command::LoadEarlier
                            && self
                                .state
                                .lock()
                                .unwrap_or_else(|p| p.into_inner())
                                .history_loading
                            && self
                                .store
                                .lock()
                                .unwrap_or_else(|p| p.into_inner())
                                .adopt_fill()
                        {
                            let _ = reply.send(Ok(()));
                            continue;
                        }
                    }
                    Err(error) => {
                        let _ = refuse(error);
                        continue;
                    }
                }
            }
            let login_recovery = if let Command::ResumeAfterLogin { failure, .. } = &command {
                let state = self.state.lock().unwrap_or_else(|p| p.into_inner());
                if state.error.as_ref() != Some(failure)
                    || !super::requires_sign_in(crate::agent::Provider::Codex, failure)
                    || state.busy
                    || state.waiting
                    || !state.requests.is_empty()
                    || state.exited
                    || !state.ready
                {
                    let _ = refuse(
                        "The sign-in failure changed or the conversation is no longer idle".into(),
                    );
                    continue;
                }
                Some(failure.clone())
            } else {
                None
            };
            let starts_turn = matches!(&command, Command::Prompt { .. });
            if run.is_some() && starts_turn {
                let state = self.state.lock().unwrap_or_else(|p| p.into_inner());
                if !state.ready || state.busy || state.waiting || !state.requests.is_empty() {
                    let _ = refuse("The provider is no longer idle for the accepted run".into());
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
                Command::Answer {
                    request_id, action, ..
                } => !self.adapter.answer_declines(request_id, action),
                _ => false,
            };
            if authorizes_work
                && let Some(context) = &self.launch_context
                && let Err(error) = context.verify_directory(&self.working_directory)
            {
                let _ = refuse(error);
                continue;
            }
            if authorizes_work
                && let Some(scope) = &self.launch_account_scope
                && let Err(error) = scope.revalidate()
            {
                let _ = refuse(error);
                continue;
            }
            let loads_earlier = command == Command::LoadEarlier;
            let prepared = match run {
                Some(run) => self.adapter.command_for_run(run, command),
                None => self.adapter.command(command),
            };
            match prepared {
                Ok(step) => {
                    effects.possible();
                    if login_recovery.is_some() {
                        self.login_recovery = login_recovery;
                    }
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
                    let _ = reply.send(result.clone().map_err(Rejection::Unacknowledged));
                    result?;
                    if started {
                        self.publish_activity();
                    }
                }
                Err(message) => {
                    if loads_earlier {
                        // The fill that follows must not ask again for the
                        // page the reader was just refused.
                        self.store
                            .lock()
                            .unwrap_or_else(|p| p.into_inner())
                            .history_failed();
                    }
                    self.apply(Event::Error {
                        message: message.clone(),
                        fatal: false,
                    });
                    let _ = refuse(message);
                }
            }
        }
        if closing {
            return Ok(());
        }
        self.fill_history()
    }
    /// Asks the provider for the earlier page the transcript window has room
    /// for, which no reader asked for; see [`Store::fill`].
    ///
    /// Only while someone has the conversation shown: the host reads windows
    /// of conversations nobody shows, and a page arriving there would change
    /// the generation that its release and retirement checks hold.
    fn fill_history(&mut self) -> Result<(), String> {
        if self.stop.load(Ordering::Acquire)
            || !self.visible.load(Ordering::Acquire)
            || self.queued > QUEUE_BACKPRESSURE
            || self
                .state
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .history_loading
            || !self.store.lock().unwrap_or_else(|p| p.into_inner()).fill()
        {
            return Ok(());
        }
        match self.adapter.command(Command::LoadEarlier) {
            Ok(step) => {
                // A provider may decline without saying so, as one with no
                // history to page yet does.
                let answered = step.events.iter().any(|event| {
                    matches!(
                        event,
                        Event::History { .. }
                            | Event::HistoryLoading(true)
                            | Event::HistoryError(_)
                    )
                });
                self.step(step)?;
                if !answered {
                    self.store
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .history_failed();
                }
                Ok(())
            }
            // Nobody asked for this page, so its failure is nobody's error.
            // The reader can still ask for it, and hear why it failed.
            Err(error) => {
                tracing::debug!(%error, "No earlier history to fill the transcript window");
                self.store
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .history_failed();
                Ok(())
            }
        }
    }
    fn update_readiness(&self, state: &Snapshot) -> UpdateReadiness {
        if !state.ready || state.exited || !self.pending.is_empty() || !self.output.is_empty() {
            UpdateReadiness::Unready
        } else if state.holds_work() {
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
        let observed = step.events.iter().any(|event| {
            !matches!(
                event,
                Event::AccountUsage(_) | Event::AgentAddress(_) | Event::PromptCache(_)
            )
        });
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
        let suppress_current_notification = step.suppress_current_notification;
        if !suppress_current_notification && let Some(preview) = &step.notification_preview {
            self.state
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .notification_preview = Arc::new(preview.clone());
        }
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
            let expendable = self.adapter.expendable_write(&value);
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
            self.pending.push_back(QueuedWrite { bytes, expendable });
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
        let mut frame_contents = if !suppress_current_notification
            && step
                .events
                .iter()
                .any(|event| matches!(event, Event::TurnFinished { .. }))
        {
            let frame_context = {
                let state = self.state.lock().unwrap_or_else(|p| p.into_inner());
                super::notification::FrameContext::capture(&state)
            };
            frame_context.project(&step.events)
        } else {
            Default::default()
        };
        // Preserve decoded outcomes even when the transcript cache fails.
        // Stop further Store writes, then report the original error after
        // this bounded frame's semantic proof has been published.
        let mut frame_error = None;
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
            if let Event::ActiveModel(model) = event {
                self.prompt_cache
                    .model(model.as_ref().map(|model| model.id.as_str()));
                let changed = {
                    let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
                    if state.active_model != model {
                        state.active_model = model.clone();
                        state.active_model_revision = state.active_model_revision.saturating_add(1);
                        Some(state.active_model_revision)
                    } else {
                        None
                    }
                };
                if let Some(revision) = changed {
                    (self.notify)(Update::ActiveModel { revision, model });
                }
                continue;
            }
            if let Event::PromptCache(evidence) = event {
                // A frame of an earlier turn says nothing about the cache now.
                if !suppress_current_notification && self.prompt_cache.observe(evidence) {
                    self.resident_dirty = true;
                }
                continue;
            }
            if let Event::AgentAddress(address) = event {
                if self.agent_addresses.learn(address) {
                    (self.notify)(Update::AgentAddresses(self.agent_addresses.list()));
                }
                continue;
            }
            let fatal = match &event {
                Event::Error {
                    message,
                    fatal: true,
                } => Some(message.clone()),
                _ => None,
            };
            // The disk-backed transcript store consumes text/tool items. Keep
            // their bounded live excerpt before that boundary, without ever
            // hydrating the page a window happens to be reading.
            if !suppress_current_notification
                && matches!(
                    &event,
                    Event::Item(_)
                        | Event::TextDelta { .. }
                        | Event::TurnStarted
                        | Event::Reset { .. }
                )
            {
                let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
                Arc::make_mut(&mut state.notification_preview).observe(&event);
            }
            // Only clone semantic metadata, never transcript/tool bodies.
            let authority = matches!(
                &event,
                Event::TurnStarted
                    | Event::TurnSettled
                    | Event::TurnFinished { .. }
                    | Event::Usage(_)
                    | Event::Session { .. }
            )
            .then(|| event.clone());
            let events = if frame_error.is_some() {
                None
            } else {
                match self
                    .store
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .apply(event)
                {
                    Ok(events) => Some(events),
                    Err(error) => {
                        frame_error = Some(error);
                        None
                    }
                }
            };
            if let Some(events) = events {
                if events.is_empty() || suppress_current_notification {
                    self.touch();
                }
                for event in events {
                    // Preserve history and independently identified children
                    // and requests. Old parent-turn scalar metadata cannot
                    // replace the current turn's state or notification input.
                    if suppress_current_notification
                        && matches!(
                            &event,
                            Event::Error { .. }
                                | Event::Usage(_)
                                | Event::Plan { .. }
                                | Event::Status { .. }
                                | Event::Waiting(_)
                                | Event::TurnStarted
                                | Event::TurnSettled
                                | Event::TurnFinished { .. }
                        )
                    {
                        continue;
                    }
                    let content = matches!(&event, Event::TurnFinished { .. })
                        .then(|| frame_contents.pop_front())
                        .flatten();
                    self.apply_with_attention(event, content, true);
                }
            } else if let Some(event) = authority {
                if suppress_current_notification {
                    continue;
                }
                let content = matches!(&event, Event::TurnFinished { .. })
                    .then(|| frame_contents.pop_front())
                    .flatten();
                self.apply_with_attention(event, content, true);
            }
            if self.retained > STATE_LIMIT {
                frame_error.get_or_insert_with(|| {
                    "Provider request/model metadata exceeds native IPC presentation capacity"
                        .into()
                });
            }
            if let Some(message) = fatal {
                frame_error.get_or_insert(message);
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
        match frame_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
    fn publish_current_attention(&self, evidence: crate::notifications::Evidence) {
        let content = self
            .state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .notification_content(evidence.kind);
        (self.notify)(Update::AttentionContext {
            evidence,
            content: Some(content),
        });
    }
    fn apply(&mut self, event: Event) {
        self.apply_with_attention(event, None, false);
    }
    fn apply_with_attention(
        &mut self,
        event: Event,
        content: Option<crate::notifications::Content>,
        preview_observed: bool,
    ) {
        // The Store consumes parent bodies. Child bodies still project here,
        // and Claude repeats the same identity on ordinary body frames.
        // Neither should turn hidden streaming into metadata RPCs.
        if self.resident_changed.is_some() {
            self.resident_dirty |= match &event {
                Event::Item(_)
                | Event::TextDelta { .. }
                | Event::OutputDelta { .. }
                | Event::SubagentItem { .. }
                | Event::SubagentTextDelta { .. }
                | Event::SubagentOutputDelta { .. } => false,
                Event::Session { id, title } => {
                    let state = self.state.lock().unwrap_or_else(|p| p.into_inner());
                    let changed = state.session_id.as_deref() != Some(id.as_str())
                        || title
                            .as_ref()
                            .is_some_and(|title| state.title.as_ref() != Some(title));
                    drop(state);
                    changed
                        || self
                            .confirmed_session
                            .lock()
                            .unwrap_or_else(|p| p.into_inner())
                            .as_deref()
                            != Some(id.as_str())
                }
                _ => true,
            };
        }
        let child_changed = matches!(
            &event,
            Event::Subagent { .. }
                | Event::SubagentItem { .. }
                | Event::SubagentTextDelta { .. }
                | Event::SubagentOutputDelta { .. }
                | Event::SubagentItemsRemoved { .. }
                | Event::Reset { .. }
        );
        if matches!(
            event,
            Event::TurnStarted | Event::TurnSettled | Event::TurnFinished { .. }
        ) {
            if let Some(evidence) = self.attention.apply(&event) {
                (self.notify)(Update::AttentionContext { evidence, content });
            }
            if !matches!(event, Event::TurnStarted) {
                return;
            }
        }
        if matches!(&event, Event::Reset { .. }) {
            self.attention.apply(&event);
            *self
                .confirmed_session
                .lock()
                .unwrap_or_else(|p| p.into_inner()) = None;
        }
        let mut diagnostic_store =
            child_changed.then(|| self.store.lock().unwrap_or_else(|p| p.into_inner()));
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        recover_login_error(&mut state.error, &mut self.login_recovery, &event);
        recover_history_error(&mut state.error, &mut self.history_error, &event);
        let name_changed = match &event {
            Event::OpeningTitle(title) => state.opening_title.as_ref() != Some(title),
            Event::ProviderTitleCleared { id } => {
                state.session_id.as_ref() == Some(id) && !state.provider_title_cleared
            }
            Event::Session { id, title } => {
                state.session_id.as_ref() != Some(id)
                    || title
                        .as_ref()
                        .is_some_and(|title| state.title.as_ref() != Some(title))
            }
            Event::Reset { .. } => state.session_id.is_some() || state.opening_title.is_some(),
            _ => false,
        };
        apply_projected_event(&mut state, &mut self.indices, event, preview_observed);
        if let Some(store) = &mut diagnostic_store {
            let owners = store.diagnostics.owners(&state);
            drop(state);
            // Keep the store lock across projection and source publication so
            // no snapshot caches this generation without its final authority.
            // Disk pruning itself never holds the resident-state lock.
            store.diagnostics.retain(&owners);
            state = self.state.lock().unwrap_or_else(|p| p.into_inner());
            store.diagnostics.publish(&mut state);
        }
        // Only metadata is resident independently of the disk-backed window.
        let items = std::mem::take(&mut state.items);
        let child_items: Vec<_> = state
            .subagents
            .iter_mut()
            .map(|agent| std::mem::take(&mut agent.items))
            .collect();
        // Owned edit summaries have their own item/byte budgets; they do not
        // count toward the provider's request/model identity budget.
        let file_changes = std::mem::take(&mut state.file_changes);
        let mut counter = ByteCounter(0);
        let _ = serde_json::to_writer(&mut counter, &*state);
        state.file_changes = file_changes;
        state.items = items;
        for (agent, items) in state.subagents.iter_mut().zip(child_items) {
            agent.items = items;
        }
        self.retained = counter.0;
        state.generation = state.generation.saturating_add(1);
        self.dirty = true;
        drop(state);
        if name_changed {
            (self.notify)(Update::ConversationNameChanged);
        }
    }
    fn publish_activity(&mut self) {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        // Evaluate after the whole provider frame, including any children or
        // attention requests reported after the parent result.
        if self.attention.finish(&mut state, observed_at()) {
            self.resident_dirty = true;
            state.generation = state.generation.saturating_add(1);
            self.dirty = true;
        }
        let activity = state.activity();
        let generation = state.generation;
        let waiting_fingerprint = if activity.waits_on_user() {
            state.waiting_fingerprint()
        } else {
            None
        };
        let waiting_changed = waiting_fingerprint != self.waiting_fingerprint;
        self.waiting_fingerprint = waiting_fingerprint;
        let became_ready = state.ready && !self.ready;
        self.ready = state.ready;
        let prompt_available = accepts_prompt(&state) && !self.stop.load(Ordering::Acquire);
        let became_available = prompt_available && !self.prompt_available;
        self.prompt_available = prompt_available;
        drop(state);
        let baselined = std::mem::replace(&mut self.attention_baselined, true);
        let activity_changed = activity != self.activity;
        if activity_changed {
            self.activity = activity;
            if let Some((run, update)) = self.execution.activity(activity) {
                (self.notify)(Update::Execution { run, update });
            }
        }
        if activity.waits_on_user() && baselined && (activity_changed || waiting_changed) {
            self.publish_current_attention(crate::notifications::Evidence {
                key: format!("{}:waiting:{generation}", self.attention.key),
                kind: crate::notifications::Kind::Waiting,
            });
        }
        if activity_changed {
            (self.notify)(Update::Activity {
                state: activity,
                generation,
            });
        }
        if became_ready {
            (self.notify)(Update::Ready);
        }
        if became_available {
            // `provider_step` follows removal of its complete JSON line from
            // `output`, so an accepting provider has no framing bytes whose
            // ownership we could discard here. A large tool result otherwise
            // leaves its geometric allocation resident for the lifetime of a
            // retained actor, one process per Claude conversation.
            self.reclaim_idle_frame_buffer();
            (self.notify)(Update::PromptAvailable);
        }
        // apply_step has installed confirmed_session before reaching here.
        self.publish_resident_metadata();
    }
    fn publish_resident_metadata(&mut self) {
        if std::mem::take(&mut self.resident_dirty)
            && let Some(changed) = &self.resident_changed
        {
            changed();
        }
    }
    fn touch(&mut self) {
        self.state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .generation += 1;
        self.dirty = true;
    }

    fn reclaim_idle_frame_buffer(&mut self) {
        if release_idle_frame_buffer(&mut self.output) {
            // Do not call process-wide malloc_trim here: ordinary ACP/OMP
            // actors share cagentsd's allocator and would serialize its
            // unrelated arenas.
            #[cfg(test)]
            self.idle_frame_buffer_capacity
                .store(self.output.capacity(), Ordering::Release);
        }
    }
}

/// Return idle JSON-line framing slack to the allocator. Callers must retain
/// any incomplete frame; a complete frame has already been parsed and moved to
/// its durable transcript/state owners before this boundary.
fn release_idle_frame_buffer(buffer: &mut Vec<u8>) -> bool {
    if !buffer.is_empty() || buffer.capacity() <= IDLE_FRAME_BUFFER_CAPACITY {
        return false;
    }
    buffer.shrink_to(IDLE_FRAME_BUFFER_CAPACITY);
    true
}

fn page_command(store: &Mutex<Store>, command: &Command) -> Result<bool, String> {
    let mut store = store.lock().unwrap_or_else(|p| p.into_inner());
    match command {
        Command::LoadEarlier => store.earlier(),
        Command::LoadLater => store.later(),
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
fn recover_login_error(error: &mut Option<String>, pending: &mut Option<String>, event: &Event) {
    match event {
        Event::LoginVerificationFailed { failure, detail } => {
            if pending.as_ref() == Some(failure) && error.as_ref() == Some(failure) {
                *error = Some(super::sign_in_verification_failure(failure, detail));
                *pending = None;
            }
        }
        Event::LoginRecovered { failure } => {
            if pending.as_ref() == Some(failure) && error.as_ref() == Some(failure) {
                *error = None;
            }
            *pending = None;
        }
        // Even an identical later error is a new failure and invalidates the
        // old proof. No unrelated transcript/cache error or turn is cleared.
        Event::Error { .. }
        | Event::HistoryError(_)
        | Event::Reset { .. }
        | Event::Exited { .. }
        | Event::TurnStarted
        | Event::Request(_)
        | Event::Waiting(true)
        | Event::Status { busy: true, .. } => *pending = None,
        _ => {}
    }
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

/// Protocol regressions project through the same reducer as a resident actor.
#[cfg(test)]
pub(crate) fn project_events_for_test(state: &mut Snapshot, events: Vec<Event>) {
    let mut indices = state
        .items
        .iter()
        .enumerate()
        .map(|(index, item)| (item.id.clone(), index))
        .collect();
    for event in events {
        apply_event(state, &mut indices, event);
    }
}

#[cfg(test)]
fn apply_event(state: &mut Snapshot, indices: &mut BTreeMap<String, usize>, event: Event) {
    apply_projected_event(state, indices, event, false);
}

fn apply_projected_event(
    state: &mut Snapshot,
    indices: &mut BTreeMap<String, usize>,
    event: Event,
    preview_observed: bool,
) {
    // Frame events have already reached the resident preview before Store.
    // Do not consume a restored start boundary or append a delta twice.
    if !preview_observed {
        Arc::make_mut(&mut state.notification_preview).observe(&event);
    }
    let child_copy_update = child_text_copy::prepare(state, &event);
    let latest_history = matches!(&event, Event::LatestHistory { .. });
    let child_changed = matches!(
        &event,
        Event::Subagent { .. }
            | Event::SubagentItem { .. }
            | Event::SubagentTextDelta { .. }
            | Event::SubagentOutputDelta { .. }
            | Event::SubagentItemsRemoved { .. }
    );
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
        Event::TurnStarted => {
            state.prompt_suggestion = None;
            state.turn_generation = state.turn_generation.wrapping_add(1);
            state.finished_at = None;
        }
        Event::TurnSettled | Event::TurnFinished { .. } | Event::Execution { .. } => {}
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
            state.finished_at = None;
            state.prompt_suggestion = None;
            state.branch_points.clear();
            state.forked = None;
            state.interrupt_mode = None;
            state.queued_messages.clear();
            state.queue_removals.clear();
            state.items.clear();
            state.subagents.clear();
            indices.clear();
            state.requests.clear();
            // Where the conversation compacts is the session's setting, not
            // the cleared conversation's.
            state.usage = crate::chat::Usage {
                compact_limit: state.usage.compact_limit,
                ..Default::default()
            };
            state.session_id = None;
            state.title = None;
            state.provider_title_cleared = false;
            state.opening_title = None;
            state.has_earlier = false;
            state.has_later = false;
            state.history_loading = false;
            state.detail = reason;
            state.busy = false;
            state.waiting = false;
            state.error = None;
            state.read_only_reason = None;
            state.plan.clear();
            state.permission_mode = None;
            state.permissions = None;
            state.permission_denials.clear();
        }
        Event::Session { id, title } => {
            if state.session_id.as_deref() != Some(id.as_str()) {
                state.prompt_suggestion = None;
                if state.session_id.is_some() {
                    state.opening_title = None;
                }
                state.title = title;
                state.provider_title_cleared = false;
            } else if title.is_some() {
                // Identity-only frames carry no new naming information.
                state.title = title;
                state.provider_title_cleared = false;
            }
            state.session_id = Some(id);
        }
        Event::ProviderTitleCleared { id } => {
            if state.session_id.as_ref() == Some(&id) {
                state.title = None;
                state.provider_title_cleared = true;
            }
        }
        Event::OpeningTitle(title) => state.opening_title = Some(title),
        Event::OpeningHistoryComplete => {}
        Event::Plan { steps } => state.plan = steps,
        // About the account rather than the conversation: `step` forwards it
        // to the surfaces before the store, so it never reaches a snapshot.
        Event::AccountUsage(_)
        | Event::AgentAddress(_)
        | Event::PromptCache(_)
        | Event::LoginRecovered { .. }
        | Event::LoginVerificationFailed { .. } => {}
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
        Event::ActiveModel(model) => state.active_model = model,
        Event::FastMode(value) => state.fast_mode = Some(value),
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
            model,
            waiting,
            detail,
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
            if let Some(model) = model {
                agent.model = Some(model);
            }
            if let Some(waiting) = waiting {
                agent.waiting = waiting;
            }
            let has_detail = detail.is_some();
            if let Some(detail) = detail {
                agent.detail = detail;
            }
            if let Some(status) = status {
                agent.status = status;
                if status != ToolStatus::Running {
                    agent.waiting = false;
                    if !has_detail {
                        agent.detail.clear();
                    }
                }
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
        Event::BackgroundTasks(live) => state.background_tasks = live,
        Event::Status { busy, detail } => {
            if busy {
                state.prompt_suggestion = None;
            }
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
            if value.cost_estimate.is_some() {
                state.usage.cost_estimate = value.cost_estimate;
            }
            // Every reading states where the provider compacts as it knows it
            // then, and `None` says it doesn't compact early or hasn't said.
            state.usage.compact_limit = value.compact_limit;
        }
        Event::PromptSuggestion {
            session_id,
            source,
            text,
        } => {
            if !state.busy
                && !state.exited
                && state.session_id.as_deref() == Some(session_id.as_str())
                && crate::chat::PromptSuggestion::valid_text(&text)
            {
                state.prompt_suggestion = Some(crate::chat::PromptSuggestion {
                    session_id,
                    source,
                    turn_generation: state.turn_generation,
                    text,
                });
            }
        }
        Event::PromptSuggestionCleared => state.prompt_suggestion = None,
        Event::Composer { text } => state.composer = Some(text),
        Event::OpenUrl { url, instructions } => state.open_url = Some((url, instructions)),
        Event::Error { message, fatal } => {
            state.error = Some(message);
            if fatal {
                state.prompt_suggestion = None;
                state.busy = false;
                state.history_loading = false;
            }
        }
        Event::Exited { .. } => {
            state.prompt_suggestion = None;
            state.exited = true;
            for agent in &mut state.subagents {
                if agent.status == ToolStatus::Waiting {
                    agent.status = ToolStatus::Stopped;
                    agent.detail =
                        "The provider exited before its background work reported back".to_owned();
                }
            }
            state.waiting = false;
            state.busy = false;
            state.ready = false;
            state.history_loading = false;
            state.background_tasks = 0;
        }
        Event::ReadOnlyHistory { reason } => state.read_only_reason = Some(reason),
        Event::Ready(ready) => {
            if !ready {
                state.prompt_suggestion = None;
            }
            state.ready = ready;
        }
    }
    if child_changed {
        trim_subagents(state);
    }
    child_text_copy::finish(state, child_copy_update);
}

/// Marks the steps still running or waiting as stopped, copying only those
/// items. Nothing a provider that ended was doing outlives it; a step it left
/// going would otherwise read as still working. A subagent waiting on its
/// background work is ended by [`Event::Exited`], which says why.
fn stop_snapshot_steps(state: &mut Snapshot) {
    stop_running_steps(&mut state.items);
    for agent in &mut state.subagents {
        if agent.status == ToolStatus::Running {
            agent.status = ToolStatus::Stopped;
            agent.waiting = false;
        }
        stop_running_steps(&mut agent.items);
    }
}

fn stop_running_steps(items: &mut [Arc<Item>]) {
    let now = observed_at();
    for item in items {
        if item.content.status().is_some_and(ToolStatus::is_live) {
            let item = Arc::make_mut(item);
            if let Some(status) = item.content.status_mut() {
                *status = ToolStatus::Stopped;
                // It ended now, so its duration runs to the provider's exit.
                item.started_at = item.started_at.or(item.at);
                item.at = now;
            }
        }
    }
}

/// Running task identities authorize interruption and idle waits. Retiring
/// presentation history must never retire that ownership evidence. Keep only
/// the newest 32 rich child transcripts, with older active entries remaining
/// available for status and selective Stop.
fn trim_subagents(state: &mut Snapshot) {
    const RICH_HISTORY: usize = 32;
    while state.subagents.len() > RICH_HISTORY {
        let Some(finished) = state
            .subagents
            .iter()
            .position(|agent| !agent.status.is_live())
        else {
            break;
        };
        state.subagents.remove(finished);
    }
    for agent in state.subagents.iter_mut().rev().skip(RICH_HISTORY) {
        agent.items.clear();
    }
}

fn subagent<'a>(state: &'a mut Snapshot, id: &str) -> &'a mut Subagent {
    if let Some(index) = state.subagents.iter().position(|agent| agent.id == id) {
        return &mut state.subagents[index];
    }
    state.subagents.push(Subagent {
        model: None,
        waiting: false,
        detail: String::new(),
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
        && child_diagnostics::marked(last)
    {
        child_diagnostics::project(Arc::make_mut(last));
    }
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

/// Exercise a real protocol adapter through the live actor/store callback,
/// sampling content at attention publication rather than after the command.
#[cfg(test)]
pub(crate) fn notification_frame_for_test(
    adapter: Box<dyn Adapter>,
    frame: serde_json::Value,
    fail_store: bool,
) -> (
    crate::notifications::Evidence,
    crate::notifications::Content,
) {
    let mut receipts = notification_frames_for_test(adapter, vec![frame], fail_store);
    assert!(!receipts.is_empty(), "one authoritative outcome expected");
    receipts.remove(0)
}

/// A real provider adapter keeps its pending request map across frames, while
/// the actual actor/store publishes semantic waiting-context revisions.
#[cfg(test)]
pub(crate) fn notification_frames_for_test(
    adapter: Box<dyn Adapter>,
    frames: Vec<serde_json::Value>,
    fail_store: bool,
) -> Vec<(
    crate::notifications::Evidence,
    crate::notifications::Content,
)> {
    notification_frames_with_snapshot_for_test(adapter, frames, fail_store).0
}

#[cfg(test)]
pub(crate) fn notification_frames_with_snapshot_for_test(
    adapter: Box<dyn Adapter>,
    frames: Vec<serde_json::Value>,
    fail_store: bool,
) -> (
    Vec<(
        crate::notifications::Evidence,
        crate::notifications::Content,
    )>,
    Snapshot,
) {
    let (receipts, snapshot, ()) =
        notification_frames_with_probe_for_test(adapter, frames, fail_store, |_, _| ());
    (receipts, snapshot)
}

#[cfg(test)]
pub(crate) fn notification_frames_with_probe_for_test<R>(
    adapter: Box<dyn Adapter>,
    frames: Vec<serde_json::Value>,
    fail_store: bool,
    probe: impl FnOnce(
        &Snapshot,
        &dyn Fn(&super::TextCopySource, u64) -> Result<super::TextCopyPage, String>,
    ) -> R,
) -> (
    Vec<(
        crate::notifications::Evidence,
        crate::notifications::Content,
    )>,
    Snapshot,
    R,
) {
    assert!(!frames.is_empty() && frames.len() <= 32);
    let count = frames.len();
    struct Frame {
        adapter: Box<dyn Adapter>,
        frames: std::collections::VecDeque<serde_json::Value>,
        interrupted: Arc<AtomicBool>,
    }
    impl Adapter for Frame {
        fn initialize(&mut self) -> Result<Step, String> {
            Ok(Step {
                events: vec![Event::Ready(true), Event::TurnStarted],
                ..Default::default()
            })
        }
        fn command(&mut self, command: Command) -> Result<Step, String> {
            match command {
                Command::Refresh => self
                    .adapter
                    .receive(self.frames.pop_front().ok_or("fixture frames consumed")?),
                Command::Interrupt if self.frames.is_empty() => {
                    // The script has no live provider work. A busy captured
                    // snapshot still needs an explicit fixture teardown ACK;
                    // never consume another provider frame or emit an outcome.
                    self.interrupted.store(true, Ordering::Release);
                    Ok(Step {
                        events: vec![Event::Status {
                            busy: false,
                            detail: "Fixture interruption acknowledged".into(),
                        }],
                        ..Step::default()
                    })
                }
                _ => Err("fixture accepts scripted Refresh and final Interrupt only".into()),
            }
        }
        fn receive(&mut self, _: serde_json::Value) -> Result<Step, String> {
            Ok(Step::default())
        }
    }
    let directory = tempfile::tempdir().unwrap();
    let spec = ProcessSpec {
        stdout_noise: None,
        followed: false,
        execution: None,
        messages: None,
        unset_environment: Default::default(),
        transport: TransportSpec::JsonLines,
        program: "/bin/cat".into(),
        arguments: vec![],
        working_directory: directory.path().into(),
        environment: BTreeMap::new(),
    };
    let interrupted = Arc::new(AtomicBool::new(false));
    let mut adapter: Box<dyn Adapter> = Box::new(Frame {
        adapter,
        frames: frames.into(),
        interrupted: interrupted.clone(),
    });
    let mut initial = adapter.initialize().unwrap();
    let store = preflight(&spec, &mut initial).unwrap();
    let (send, receive) = crossbeam_channel::bounded(64);
    let handle = LocalHandle::start(spec, adapter, initial, store, None, move |update| {
        if let Update::AttentionContext { evidence, content } = update {
            let _ = send.try_send((
                evidence,
                content.expect("fixture fits the bounded frame context budget"),
            ));
        }
    })
    .unwrap();
    handle.check_update_readiness().unwrap();
    if fail_store {
        handle.0.store.lock().unwrap().read_only_for_test().unwrap();
    }
    let commands: Vec<_> = (0..count)
        .map(|_| handle.command(Command::Refresh))
        .collect();
    let first = receive.recv_timeout(Duration::from_secs(2));
    let later: Vec<_> = receive.try_iter().collect();
    // Parent items belong to the transcript Store, so the resident metadata
    // alone is not a client snapshot. Hydrate through the normal read boundary
    // before probing original-copy sources and capturing pre-teardown state.
    let snapshot = handle.snapshot(None).unwrap();
    // Collect observations before teardown; callers assert them only after
    // the existing process cleanup/completion guards below have succeeded.
    let probed = probe(&snapshot, &|source, offset| {
        handle.text_copy(source, offset)
    });
    // Retire the actual process before asserting any result, including the
    // missing-callback/error paths, just as the one-frame fixture did.
    let closed = handle.close();
    for (index, command) in commands.into_iter().enumerate() {
        if fail_store && index + 1 == count {
            let error =
                command.expect_err("readonly SQLite must reject the final write-bearing item");
            assert!(
                error.contains("readonly") || error.contains("read-only"),
                "unexpected injected store failure: {error}"
            );
        } else {
            command.unwrap();
        }
    }
    closed.unwrap();
    if snapshot.busy && !snapshot.exited && snapshot.error.is_none() && !fail_store {
        assert!(
            interrupted.load(Ordering::Acquire),
            "a healthy busy fixture must acknowledge teardown explicitly"
        );
        assert!(
            handle.failure().is_none(),
            "fixture teardown must not wait for an interruption timeout: {:?}",
            handle.failure()
        );
        let teardown_receipts: Vec<_> = receive.try_iter().collect();
        assert!(
            teardown_receipts.is_empty(),
            "fixture teardown cannot invent provider attention: {teardown_receipts:?}"
        );
    }
    assert!(
        handle.is_completed(),
        "real fixture process teardown must still be proved"
    );
    let mut receipts = vec![first.expect("real adapter must publish an outcome through the actor")];
    receipts.extend(later);
    (receipts, snapshot, probed)
}

#[cfg(test)]
mod tests;

/// Explicit retained-helper dispatch after Rust runtime initialization.
pub(crate) fn run_retained_helper_if_requested() -> Option<Result<(), String>> {
    retained_actor::transport::run_helper_if_requested()
}
