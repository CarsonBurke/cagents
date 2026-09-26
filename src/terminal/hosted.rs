use std::{
    collections::BTreeMap,
    io,
    net::Shutdown,
    os::unix::{net::UnixStream, process::CommandExt},
    path::{Path, PathBuf},
    process::{Command, ExitStatus, Stdio},
    sync::{
        Arc, Condvar, Mutex, OnceLock,
        atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use arc_swap::ArcSwap;
use crossbeam_channel::{Receiver, Sender, TryRecvError, TrySendError};
use thiserror::Error;

use crate::model::{HostSessionProof, TerminalId, TerminalSession};

use super::{
    KeyInput, MouseInput, PasteOrigin, RenderSnapshot, TerminalEvent, TerminalSendError,
    TerminalSize, TerminalWake,
    ipc::{
        ClientEnvelope, ClientRequest, FrameCodec, FrameError, MAX_FRAME_BYTES, MAX_INPUT_BYTES,
        PROTOCOL_VERSION, ProtocolError, ProtocolErrorCode, ResumeCursor, ServerEnvelope,
        ServerMessage, ServerResponse, TerminalScroll, TerminalSnapshot, TerminalSpawnOptions,
        TerminalVisibility, WireTerminalSize, daemon_socket_path, read_frame, write_frame,
    },
};

const COMMAND_CAPACITY: usize = 256;
const EVENT_CAPACITY: usize = 256;
const MAX_IN_FLIGHT_REQUESTS: usize = 64;
// Used only while authenticating, before the terminal is attached or accepts
// input. Connected clients are wake-driven by `wait_for_client_work`.
const HANDSHAKE_POLL_INTERVAL: Duration = Duration::from_millis(100);
const INITIAL_RECONNECT_DELAY: Duration = Duration::from_millis(50);
const MAX_RECONNECT_DELAY: Duration = Duration::from_secs(1);
const DAEMON_READY_TIMEOUT: Duration = Duration::from_secs(5);
const DAEMON_READY_POLL_INTERVAL: Duration = Duration::from_millis(25);

type WakeCallback = dyn Fn(TerminalWake) + Send + Sync + 'static;

struct WorkerStop {
    requested: AtomicBool,
    worker: OnceLock<thread::Thread>,
    notification_tx: Sender<()>,
    notification_rx: Receiver<()>,
    connection: Mutex<Option<UnixStream>>,
}

impl Default for WorkerStop {
    fn default() -> Self {
        let (notification_tx, notification_rx) = crossbeam_channel::bounded(1);
        Self {
            requested: AtomicBool::new(false),
            worker: OnceLock::new(),
            notification_tx,
            notification_rx,
            connection: Mutex::new(None),
        }
    }
}

impl WorkerStop {
    fn register_current_thread(&self) {
        let _ = self.worker.set(thread::current());
    }

    fn request(&self) {
        self.requested.store(true, Ordering::Release);
        if let Ok(connection) = self.connection.lock()
            && let Some(connection) = connection.as_ref()
        {
            let _ = connection.shutdown(std::net::Shutdown::Both);
        }
        let _ = self.notification_tx.try_send(());
        if let Some(worker) = self.worker.get() {
            worker.unpark();
        }
    }

    fn is_requested(&self) -> bool {
        self.requested.load(Ordering::Acquire)
    }

    fn register_connection(
        &self,
        stream: &UnixStream,
    ) -> std::io::Result<ConnectionRegistration<'_>> {
        let mut connection = self
            .connection
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        *connection = Some(stream.try_clone()?);
        if self.is_requested() {
            stream.shutdown(std::net::Shutdown::Both)?;
        }
        Ok(ConnectionRegistration(self))
    }

    fn notification(&self) -> &Receiver<()> {
        &self.notification_rx
    }

    fn wait_timeout(&self, timeout: Duration) -> bool {
        if !self.is_requested() {
            thread::park_timeout(timeout);
        }
        self.is_requested()
    }
}

struct ConnectionRegistration<'a>(&'a WorkerStop);
impl Drop for ConnectionRegistration<'_> {
    fn drop(&mut self) {
        self.0
            .connection
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take();
    }
}

#[derive(Debug, Error)]
pub enum HostedTerminalStartError {
    #[error("the expected cagents daemon proof is incomplete")]
    InvalidProof,
    #[error("could not locate the cagents daemon socket: {0}")]
    SocketPath(#[source] io::Error),
    #[error("could not start the hosted terminal client: {0}")]
    Worker(#[source] io::Error),
    #[error("could not locate the running application binary: {0}")]
    CurrentExecutable(#[source] io::Error),
    #[error("the sibling cagentsd binary does not exist at {0}")]
    HostBinaryMissing(PathBuf),
    #[error("could not launch cagentsd at {path}: {source}")]
    LaunchHost {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("cagents daemon did not become ready at {path} within {timeout:?}")]
    HostReadyTimeout { path: PathBuf, timeout: Duration },
    #[error("could not complete cagents daemon startup: {0}")]
    HostStartup(String),
    #[error("cagents daemon exited before becoming ready at {path}: {status}")]
    HostExited { path: PathBuf, status: ExitStatus },
    #[error("could not connect to the cagents daemon socket at {path}: {source}")]
    HostSocket {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

#[derive(Default)]
struct AttachmentState {
    confirmed: bool,
    failure: Option<String>,
}

#[derive(Default)]
struct OutputObservation {
    epoch: u64,
    pending: bool,
}

struct HostedPublishedState {
    snapshot: ArcSwap<RenderSnapshot>,
    generation: AtomicU64,
    event_sequence: AtomicU64,
    activity: Mutex<Option<crate::model::TerminalOutputActivity>>,
    agent_activity: Mutex<Option<crate::agent::Activity>>,
    agent_refresh_pending: AtomicBool,
    agent_revision: AtomicU64,
    activity_current: AtomicBool,
    acknowledged_generation: AtomicU64,
    acknowledgement_tx: Sender<()>,
    acknowledgement_rx: Receiver<()>,
    /// The newest generation, or repaint reason, a wake has already announced
    /// to the UI.
    ///
    /// A published generation is announced exactly once, which is what keeps a
    /// terminal at full output rate to one wake per drawn frame: the daemon
    /// withholds the next capture until the UI acknowledges this generation,
    /// so generations arrive no faster than frames are drawn. Coalescing on a
    /// bare "a wake is pending" flag did the same arithmetic but could only be
    /// cleared by a draw, so a wake that no draw ever answered suppressed
    /// every later generation - the terminal then stayed frozen until an
    /// unrelated pointer event repainted the widget from this snapshot.
    announced_generation: AtomicU64,
    semantic_wake_pending: AtomicBool,
    output_observation: Mutex<OutputObservation>,
    visibility: AtomicU8,
    proof: Mutex<Option<HostSessionProof>>,
    attachment: Mutex<AttachmentState>,
    attachment_changed: Condvar,
    wake: Arc<WakeCallback>,
}

impl HostedPublishedState {
    fn new(
        expected_host: Option<HostSessionProof>,
        visible: bool,
        wake: Arc<WakeCallback>,
    ) -> Self {
        let (acknowledgement_tx, acknowledgement_rx) = crossbeam_channel::bounded(1);
        Self {
            snapshot: ArcSwap::from_pointee(RenderSnapshot::empty()),
            generation: AtomicU64::new(0),
            event_sequence: AtomicU64::new(0),
            activity: Mutex::new(None),
            agent_activity: Mutex::new(None),
            agent_refresh_pending: AtomicBool::new(false),
            agent_revision: AtomicU64::new(0),
            activity_current: AtomicBool::new(false),
            acknowledged_generation: AtomicU64::new(0),
            acknowledgement_tx,
            acknowledgement_rx,
            announced_generation: AtomicU64::new(0),
            semantic_wake_pending: AtomicBool::new(false),
            output_observation: Mutex::new(OutputObservation::default()),
            visibility: AtomicU8::new(Self::visibility_code(if visible {
                TerminalVisibility::Visible
            } else {
                TerminalVisibility::Hidden
            })),
            proof: Mutex::new(expected_host),
            attachment: Mutex::new(AttachmentState::default()),
            attachment_changed: Condvar::new(),
            wake,
        }
    }

    fn publish(&self, snapshot: RenderSnapshot) {
        let generation = snapshot.generation;
        let current_generation = self.generation.load(Ordering::Acquire);
        if generation < current_generation
            || generation == current_generation && !self.snapshot.load().detached
        {
            return;
        }
        let activity = snapshot.activity;
        self.snapshot.store(Arc::new(snapshot));
        self.generation.store(generation, Ordering::Release);
        self.observe_activity(activity);
        if generation == current_generation {
            // Past the guard above, a repeat of the current generation is a
            // hibernated view being repopulated. The grid's own number cannot
            // express that, so only a repaint announcement carries it.
            self.announce_repaint(generation);
        } else {
            self.request_wake(generation);
        }
    }

    fn activity(&self) -> Option<crate::model::TerminalOutputActivity> {
        if !self.activity_current.load(Ordering::Acquire) {
            return None;
        }
        *self
            .activity
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }

    fn observe_activity(&self, observed: Option<crate::model::TerminalOutputActivity>) {
        let Some(observed) = observed else {
            self.invalidate_activity();
            return;
        };
        let mut activity = self
            .activity
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if activity.is_some_and(|old| {
            observed.sequence < old.sequence || observed.sequence == old.sequence && observed != old
        }) {
            return;
        }
        let changed = *activity != Some(observed) || !self.activity_current.load(Ordering::Acquire);
        *activity = Some(observed);
        self.activity_current.store(true, Ordering::Release);
        drop(activity);
        if changed {
            // An activity fact changes what the card chrome reads without
            // advancing the grid, so the generation cannot carry it.
            self.announce_repaint(self.generation.load(Ordering::Acquire));
        }
    }

    fn invalidate_activity(&self) {
        if self.activity_current.swap(false, Ordering::AcqRel) {
            self.announce_repaint(self.generation.load(Ordering::Acquire));
        }
    }

    fn observe_agent_activity(
        &self,
        events: &Sender<TerminalEvent>,
        observed: Option<crate::agent::Activity>,
    ) -> Result<(), TrySendError<TerminalEvent>> {
        let mut current = self
            .agent_activity
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if *current == observed {
            return Ok(());
        }
        let sent = events.try_send(TerminalEvent::AgentActivityChanged(observed));
        *current = observed;
        drop(current);
        self.request_semantic_wake();
        sent
    }

    /// Resolves an incoming repaint against the snapshot this client already
    /// holds, publishing the result.
    ///
    /// Returns `false` when the daemon's delta base is no longer what this
    /// client holds — the local snapshot was replaced by a detached marker on
    /// hibernate, or the grid was resized between frames. The caller must then
    /// ask for a full repaint; silently keeping the stale grid would render
    /// stale text indefinitely.
    fn apply(&self, snapshot: TerminalSnapshot) -> bool {
        // A reply can already be in flight when its view disappears. Do not
        // resolve row deltas or repopulate hibernated visuals in the background;
        // reveal requests the latest grid against the snapshot we actually kept.
        if !self.is_visible() {
            return true;
        }
        let apply_started =
            tracing::enabled!(target: "cagents::terminal_pipeline", tracing::Level::TRACE)
                .then(std::time::Instant::now);
        let current = self.snapshot.load();
        match snapshot.apply(Some(current.as_ref())) {
            Ok(resolved) => {
                self.publish(resolved);
                if let Some(started) = apply_started {
                    tracing::trace!(target: "cagents::terminal_pipeline", stage = "snapshot_apply",
                        generation = self.generation.load(Ordering::Acquire),
                        elapsed_ns = started.elapsed().as_nanos() as u64);
                }
                true
            }
            Err(error) => {
                tracing::debug!(
                    ?error,
                    "could not apply an incremental terminal snapshot; requesting a full repaint"
                );
                false
            }
        }
    }

    fn mark_detached(&self) {
        let generation = self.generation.load(Ordering::Acquire);
        self.snapshot
            .store(Arc::new(RenderSnapshot::detached(generation)));
        // The marker replaces the grid under the generation it already had,
        // so only a repaint announcement can carry it to the UI.
        self.announce_repaint(generation);
    }

    /// Announces a newly published generation, at most once.
    ///
    /// The watermark is the whole of the coalescing: the daemon withholds the
    /// next capture until the UI acknowledges this generation, so announcing
    /// each generation once is one wake per drawn frame. Crucially it does not
    /// depend on a draw ever happening - a generation the UI never drew cannot
    /// silence the generation after it.
    fn request_wake(&self, generation: u64) {
        if !self.is_visible() {
            return;
        }
        if self
            .announced_generation
            .fetch_max(generation, Ordering::AcqRel)
            < generation
        {
            (self.wake)(TerminalWake { generation });
        }
    }

    /// Announces a repaint the grid's generation cannot express: a reveal, a
    /// detached marker, or an activity fact that changed under the generation
    /// already announced.
    ///
    /// These are transitions rather than output, so they are announced
    /// unconditionally. A transition that coincides with a publication still
    /// costs one wake, because the watermark it advances is what makes that
    /// publication's own announcement a no-op.
    fn announce_repaint(&self, generation: u64) {
        if !self.is_visible() {
            return;
        }
        self.announced_generation
            .fetch_max(generation, Ordering::AcqRel);
        (self.wake)(TerminalWake { generation });
    }

    fn request_semantic_wake(&self) {
        if self
            .semantic_wake_pending
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            (self.wake)(TerminalWake {
                generation: self.generation.load(Ordering::Acquire),
            });
        }
    }

    fn observe_output(&self, epoch: u64) {
        let mut observation = self
            .output_observation
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if epoch != observation.epoch || observation.pending {
            return;
        }
        observation.pending = true;
        drop(observation);
        self.request_semantic_wake();
    }

    fn take_observed_output(&self) -> bool {
        let mut observation = self
            .output_observation
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        std::mem::take(&mut observation.pending)
    }

    fn begin_event_drain(&self) {
        self.semantic_wake_pending.store(false, Ordering::Release);
    }

    fn hibernate(&self) {
        self.set_visibility(TerminalVisibility::Hibernated);
        let generation = self.generation.load(Ordering::Acquire);
        self.snapshot
            .store(Arc::new(RenderSnapshot::detached(generation)));
    }

    fn snapshot_cursor(&self) -> u64 {
        // Reconnect/attach replay has no visibility vote yet. A hidden view
        // needs semantic history, not a grid; reveal will request a real cursor.
        if !self.is_visible() {
            return u64::MAX;
        }
        let snapshot = self.snapshot.load();
        if snapshot.detached {
            0
        } else {
            snapshot.generation
        }
    }

    /// Records that the UI drew `observed_generation`, returning the redraw
    /// credit the daemon withholds its next capture for.
    ///
    /// It deliberately re-arms nothing. Every published generation announces
    /// itself exactly once, so there is no announcement for a draw to release
    /// - and no window between clearing a flag and re-reading the published
    /// generation for a publication to fall into.
    fn acknowledge(&self, observed_generation: u64) {
        let previous = self
            .acknowledged_generation
            .fetch_max(observed_generation, Ordering::AcqRel);
        if observed_generation > previous {
            // Returning redraw credit must wake the socket worker even when
            // the daemon is waiting for that credit and no input is arriving.
            // The atomic carries the newest generation; one queued edge is
            // sufficient regardless of how many frames the UI acknowledges.
            let _ = self.acknowledgement_tx.try_send(());
        }
    }

    fn set_visible(&self, visible: bool) {
        self.set_visibility(if visible {
            TerminalVisibility::Visible
        } else {
            TerminalVisibility::Hidden
        });
    }

    fn set_visibility(&self, visibility: TerminalVisibility) {
        self.visibility
            .store(Self::visibility_code(visibility), Ordering::Release);
        if visibility == TerminalVisibility::Visible {
            // A revealed view holds a detached marker or a grid the UI has
            // already drawn, so no generation is owed to it; the repaint is.
            self.announce_repaint(self.generation.load(Ordering::Acquire));
        }
    }

    fn is_visible(&self) -> bool {
        self.desired_visibility() == TerminalVisibility::Visible
    }

    fn desired_visibility(&self) -> TerminalVisibility {
        match self.visibility.load(Ordering::Acquire) {
            0 => TerminalVisibility::Visible,
            1 => TerminalVisibility::Hidden,
            2 => TerminalVisibility::Hibernated,
            _ => unreachable!("hosted terminal visibility encoding is invalid"),
        }
    }

    const fn visibility_code(visibility: TerminalVisibility) -> u8 {
        match visibility {
            TerminalVisibility::Visible => 0,
            TerminalVisibility::Hidden => 1,
            TerminalVisibility::Hibernated => 2,
        }
    }

    fn confirm_attachment(&self) {
        self.attachment
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .confirmed = true;
        self.attachment_changed.notify_all();
    }

    fn fail_attachment(&self, message: String) {
        let mut attachment = self
            .attachment
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if attachment.failure.is_none() {
            attachment.failure = Some(message);
        }
        self.attachment_changed.notify_all();
    }

    fn wait_until_attached(&self, timeout: Duration) -> Result<(), String> {
        let attachment = self
            .attachment
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let (attachment, _) = self
            .attachment_changed
            .wait_timeout_while(attachment, timeout, |state| {
                !state.confirmed && state.failure.is_none()
            })
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(message) = &attachment.failure {
            Err(message.clone())
        } else if attachment.confirmed {
            Ok(())
        } else {
            Err("timed out waiting for the daemon to attach the terminal".to_owned())
        }
    }

    fn proof(&self) -> Option<HostSessionProof> {
        self.proof
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    fn pin_proof(&self, proof: &HostSessionProof) -> bool {
        if !proof.is_complete() {
            return false;
        }
        let mut pinned = self
            .proof
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match pinned.as_ref() {
            Some(expected) => expected == proof,
            None => {
                *pinned = Some(proof.clone());
                true
            }
        }
    }
}

#[derive(Debug)]
enum ClientCommand {
    Input(Vec<u8>),
    Paste(String, PasteOrigin),
    Key(KeyInput),
    Mouse(MouseInput),
    Focus(bool),
    Resize(TerminalSize),
    Scroll(TerminalScroll),
    SetTheme(super::TerminalTheme),
    Visibility(TerminalVisibility),
    AcknowledgeOutput(u64),
    Hibernate,
    Resume { visible: bool },
    ShutdownTerminal(Sender<()>),
    Stop,
    RefreshAgentActivity,
}

impl ClientCommand {
    fn operation(&self) -> &'static str {
        match self {
            Self::Input(_) => "write terminal input",
            Self::Paste(..) => "paste into terminal",
            Self::Key(_) => "send terminal key",
            Self::Mouse(_) => "send terminal mouse input",
            Self::Focus(_) => "set terminal focus",
            Self::Resize(_) => "resize terminal",
            Self::Scroll(_) => "scroll terminal",
            Self::SetTheme(_) => "set terminal theme",
            Self::Visibility(_) => "set terminal visibility",
            Self::AcknowledgeOutput(_) => "acknowledge terminal output",
            Self::Hibernate => "hibernate terminal",
            Self::Resume { .. } => "resume terminal",
            Self::ShutdownTerminal(_) => "shutdown terminal",
            Self::Stop => "detach terminal",
            Self::RefreshAgentActivity => "refresh terminal agent activity",
        }
    }

    fn into_request(self, terminal_id: TerminalId) -> ClientRequest {
        match self {
            Self::Input(bytes) => ClientRequest::Input { terminal_id, bytes },
            Self::Paste(text, origin) => ClientRequest::Paste {
                terminal_id,
                text,
                origin,
            },
            Self::Key(input) => ClientRequest::Key { terminal_id, input },
            Self::Mouse(input) => ClientRequest::Mouse { terminal_id, input },
            Self::Focus(focused) => ClientRequest::Focus {
                terminal_id,
                focused,
            },
            Self::Resize(size) => ClientRequest::Resize {
                terminal_id,
                size: WireTerminalSize::from(size),
            },
            Self::Scroll(scroll) => ClientRequest::Scroll {
                terminal_id,
                scroll,
            },
            Self::SetTheme(theme) => ClientRequest::SetTheme { terminal_id, theme },
            Self::Visibility(visibility) => ClientRequest::SetVisibility {
                terminal_id,
                visibility,
            },
            Self::AcknowledgeOutput(epoch) => {
                ClientRequest::TerminalAcknowledge { terminal_id, epoch }
            }
            Self::Hibernate => ClientRequest::Hibernate { terminal_id },
            Self::Resume { visible } => ClientRequest::Resume {
                terminal_id,
                visible,
            },
            Self::ShutdownTerminal(_) => ClientRequest::ShutdownTerminal { terminal_id },
            Self::Stop => ClientRequest::Detach { terminal_id },
            Self::RefreshAgentActivity => ClientRequest::RefreshAgentActivity { terminal_id },
        }
    }
}

struct HostedHandleInner {
    terminal_id: TerminalId,
    commands: Sender<ClientCommand>,
    events: Receiver<TerminalEvent>,
    published: Arc<HostedPublishedState>,
    stop: Arc<WorkerStop>,
    worker: Mutex<Option<JoinHandle<()>>>,
}

impl Drop for HostedHandleInner {
    fn drop(&mut self) {
        self.stop.request();
        let _ = self.commands.try_send(ClientCommand::Stop);
        if let Ok(worker) = self.worker.get_mut()
            && let Some(worker) = worker.take()
            && worker.thread().id() != thread::current().id()
        {
            let _ = worker.join();
        }
    }
}

/// A cloneable client-side attachment to a terminal owned by the cagents daemon.
///
/// Dropping the last clone detaches its daemon connection; explicit
/// [`shutdown`](Self::shutdown) is required to terminate the remote PTY.
#[derive(Clone)]
pub struct HostedTerminalHandle {
    inner: Arc<HostedHandleInner>,
}

impl std::fmt::Debug for HostedTerminalHandle {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("HostedTerminalHandle")
            .field("terminal_id", &self.inner.terminal_id)
            .field("generation", &self.generation())
            .field("proof", &self.proof())
            .finish_non_exhaustive()
    }
}

impl HostedTerminalHandle {
    pub fn connect(
        terminal_id: TerminalId,
        expected_host: Option<HostSessionProof>,
        wake: impl Fn(TerminalWake) + Send + Sync + 'static,
    ) -> Result<Self, HostedTerminalStartError> {
        let socket = ensure_daemon_ready()?;
        Self::connect_at(socket, terminal_id, expected_host, wake)
    }

    pub fn connect_at(
        socket: impl Into<PathBuf>,
        terminal_id: TerminalId,
        expected_host: Option<HostSessionProof>,
        wake: impl Fn(TerminalWake) + Send + Sync + 'static,
    ) -> Result<Self, HostedTerminalStartError> {
        Self::start(
            socket.into(),
            terminal_id,
            ConnectionIntent::Attach,
            expected_host,
            Arc::new(wake),
        )
    }

    pub fn attach_or_spawn(
        session: TerminalSession,
        options: impl Into<TerminalSpawnOptions>,
        expected_host: Option<HostSessionProof>,
        wake: impl Fn(TerminalWake) + Send + Sync + 'static,
    ) -> Result<Self, HostedTerminalStartError> {
        let socket = ensure_daemon_ready()?;
        Self::attach_or_spawn_at(socket, session, options, expected_host, wake)
    }

    pub fn attach_or_spawn_at(
        socket: impl Into<PathBuf>,
        session: TerminalSession,
        options: impl Into<TerminalSpawnOptions>,
        expected_host: Option<HostSessionProof>,
        wake: impl Fn(TerminalWake) + Send + Sync + 'static,
    ) -> Result<Self, HostedTerminalStartError> {
        let options = options.into();
        let terminal_id = session.id;
        Self::start(
            socket.into(),
            terminal_id,
            ConnectionIntent::Spawn(Box::new(SpawnIntent { session, options })),
            expected_host,
            Arc::new(wake),
        )
    }

    fn start(
        socket: PathBuf,
        terminal_id: TerminalId,
        intent: ConnectionIntent,
        expected_host: Option<HostSessionProof>,
        wake: Arc<WakeCallback>,
    ) -> Result<Self, HostedTerminalStartError> {
        if expected_host
            .as_ref()
            .is_some_and(|proof| !proof.is_complete())
        {
            return Err(HostedTerminalStartError::InvalidProof);
        }
        let (command_tx, command_rx) = crossbeam_channel::bounded(COMMAND_CAPACITY);
        let (event_tx, event_rx) = crossbeam_channel::bounded(EVENT_CAPACITY);
        let initially_visible = match &intent {
            ConnectionIntent::Attach => true,
            ConnectionIntent::Spawn(spawn) => spawn.options.initially_visible,
        };
        let published = Arc::new(HostedPublishedState::new(
            expected_host,
            initially_visible,
            wake,
        ));
        let stop = Arc::new(WorkerStop::default());
        let worker_stop = Arc::clone(&stop);
        let worker_published = Arc::clone(&published);
        let worker = thread::Builder::new()
            .name(format!("hosted-terminal-{terminal_id}"))
            .spawn(move || {
                worker_stop.register_current_thread();
                client_loop(
                    &socket,
                    terminal_id,
                    intent,
                    command_rx,
                    event_tx,
                    worker_published,
                    worker_stop,
                );
            })
            .map_err(HostedTerminalStartError::Worker)?;

        Ok(Self {
            inner: Arc::new(HostedHandleInner {
                terminal_id,
                commands: command_tx,
                events: event_rx,
                published,
                stop,
                worker: Mutex::new(Some(worker)),
            }),
        })
    }

    pub fn terminal_id(&self) -> TerminalId {
        self.inner.terminal_id
    }

    /// Returns the fenced host identity only after the daemon accepted this terminal.
    pub fn proof(&self) -> Option<HostSessionProof> {
        let confirmed = self
            .inner
            .published
            .attachment
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .confirmed;
        confirmed.then(|| self.inner.published.proof()).flatten()
    }

    /// Waits for a successful terminal attach, spawn, or reconnect resume.
    /// Semantic events remain queued for the UI; authenticating the daemon alone
    /// does not confirm that it accepted this terminal.
    pub fn wait_until_attached(&self, timeout: Duration) -> Result<(), String> {
        self.inner.published.wait_until_attached(timeout)
    }

    pub fn snapshot(&self) -> Arc<RenderSnapshot> {
        self.inner.published.snapshot.load_full()
    }

    pub fn activity(&self) -> Option<crate::model::TerminalOutputActivity> {
        self.inner.published.activity()
    }

    pub fn agent_activity(&self) -> Option<crate::agent::Activity> {
        *self
            .inner
            .published
            .agent_activity
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }

    pub fn refresh_agent_activity(&self) -> Result<(), TerminalSendError> {
        let pending = &self.inner.published.agent_refresh_pending;
        if pending.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        if let Err(error) = self.try_command(ClientCommand::RefreshAgentActivity) {
            pending.store(false, Ordering::Release);
            return Err(error);
        }
        Ok(())
    }

    pub fn generation(&self) -> u64 {
        self.inner.published.generation.load(Ordering::Acquire)
    }

    pub fn acknowledge_redraw(&self, observed_generation: u64) {
        self.inner.published.acknowledge(observed_generation);
    }

    pub fn take_observed_output(&self) -> bool {
        self.inner.published.take_observed_output()
    }

    /// Acknowledge the scope even if this terminal never became onscreen.
    pub fn acknowledge_updates(&self) -> Result<(), TerminalSendError> {
        let mut observation = self
            .inner
            .published
            .output_observation
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let epoch = observation.epoch.wrapping_add(1);
        self.try_command(ClientCommand::AcknowledgeOutput(epoch))?;
        observation.epoch = epoch;
        observation.pending = false;
        Ok(())
    }

    pub fn events(&self) -> Receiver<TerminalEvent> {
        self.inner.published.begin_event_drain();
        self.inner.events.clone()
    }

    pub fn input(&self, bytes: impl Into<Vec<u8>>) -> Result<(), TerminalSendError> {
        self.try_command(ClientCommand::Input(bytes.into()))
    }

    pub fn paste(
        &self,
        text: impl Into<String>,
        origin: PasteOrigin,
    ) -> Result<(), TerminalSendError> {
        let text = text.into();
        if text.len() > MAX_INPUT_BYTES {
            return Err(TerminalSendError::PasteTooLarge(text.len()));
        }
        self.try_command(ClientCommand::Paste(text, origin))
    }

    pub fn key(&self, key: KeyInput) -> Result<(), TerminalSendError> {
        self.try_command(ClientCommand::Key(key))
    }

    pub fn mouse(&self, mouse: MouseInput) -> Result<(), TerminalSendError> {
        self.try_command(ClientCommand::Mouse(mouse))
    }

    pub fn focus(&self, focused: bool) -> Result<(), TerminalSendError> {
        self.try_command(ClientCommand::Focus(focused))
    }

    pub fn resize(&self, size: TerminalSize) -> Result<(), TerminalSendError> {
        self.try_command(ClientCommand::Resize(size))
    }

    pub fn scroll_lines(&self, delta: isize) -> Result<(), TerminalSendError> {
        self.try_command(ClientCommand::Scroll(TerminalScroll::Lines(delta as i64)))
    }

    pub fn scroll_to_top(&self) -> Result<(), TerminalSendError> {
        self.try_command(ClientCommand::Scroll(TerminalScroll::Top))
    }

    pub fn scroll_to_bottom(&self) -> Result<(), TerminalSendError> {
        self.try_command(ClientCommand::Scroll(TerminalScroll::Bottom))
    }

    pub fn scroll_to_row(&self, row: u64) -> Result<(), TerminalSendError> {
        self.try_command(ClientCommand::Scroll(TerminalScroll::Row(row)))
    }

    pub fn set_theme(&self, theme: super::TerminalTheme) -> Result<(), TerminalSendError> {
        self.try_command(ClientCommand::SetTheme(theme))
    }

    pub fn hide(&self) -> Result<(), TerminalSendError> {
        self.try_command(ClientCommand::Visibility(TerminalVisibility::Hidden))?;
        self.inner.published.set_visible(false);
        Ok(())
    }

    pub fn reveal(&self) -> Result<(), TerminalSendError> {
        let previous = self.inner.published.desired_visibility();
        // A visible peer may let the daemon answer immediately with its retained
        // grid. Open the local gate before that response can reach this client.
        self.inner.published.set_visible(true);
        if let Err(error) = self.try_command(ClientCommand::Visibility(TerminalVisibility::Visible))
        {
            self.inner.published.set_visibility(previous);
            return Err(error);
        }
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn visibility(&self) -> TerminalVisibility {
        self.inner.published.desired_visibility()
    }

    pub fn hibernate(&self) -> Result<(), TerminalSendError> {
        self.try_command(ClientCommand::Hibernate)?;
        self.inner.published.hibernate();
        Ok(())
    }

    pub fn restore(&self, visible: bool) -> Result<(), TerminalSendError> {
        let previous = self.inner.published.desired_visibility();
        self.inner.published.set_visible(visible);
        if let Err(error) = self.try_command(ClientCommand::Resume { visible }) {
            self.inner.published.set_visibility(previous);
            return Err(error);
        }
        Ok(())
    }

    pub fn shutdown(&self) -> Result<(), TerminalSendError> {
        let (completed_tx, completed_rx) = crossbeam_channel::bounded(1);
        self.try_command(ClientCommand::ShutdownTerminal(completed_tx))?;
        completed_rx
            .recv_timeout(Duration::from_secs(5))
            .map_err(|_| TerminalSendError::Closed)
    }

    fn try_command(&self, command: ClientCommand) -> Result<(), TerminalSendError> {
        if self.inner.stop.is_requested() {
            return Err(TerminalSendError::Closed);
        }
        self.inner
            .commands
            .try_send(command)
            .map_err(|error| match error {
                TrySendError::Full(_) => TerminalSendError::Full,
                TrySendError::Disconnected(_) => TerminalSendError::Closed,
            })
    }
}

#[derive(Clone, Debug)]
enum ConnectionIntent {
    Attach,
    Spawn(Box<SpawnIntent>),
}

#[derive(Clone, Debug)]
struct SpawnIntent {
    session: TerminalSession,
    options: TerminalSpawnOptions,
}

#[derive(Debug)]
enum PendingRequest {
    Attach,
    Spawn,
    Snapshot,
    Acknowledge,
    Command(&'static str),
    Hibernate,
    Shutdown(Sender<()>),
    AgentActivity(u64),
}
/// Returns the per-user daemon socket, starting the sibling `cagentsd` if necessary.
///
/// Ordinary startup is bounded to five seconds; upgrade restoration receives a
/// bounded per-session allowance. The daemon gets its own process group so closing
/// the launching CLI, GUI, or terminal does not terminate it.
pub fn ensure_daemon_ready() -> Result<PathBuf, HostedTerminalStartError> {
    let socket = daemon_socket_path().map_err(HostedTerminalStartError::SocketPath)?;
    ensure_daemon_ready_at(&socket)?;
    Ok(socket)
}

fn ensure_daemon_ready_at(socket: &Path) -> Result<(), HostedTerminalStartError> {
    match UnixStream::connect(socket) {
        Ok(stream) => {
            drop(stream);
            return Ok(());
        }
        Err(error)
            if !matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
            ) =>
        {
            return Err(HostedTerminalStartError::HostSocket {
                path: socket.to_owned(),
                source: error,
            });
        }
        Err(_) => {}
    }

    let timeout = crate::upgrade::handoff::startup_timeout()
        .map_err(HostedTerminalStartError::HostStartup)?;
    let current_exe =
        std::env::current_exe().map_err(HostedTerminalStartError::CurrentExecutable)?;
    let Some(binary_directory) = current_exe.parent() else {
        return Err(HostedTerminalStartError::HostBinaryMissing(current_exe));
    };
    let daemon_binary = binary_directory.join(format!("cagentsd{}", std::env::consts::EXE_SUFFIX));
    if !daemon_binary.is_file() {
        return Err(HostedTerminalStartError::HostBinaryMissing(daemon_binary));
    }

    let mut command = Command::new(&daemon_binary);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        // This process may itself have been started with the UI launch
        // handshake set. Inheriting it would make the daemon, and every child
        // the tray spawns from it, emit a stray readiness line on stdout.
        .env_remove(crate::instance::UI_LAUNCH_HANDSHAKE_ENV);
    // SAFETY: the hook performs only the close_range syscall before exec.
    unsafe {
        command.pre_exec(crate::daemon::seal_inherited_descriptors);
    }
    let mut child = command
        .spawn()
        .map_err(|source| HostedTerminalStartError::LaunchHost {
            path: daemon_binary,
            source,
        })?;
    let started = Instant::now();
    let result = loop {
        let status = match child.try_wait() {
            Ok(status) => status,
            Err(error) => break Err(HostedTerminalStartError::HostStartup(error.to_string())),
        };
        match daemon_startup_ready(socket, timeout, started.elapsed(), status) {
            Ok(true) => break Ok(()),
            Ok(false) => thread::sleep(
                DAEMON_READY_POLL_INTERVAL.min(timeout.saturating_sub(started.elapsed())),
            ),
            Err(error) => break Err(error),
        }
    };
    let _ = thread::Builder::new()
        .name("cagentsd-reaper".into())
        .spawn(move || {
            let _ = child.wait();
        });
    result
}

fn daemon_startup_ready(
    socket: &Path,
    timeout: Duration,
    elapsed: Duration,
    child_status: Option<ExitStatus>,
) -> Result<bool, HostedTerminalStartError> {
    match UnixStream::connect(socket) {
        Ok(_) => return Ok(true),
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
            ) => {}
        Err(source) => {
            return Err(HostedTerminalStartError::HostSocket {
                path: socket.to_owned(),
                source,
            });
        }
    }
    if let Some(status) = child_status {
        // A concurrent launch can win the lease while our child exits. Its
        // listener is deliberately absent until every session has restored.
        if !crate::daemon::HostLease::is_held_at(socket)
            .map_err(|error| HostedTerminalStartError::HostStartup(error.to_string()))?
        {
            return Err(HostedTerminalStartError::HostExited {
                path: socket.to_owned(),
                status,
            });
        }
    }
    if elapsed >= timeout {
        return Err(HostedTerminalStartError::HostReadyTimeout {
            path: socket.to_owned(),
            timeout,
        });
    }
    Ok(false)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ConnectionEnd {
    Retry,
    Stop,
}

#[derive(Debug)]
enum ServerRead {
    Envelope(Box<ServerEnvelope>),
    Closed,
}

/// One blocking reader owns the socket clone. The client loop selects between
/// its bounded inbox and the command channel, so a quiet daemon can never delay
/// keyboard input. The bounded channel is allocated once per connection rather
/// than once per frame.
struct ConnectionIo {
    writer: UnixStream,
    codec: FrameCodec,
    incoming: Option<Receiver<ServerRead>>,
    reader: Option<JoinHandle<()>>,
}

impl ConnectionIo {
    fn new(writer: UnixStream, terminal_id: TerminalId) -> io::Result<Self> {
        writer.set_read_timeout(None)?;
        let mut reader_stream = writer.try_clone()?;
        let (incoming_tx, incoming_rx) = crossbeam_channel::bounded(EVENT_CAPACITY);
        let reader = thread::Builder::new()
            .name(format!("hosted-terminal-reader-{terminal_id}"))
            .spawn(move || {
                let mut codec = FrameCodec::new(MAX_FRAME_BYTES);
                loop {
                    let message = match codec.read::<ServerEnvelope>(&mut reader_stream) {
                        Ok(Some(envelope)) => ServerRead::Envelope(Box::new(envelope)),
                        Ok(None) | Err(_) => ServerRead::Closed,
                    };
                    let closed = matches!(message, ServerRead::Closed);
                    if incoming_tx.send(message).is_err() || closed {
                        return;
                    }
                }
            })?;
        Ok(Self {
            writer,
            codec: FrameCodec::new(MAX_FRAME_BYTES),
            incoming: Some(incoming_rx),
            reader: Some(reader),
        })
    }

    fn writer(&mut self) -> (&mut UnixStream, &mut FrameCodec) {
        (&mut self.writer, &mut self.codec)
    }

    fn incoming(&self) -> &Receiver<ServerRead> {
        self.incoming
            .as_ref()
            .expect("connection inbox exists until close")
    }

    fn close(&mut self) {
        // Dropping the inbox releases a reader blocked on its bounded send;
        // shutting down the socket releases one blocked in `read_frame`.
        self.incoming.take();
        let _ = self.writer.shutdown(Shutdown::Both);
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

impl Drop for ConnectionIo {
    fn drop(&mut self) {
        self.close();
    }
}

#[derive(Debug)]
enum ClientWork {
    Stop,
    Acknowledge,
    Command(ClientCommand),
    Server(Box<ServerEnvelope>),
    CommandsClosed,
    ServerClosed,
}

fn wait_for_client_work(
    stop: &WorkerStop,
    commands: &Receiver<ClientCommand>,
    incoming: &Receiver<ServerRead>,
    acknowledgements: &Receiver<()>,
) -> ClientWork {
    crossbeam_channel::select_biased! {
        recv(stop.notification()) -> _ => ClientWork::Stop,
        recv(acknowledgements) -> _ => ClientWork::Acknowledge,
        recv(commands) -> command => match command {
            Ok(command) => ClientWork::Command(command),
            Err(_) => ClientWork::CommandsClosed,
        },
        recv(incoming) -> message => server_work(message),
    }
}

fn wait_for_server_or_stop(stop: &WorkerStop, incoming: &Receiver<ServerRead>) -> ClientWork {
    crossbeam_channel::select_biased! {
        recv(stop.notification()) -> _ => ClientWork::Stop,
        recv(incoming) -> message => server_work(message),
    }
}

fn server_work(message: Result<ServerRead, crossbeam_channel::RecvError>) -> ClientWork {
    match message {
        Ok(ServerRead::Envelope(envelope)) => ClientWork::Server(envelope),
        Ok(ServerRead::Closed) | Err(_) => ClientWork::ServerClosed,
    }
}

struct ConnectionContext<'a> {
    socket: &'a Path,
    terminal_id: TerminalId,
    intent: &'a ConnectionIntent,
    commands: &'a Receiver<ClientCommand>,
    events: &'a Sender<TerminalEvent>,
    published: &'a HostedPublishedState,
    stop: &'a WorkerStop,
}

fn client_loop(
    socket: &Path,
    terminal_id: TerminalId,
    intent: ConnectionIntent,
    commands: Receiver<ClientCommand>,
    events: Sender<TerminalEvent>,
    published: Arc<HostedPublishedState>,
    stop: Arc<WorkerStop>,
) {
    let mut reconnect_delay = INITIAL_RECONNECT_DELAY;
    let mut carried = None;
    let mut reconnect_started = None;
    let connection = ConnectionContext {
        socket,
        terminal_id,
        intent: &intent,
        commands: &commands,
        events: &events,
        published: &published,
        stop: &stop,
    };
    loop {
        if stop.is_requested() {
            break;
        }
        let end = serve_connection(&connection, &mut carried, || {
            reconnect_started = None;
            reconnect_delay = INITIAL_RECONNECT_DELAY;
        });
        published.invalidate_activity();
        published
            .agent_refresh_pending
            .store(false, Ordering::Release);
        let _ = published.observe_agent_activity(&events, None);
        match end {
            ConnectionEnd::Stop => break,
            ConnectionEnd::Retry => {
                let started = reconnect_started.get_or_insert_with(Instant::now);
                if started.elapsed() >= DAEMON_READY_TIMEOUT {
                    reject_handshake(
                        &events,
                        &published,
                        &stop,
                        ProtocolError::new(
                            ProtocolErrorCode::InvalidRequest,
                            "daemon connection was lost; rediscovery is required before this terminal can continue",
                            false,
                        ),
                    );
                    break;
                }
            }
        }

        if wait_before_reconnect(&stop, &commands, &mut carried, reconnect_delay)
            == ConnectionEnd::Stop
        {
            break;
        }
        reconnect_delay = (reconnect_delay * 2).min(MAX_RECONNECT_DELAY);
    }
    // Fence command submission before dropping the semantic sender. Argument
    // destruction otherwise disconnects `events` before `commands`, briefly
    // accepting input after consumers already observed permanent termination.
    stop.request();
    published.fail_attachment("hosted terminal connection stopped".to_owned());
}

fn wait_before_reconnect(
    stop: &WorkerStop,
    commands: &Receiver<ClientCommand>,
    carried: &mut Option<ClientCommand>,
    reconnect_delay: Duration,
) -> ConnectionEnd {
    if stop.is_requested() {
        return ConnectionEnd::Stop;
    }
    if carried.is_some() {
        return if stop.wait_timeout(reconnect_delay) {
            ConnectionEnd::Stop
        } else {
            ConnectionEnd::Retry
        };
    }
    match commands.recv_timeout(reconnect_delay) {
        Ok(ClientCommand::Stop) | Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
            ConnectionEnd::Stop
        }
        Ok(command) => {
            *carried = Some(command);
            if stop.is_requested() {
                ConnectionEnd::Stop
            } else {
                ConnectionEnd::Retry
            }
        }
        Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
            if stop.is_requested() {
                ConnectionEnd::Stop
            } else {
                ConnectionEnd::Retry
            }
        }
    }
}
fn coalesce_client_scroll_command(
    command: ClientCommand,
    commands: &Receiver<ClientCommand>,
    carried: &mut Option<ClientCommand>,
) -> ClientCommand {
    let ClientCommand::Scroll(TerminalScroll::Lines(mut delta)) = command else {
        return command;
    };
    while let Ok(next) = commands.try_recv() {
        match next {
            ClientCommand::Scroll(TerminalScroll::Lines(next_delta)) => {
                delta = delta.saturating_add(next_delta);
            }
            other => {
                *carried = Some(other);
                break;
            }
        }
    }
    ClientCommand::Scroll(TerminalScroll::Lines(delta))
}

fn serve_connection(
    context: &ConnectionContext<'_>,
    carried: &mut Option<ClientCommand>,
    mut did_attach: impl FnMut(),
) -> ConnectionEnd {
    let ConnectionContext {
        socket,
        terminal_id,
        intent,
        commands,
        events,
        published,
        stop,
    } = context;
    let terminal_id = *terminal_id;
    if stop.is_requested() {
        return ConnectionEnd::Stop;
    }
    let mut stream = match UnixStream::connect(socket) {
        Ok(stream) => stream,
        Err(_) => return ConnectionEnd::Retry,
    };
    let _connection = match stop.register_connection(&stream) {
        Ok(registration) => registration,
        Err(_) => return ConnectionEnd::Retry,
    };
    if stream
        .set_read_timeout(Some(HANDSHAKE_POLL_INTERVAL))
        .is_err()
        || stream
            .set_write_timeout(Some(Duration::from_secs(5)))
            .is_err()
    {
        return ConnectionEnd::Retry;
    }

    let connect_id = 1;
    let connect = ClientEnvelope {
        version: PROTOCOL_VERSION,
        request_id: connect_id,
        proof: None,
        request: ClientRequest::Connect {
            expected_host: published.proof(),
            resume: vec![ResumeCursor {
                terminal_id,
                generation: published.snapshot_cursor(),
                event_sequence: published.event_sequence.load(Ordering::Acquire),
            }],
        },
    };
    if write_frame(&mut stream, &connect).is_err() {
        return ConnectionEnd::Retry;
    }

    let handshake_deadline = Instant::now() + DAEMON_READY_TIMEOUT;
    let (proof, mut attached) = loop {
        if stop.is_requested() {
            return ConnectionEnd::Stop;
        }
        if Instant::now() >= handshake_deadline {
            return ConnectionEnd::Retry;
        }
        let envelope = match read_frame::<ServerEnvelope>(&mut stream) {
            Ok(Some(envelope)) => envelope,
            Ok(None) => return ConnectionEnd::Retry,
            Err(FrameError::Io(error)) if is_timeout(&error) => {
                if carried.is_none() {
                    match commands.try_recv() {
                        Ok(ClientCommand::Stop) | Err(TryRecvError::Disconnected) => {
                            return ConnectionEnd::Stop;
                        }
                        Ok(command) => *carried = Some(command),
                        Err(TryRecvError::Empty) => {}
                    }
                }
                continue;
            }
            Err(error) => {
                // The codec changed with the protocol version, so a frame from
                // a daemon of another version is undecodable rather than merely
                // unexpected. Without this the client retries forever against a
                // mismatched daemon with nothing in the log to explain it.
                tracing::warn!(
                    %error,
                    expected_version = PROTOCOL_VERSION,
                    "could not decode a daemon frame; the daemon may speak another protocol version"
                );
                return ConnectionEnd::Retry;
            }
        };
        if envelope.version != PROTOCOL_VERSION {
            tracing::warn!(
                actual = envelope.version,
                expected = PROTOCOL_VERSION,
                "the daemon speaks an unsupported protocol version"
            );
            return ConnectionEnd::Retry;
        }
        // The daemon can refuse or sever a connection before it ever reads the
        // connect frame — client capacity, for one — and that unsolicited error
        // carries the only accurate reason this handle will ever see.
        let unsolicited = envelope.request_id.is_none();
        if envelope.request_id != Some(connect_id) {
            if unsolicited && let ServerMessage::Error(error) = envelope.message {
                return reject_handshake(events, published, stop, error);
            }
            return ConnectionEnd::Retry;
        }
        match envelope.message {
            ServerMessage::Response {
                response: ServerResponse::Connected { proof, resumed },
            } => {
                if envelope.proof != proof || !published.pin_proof(&proof) {
                    return reject_handshake(
                        events,
                        published,
                        stop,
                        ProtocolError::new(
                            ProtocolErrorCode::ProofMismatch,
                            "the daemon returned a different or incomplete session proof",
                            false,
                        ),
                    );
                }
                let attached = resumed
                    .iter()
                    .any(|terminal| terminal.session.id == terminal_id && terminal.attached);
                if let Some(terminal) = resumed
                    .iter()
                    .find(|terminal| terminal.session.id == terminal_id && terminal.attached)
                {
                    published.observe_activity(terminal.activity);
                    if published
                        .observe_agent_activity(events, terminal.agent_activity)
                        .is_err()
                    {
                        return ConnectionEnd::Retry;
                    }
                    published.confirm_attachment();
                    did_attach();
                }
                break (proof, attached);
            }
            ServerMessage::Error(error) => {
                return reject_handshake(events, published, stop, error);
            }
            _ => return ConnectionEnd::Retry,
        }
    };

    let mut connection = match ConnectionIo::new(stream, terminal_id) {
        Ok(connection) => connection,
        Err(_) => return ConnectionEnd::Retry,
    };

    let mut next_request_id = connect_id + 1;
    let mut pending = BTreeMap::new();
    let mut snapshot_pending = false;
    let mut last_acknowledgement = 0;

    if stop.is_requested() {
        connection.close();
        return ConnectionEnd::Stop;
    }

    if !attached {
        let (kind, request) = match intent {
            ConnectionIntent::Attach => (
                PendingRequest::Attach,
                ClientRequest::Attach {
                    terminal_id,
                    after_generation: published.snapshot_cursor(),
                    after_event_sequence: published.event_sequence.load(Ordering::Acquire),
                },
            ),
            ConnectionIntent::Spawn(spawn) => {
                let mut options = spawn.options.clone();
                options.initially_visible = published.is_visible();
                (
                    PendingRequest::Spawn,
                    ClientRequest::Spawn {
                        session: spawn.session.clone(),
                        options,
                    },
                )
            }
        };
        if send_request(
            connection.writer(),
            &proof,
            &mut next_request_id,
            &mut pending,
            kind,
            request,
        )
        .is_err()
        {
            return ConnectionEnd::Retry;
        }
    }

    if stop.is_requested() {
        connection.close();
        return ConnectionEnd::Stop;
    }

    if send_request(
        connection.writer(),
        &proof,
        &mut next_request_id,
        &mut pending,
        PendingRequest::Command("restore hosted terminal visibility"),
        ClientRequest::SetVisibility {
            terminal_id,
            visibility: published.desired_visibility(),
        },
    )
    .is_err()
    {
        return ConnectionEnd::Retry;
    }
    let epoch = published
        .output_observation
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .epoch;
    if epoch != 0
        && send_request(
            connection.writer(),
            &proof,
            &mut next_request_id,
            &mut pending,
            PendingRequest::Command("restore terminal output observation"),
            ClientRequest::TerminalAcknowledge { terminal_id, epoch },
        )
        .is_err()
    {
        return ConnectionEnd::Retry;
    }

    loop {
        if stop.is_requested() {
            connection.close();
            return ConnectionEnd::Stop;
        }
        while pending.len() < MAX_IN_FLIGHT_REQUESTS {
            if stop.is_requested() {
                connection.close();
                return ConnectionEnd::Stop;
            }
            let command = if let Some(command) = carried.take() {
                Ok(command)
            } else {
                commands.try_recv()
            };
            let mut command = match command {
                Ok(command) => command,
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => ClientCommand::Stop,
            };
            command = coalesce_client_scroll_command(command, commands, carried);
            if stop.is_requested() {
                connection.close();
                return ConnectionEnd::Stop;
            }
            if matches!(command, ClientCommand::Stop) {
                connection.close();
                return ConnectionEnd::Stop;
            }
            let operation = command.operation();
            let pending_kind = match &command {
                ClientCommand::RefreshAgentActivity => {
                    // A reconnect can retain one queued command while a poll
                    // is racing it; keep only one request on this connection.
                    if pending
                        .values()
                        .any(|kind| matches!(kind, PendingRequest::AgentActivity(_)))
                    {
                        continue;
                    }
                    published
                        .agent_refresh_pending
                        .store(true, Ordering::Release);
                    PendingRequest::AgentActivity(published.agent_revision.load(Ordering::Acquire))
                }
                ClientCommand::Hibernate => PendingRequest::Hibernate,
                ClientCommand::ShutdownTerminal(completed) => {
                    PendingRequest::Shutdown(completed.clone())
                }
                _ => PendingRequest::Command(operation),
            };
            if send_request(
                connection.writer(),
                &proof,
                &mut next_request_id,
                &mut pending,
                pending_kind,
                command.into_request(terminal_id),
            )
            .is_err()
            {
                return ConnectionEnd::Retry;
            }
        }

        if attached && pending.len() < MAX_IN_FLIGHT_REQUESTS {
            let acknowledged = published.acknowledged_generation.load(Ordering::Acquire);
            if acknowledged > last_acknowledgement {
                if send_request(
                    connection.writer(),
                    &proof,
                    &mut next_request_id,
                    &mut pending,
                    PendingRequest::Acknowledge,
                    ClientRequest::AcknowledgeRedraw {
                        terminal_id,
                        generation: acknowledged,
                    },
                )
                .is_err()
                {
                    return ConnectionEnd::Retry;
                }
                last_acknowledgement = acknowledged;
            }
        }

        let work = if pending.len() < MAX_IN_FLIGHT_REQUESTS {
            wait_for_client_work(
                stop,
                commands,
                connection.incoming(),
                &published.acknowledgement_rx,
            )
        } else {
            wait_for_server_or_stop(stop, connection.incoming())
        };
        let envelope = match work {
            ClientWork::Acknowledge => continue,
            ClientWork::Stop => {
                connection.close();
                return ConnectionEnd::Stop;
            }
            ClientWork::Command(command) => {
                *carried = Some(command);
                continue;
            }
            ClientWork::Server(envelope) => envelope,
            ClientWork::CommandsClosed => {
                connection.close();
                return ConnectionEnd::Stop;
            }
            ClientWork::ServerClosed => return ConnectionEnd::Retry,
        };
        if envelope.version != PROTOCOL_VERSION || envelope.proof != proof {
            return ConnectionEnd::Retry;
        }

        match envelope.message {
            ServerMessage::Response { response } => {
                let Some(request_id) = envelope.request_id else {
                    return ConnectionEnd::Retry;
                };
                let Some(kind) = pending.remove(&request_id) else {
                    return ConnectionEnd::Retry;
                };
                match (kind, response) {
                    (
                        PendingRequest::AgentActivity(revision),
                        ServerResponse::AgentActivity {
                            terminal_id: response_id,
                            activity,
                        },
                    ) if response_id == terminal_id => {
                        published
                            .agent_refresh_pending
                            .store(false, Ordering::Release);
                        if revision == published.agent_revision.load(Ordering::Acquire)
                            && published.observe_agent_activity(events, activity).is_err()
                        {
                            return ConnectionEnd::Retry;
                        }
                    }
                    (PendingRequest::Attach, ServerResponse::Attached { terminal })
                        if terminal.session.id == terminal_id =>
                    {
                        attached = true;
                        published.observe_activity(terminal.activity);
                        if published
                            .observe_agent_activity(events, terminal.agent_activity)
                            .is_err()
                        {
                            return ConnectionEnd::Retry;
                        }
                        if events
                            .try_send(TerminalEvent::ResumeHintChanged(
                                terminal.session.resume_hint,
                            ))
                            .is_err()
                        {
                            return ConnectionEnd::Retry;
                        }
                        published.request_semantic_wake();
                        published.confirm_attachment();
                        did_attach();
                    }
                    (PendingRequest::Spawn, ServerResponse::Spawned { terminal })
                        if terminal.session.id == terminal_id =>
                    {
                        attached = true;
                        published.observe_activity(terminal.activity);
                        if published
                            .observe_agent_activity(events, terminal.agent_activity)
                            .is_err()
                        {
                            return ConnectionEnd::Retry;
                        }
                        if events
                            .try_send(TerminalEvent::ResumeHintChanged(
                                terminal.session.resume_hint,
                            ))
                            .is_err()
                        {
                            return ConnectionEnd::Retry;
                        }
                        published.request_semantic_wake();
                        published.confirm_attachment();
                        did_attach();
                    }
                    (
                        PendingRequest::Snapshot,
                        ServerResponse::Snapshot {
                            terminal_id: response_id,
                            snapshot,
                        },
                    ) if response_id == terminal_id => {
                        snapshot_pending = false;
                        if let Some(snapshot) = snapshot
                            && !published.apply(snapshot)
                        {
                            // The delta base is gone. Re-ask from scratch;
                            // `after_generation: 0` always yields a full grid.
                            if send_request(
                                connection.writer(),
                                &proof,
                                &mut next_request_id,
                                &mut pending,
                                PendingRequest::Snapshot,
                                ClientRequest::Snapshot {
                                    terminal_id,
                                    after_generation: 0,
                                },
                            )
                            .is_err()
                            {
                                return ConnectionEnd::Retry;
                            }
                            snapshot_pending = true;
                        }
                    }
                    (PendingRequest::Acknowledge, ServerResponse::Accepted)
                    | (PendingRequest::Command(_), ServerResponse::Accepted) => {}
                    (PendingRequest::Hibernate, ServerResponse::Accepted) => {}
                    (PendingRequest::Shutdown(completed), ServerResponse::Accepted) => {
                        let _ = completed.try_send(());
                    }
                    (PendingRequest::Command(operation), unexpected) => {
                        publish_event(
                            events,
                            published,
                            TerminalEvent::Error {
                                operation,
                                message: format!(
                                    "unexpected cagents daemon response: {unexpected:?}"
                                ),
                            },
                        );
                    }
                    _ => return ConnectionEnd::Retry,
                }
            }
            ServerMessage::Error(error) => {
                if is_committed_daemon_shutdown(&error) {
                    // In-flight controls can reach the old daemon after its
                    // checkpoint commits. Keep the runtime alive until the
                    // reconnect handshake observes the replacement proof and
                    // hands off to the UI's ownership-checked recovery.
                    return ConnectionEnd::Retry;
                }
                let Some(request_id) = envelope.request_id else {
                    // The daemon severed this connection on its own — it fell
                    // behind its delivery queue, or the daemon is at capacity.
                    // The frame carries the only accurate reason the UI can
                    // report; without it a drop looks like a dead daemon.
                    let retryable = error.retryable;
                    if !retryable {
                        published.fail_attachment(error.message.clone());
                    }
                    publish_protocol_error(events, published, "cagents daemon connection", error);
                    return if retryable {
                        ConnectionEnd::Retry
                    } else {
                        ConnectionEnd::Stop
                    };
                };
                let mut attachment_failed = false;
                let operation = match pending.remove(&request_id) {
                    Some(PendingRequest::AgentActivity(_)) => {
                        published
                            .agent_refresh_pending
                            .store(false, Ordering::Release);
                        if published.observe_agent_activity(events, None).is_err() {
                            return ConnectionEnd::Retry;
                        }
                        "refresh terminal agent activity"
                    }
                    Some(PendingRequest::Attach) => {
                        attachment_failed = true;
                        published.mark_detached();
                        "attach hosted terminal"
                    }
                    Some(PendingRequest::Spawn) => {
                        attachment_failed = true;
                        published.mark_detached();
                        "spawn hosted terminal"
                    }
                    Some(PendingRequest::Snapshot) => {
                        snapshot_pending = false;
                        "fetch hosted terminal snapshot"
                    }
                    Some(PendingRequest::Acknowledge) => "acknowledge hosted terminal redraw",
                    Some(PendingRequest::Hibernate) => "hibernate terminal",
                    Some(PendingRequest::Shutdown(_)) => "shutdown terminal",
                    Some(PendingRequest::Command(operation)) => operation,
                    None => return ConnectionEnd::Retry,
                };
                let retry = error.retryable && error.code != ProtocolErrorCode::ProofMismatch;
                let fatal =
                    !retry && (attachment_failed || error.code == ProtocolErrorCode::ProofMismatch);
                if fatal {
                    published.fail_attachment(format!("{operation}: {}", error.message));
                }
                publish_protocol_error(events, published, operation, error);
                if fatal {
                    return ConnectionEnd::Stop;
                }
                if retry {
                    return ConnectionEnd::Retry;
                }
            }
            ServerMessage::TerminalObserved {
                terminal_id: observed_id,
                epoch,
            } if envelope.request_id.is_none() && observed_id == terminal_id => {
                published.observe_output(epoch);
            }
            ServerMessage::Redraw {
                terminal_id: redraw_id,
                generation,
            } if envelope.request_id.is_none() && redraw_id == terminal_id => {
                if published.is_visible()
                    && generation > published.snapshot_cursor()
                    && !snapshot_pending
                {
                    if send_request(
                        connection.writer(),
                        &proof,
                        &mut next_request_id,
                        &mut pending,
                        PendingRequest::Snapshot,
                        ClientRequest::Snapshot {
                            terminal_id,
                            after_generation: published.snapshot_cursor(),
                        },
                    )
                    .is_err()
                    {
                        return ConnectionEnd::Retry;
                    }
                    snapshot_pending = true;
                }
            }
            ServerMessage::Snapshot {
                terminal_id: snapshot_id,
                snapshot,
            } if envelope.request_id.is_none() && snapshot_id == terminal_id => {
                if !published.apply(snapshot) && !snapshot_pending {
                    if send_request(
                        connection.writer(),
                        &proof,
                        &mut next_request_id,
                        &mut pending,
                        PendingRequest::Snapshot,
                        ClientRequest::Snapshot {
                            terminal_id,
                            after_generation: 0,
                        },
                    )
                    .is_err()
                    {
                        return ConnectionEnd::Retry;
                    }
                    snapshot_pending = true;
                }
            }
            ServerMessage::Event {
                terminal_id: event_id,
                sequence,
                event,
            } if envelope.request_id.is_none() && event_id == terminal_id => {
                match accept_server_event(events, published, sequence, event.into()) {
                    Ok(()) => {}
                    Err(TrySendError::Full(_)) => return ConnectionEnd::Retry,
                    Err(TrySendError::Disconnected(_)) => return ConnectionEnd::Stop,
                }
            }
            ServerMessage::Synchronized {
                terminal_id: synchronized_id,
                generation,
                event_sequence,
            } if envelope.request_id.is_none() && synchronized_id == terminal_id => {
                published
                    .event_sequence
                    .fetch_max(event_sequence, Ordering::AcqRel);
                if published.is_visible()
                    && generation > published.snapshot_cursor()
                    && !snapshot_pending
                {
                    if send_request(
                        connection.writer(),
                        &proof,
                        &mut next_request_id,
                        &mut pending,
                        PendingRequest::Snapshot,
                        ClientRequest::Snapshot {
                            terminal_id,
                            after_generation: published.snapshot_cursor(),
                        },
                    )
                    .is_err()
                    {
                        return ConnectionEnd::Retry;
                    }
                    snapshot_pending = true;
                }
            }
            ServerMessage::HostShuttingDown if envelope.request_id.is_none() => {
                return ConnectionEnd::Retry;
            }
            _ => return ConnectionEnd::Retry,
        }
    }
}

fn send_request(
    (stream, codec): (&mut UnixStream, &mut FrameCodec),
    proof: &HostSessionProof,
    next_request_id: &mut u64,
    pending: &mut BTreeMap<u64, PendingRequest>,
    kind: PendingRequest,
    request: ClientRequest,
) -> Result<(), FrameError> {
    let request_id = *next_request_id;
    *next_request_id = (*next_request_id).wrapping_add(1).max(2);
    codec.write(
        stream,
        &ClientEnvelope {
            version: PROTOCOL_VERSION,
            request_id,
            proof: Some(proof.clone()),
            request,
        },
    )?;
    pending.insert(request_id, kind);
    Ok(())
}

fn is_timeout(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut | io::ErrorKind::Interrupted
    )
}

fn accept_server_event(
    events: &Sender<TerminalEvent>,
    published: &HostedPublishedState,
    sequence: u64,
    event: TerminalEvent,
) -> Result<(), TrySendError<TerminalEvent>> {
    if sequence <= published.event_sequence.load(Ordering::Acquire) {
        return Ok(());
    }
    match event {
        TerminalEvent::AgentActivityChanged(activity) => {
            published.agent_revision.fetch_add(1, Ordering::AcqRel);
            published.observe_agent_activity(events, activity)?;
        }
        event => {
            if matches!(
                event,
                TerminalEvent::ResumeHintChanged(_)
                    | TerminalEvent::Exited { .. }
                    | TerminalEvent::Completed { .. }
                    | TerminalEvent::Error { .. }
            ) {
                published.agent_revision.fetch_add(1, Ordering::AcqRel);
                published.observe_agent_activity(events, None)?;
            }
            events.try_send(event)?;
        }
    }
    published.event_sequence.store(sequence, Ordering::Release);
    published.request_semantic_wake();
    Ok(())
}

fn is_committed_daemon_shutdown(error: &ProtocolError) -> bool {
    // Installed daemons report this with retryable=false: that incarnation
    // will never accept controls again. It is nevertheless transient for the
    // UI. Match only this specific refusal, not other unavailable terminals,
    // and retain compatibility without changing the wire protocol mid-upgrade.
    error.code == ProtocolErrorCode::TerminalUnavailable
        && error.message
            == "Daemon upgrade/shutdown is committed; no further control mutations are accepted"
}

fn reject_handshake(
    events: &Sender<TerminalEvent>,
    published: &HostedPublishedState,
    stop: &WorkerStop,
    error: ProtocolError,
) -> ConnectionEnd {
    if is_committed_daemon_shutdown(&error) {
        return ConnectionEnd::Retry;
    }
    // A reconnect can recover transport failures, but this handle may never
    // target another incarnation, even when the protocol marks it retryable.
    let retry = error.retryable && error.code != ProtocolErrorCode::ProofMismatch;
    if retry {
        publish_protocol_error(events, published, "connect to cagents daemon", error);
        return ConnectionEnd::Retry;
    }

    published.fail_attachment(error.message.clone());
    // The terminal cannot recover after this worker exits. Preserve its final
    // failure even when earlier events fill the inbox, but let dropping the
    // handle cancel the wait. Wake the consumer before applying backpressure.
    published.request_semantic_wake();
    if stop.is_requested() {
        return ConnectionEnd::Stop;
    }
    let event = TerminalEvent::Error {
        operation: "connect to cagents daemon",
        message: error.message,
    };
    crossbeam_channel::select_biased! {
        recv(stop.notification()) -> _ => {}
        send(events, event) -> sent => {
            if sent.is_ok() {
                published.request_semantic_wake();
            }
        }
    }
    ConnectionEnd::Stop
}

fn publish_protocol_error(
    events: &Sender<TerminalEvent>,
    published: &HostedPublishedState,
    operation: &'static str,
    error: ProtocolError,
) {
    publish_event(
        events,
        published,
        TerminalEvent::Error {
            operation,
            message: error.message,
        },
    );
}

fn publish_event(
    events: &Sender<TerminalEvent>,
    published: &HostedPublishedState,
    event: TerminalEvent,
) {
    if events.try_send(event).is_ok() {
        published.request_semantic_wake();
    }
}

#[cfg(test)]
mod tests {
    use std::{os::unix::net::UnixListener, sync::atomic::AtomicUsize};

    use crate::{
        model::{BootId, HostIncarnation, HostSessionId, LaunchRecipe},
        terminal::ipc::TerminalDescriptor,
    };

    use super::*;
    #[test]
    fn stopping_an_attachment_interrupts_blocked_socket_io() {
        let stop = Arc::new(WorkerStop::default());
        let (mut client, _server) = UnixStream::pair().unwrap();
        let registration = stop.register_connection(&client).unwrap();
        let (started_tx, started_rx) = crossbeam_channel::bounded(1);
        let (finished_tx, finished_rx) = crossbeam_channel::bounded(1);
        let worker = thread::spawn(move || {
            use std::io::Write;
            started_tx.send(()).unwrap();
            let result = client.write_all(&vec![0; 4 * 1024 * 1024]);
            finished_tx.send(result.is_err()).unwrap();
        });
        started_rx.recv().unwrap();
        stop.request();
        assert!(finished_rx.recv_timeout(Duration::from_secs(1)).unwrap());
        worker.join().unwrap();
        drop(registration);
        assert!(stop.connection.lock().unwrap().is_none());
        let (client, _server) = UnixStream::pair().unwrap();
        let _registration = stop.register_connection(&client).unwrap();
        assert!(client.take_error().unwrap().is_none());
        use std::io::Read;
        assert_eq!((&client).read(&mut [0]).unwrap(), 0);
    }
    #[test]
    fn startup_waits_for_six_second_restoration_before_accepting_listener() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("daemon.sock");
        let timeout = Duration::from_secs(35);
        assert!(!daemon_startup_ready(&socket, timeout, Duration::from_secs(6), None).unwrap());
        let _listener = UnixListener::bind(&socket).unwrap();
        assert!(daemon_startup_ready(&socket, timeout, Duration::from_secs(6), None).unwrap());
    }

    fn hold_startup_lease(socket: &Path) -> std::fs::File {
        use std::os::{fd::AsRawFd, unix::fs::OpenOptionsExt};

        let lock = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(socket.with_file_name("cagentsd.lock"))
            .unwrap();
        // SAFETY: the fixture owns this live descriptor until the file drops.
        assert_eq!(
            unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0
        );
        lock
    }

    #[test]
    fn exited_launcher_waits_for_winners_sequential_restoration() {
        use std::os::unix::process::ExitStatusExt;

        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("daemon.sock");
        let _lease = hold_startup_lease(&socket);
        let timeout = Duration::from_secs(65);
        let loser = Some(ExitStatus::from_raw(1 << 8));
        // Two providers may each consume thirty seconds. The winner owns the
        // inventory throughout; even incomplete lease metadata is not failure.
        assert!(!daemon_startup_ready(&socket, timeout, Duration::from_secs(30), loser).unwrap());
        assert!(!daemon_startup_ready(&socket, timeout, Duration::from_secs(60), loser).unwrap());
        let _listener = UnixListener::bind(&socket).unwrap();
        assert!(daemon_startup_ready(&socket, timeout, Duration::from_secs(61), loser).unwrap());
    }

    #[test]
    fn exited_startup_fails_without_owner_and_owner_cannot_extend_deadline() {
        use std::os::unix::process::ExitStatusExt;

        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("daemon.sock");
        let timeout = Duration::from_secs(35);
        let exited = Some(ExitStatus::from_raw(1 << 8));
        assert!(matches!(
            daemon_startup_ready(&socket, timeout, Duration::from_secs(1), exited),
            Err(HostedTerminalStartError::HostExited { .. })
        ));
        let lease = hold_startup_lease(&socket);
        assert!(matches!(
            daemon_startup_ready(&socket, timeout, timeout, exited),
            Err(HostedTerminalStartError::HostReadyTimeout { .. })
        ));
        drop(lease);
        // A stale lock file must not disguise a failed restoration as a winner.
        assert!(matches!(
            daemon_startup_ready(&socket, timeout, Duration::from_secs(2), exited),
            Err(HostedTerminalStartError::HostExited { .. })
        ));
        assert!(matches!(
            daemon_startup_ready(&socket, DAEMON_READY_TIMEOUT, DAEMON_READY_TIMEOUT, None,),
            Err(HostedTerminalStartError::HostReadyTimeout { .. })
        ));
    }

    fn published_state(visible: bool) -> (HostedPublishedState, Arc<AtomicUsize>) {
        let wake_count = Arc::new(AtomicUsize::new(0));
        let callback_count = Arc::clone(&wake_count);
        let published = HostedPublishedState::new(
            None,
            visible,
            Arc::new(move |_| {
                callback_count.fetch_add(1, Ordering::Relaxed);
            }),
        );
        (published, wake_count)
    }

    #[test]
    fn hidden_agent_activity_coalesces_and_clears_without_presentation_credit() {
        use crate::agent::{Activity, ActivityState, Provider};
        let (published, wakes) = published_state(false);
        let (events, received) = crossbeam_channel::bounded(4);
        let working = Some(Activity {
            provider: Provider::Omp,
            state: ActivityState::Working,
        });
        published.observe_agent_activity(&events, working).unwrap();
        published.observe_agent_activity(&events, working).unwrap();
        assert_eq!(
            received.try_recv().unwrap(),
            TerminalEvent::AgentActivityChanged(working)
        );
        assert!(received.try_recv().is_err());
        assert_eq!(wakes.load(Ordering::Relaxed), 1);
        published.begin_event_drain();
        published.observe_agent_activity(&events, None).unwrap();
        assert_eq!(
            received.try_recv().unwrap(),
            TerminalEvent::AgentActivityChanged(None)
        );
        assert_eq!(wakes.load(Ordering::Relaxed), 2);
        assert_eq!(published.snapshot_cursor(), u64::MAX);
        assert_eq!(published.acknowledged_generation.load(Ordering::Acquire), 0);
        assert!(published.acknowledgement_rx.try_recv().is_err());
    }

    #[test]
    fn failed_activity_delivery_retains_clear_for_state_reconciliation() {
        use crate::agent::{Activity, ActivityState, Provider};
        let (published, _) = published_state(false);
        let (events, _) = crossbeam_channel::bounded(1);
        *published.agent_activity.lock().unwrap() = Some(Activity {
            provider: Provider::Codex,
            state: ActivityState::Working,
        });
        assert!(published.observe_agent_activity(&events, None).is_err());
        assert_eq!(*published.agent_activity.lock().unwrap(), None);
    }

    #[test]
    fn hidden_output_observation_ignores_history_and_stale_epochs_without_visual_credit() {
        let (published, wakes) = published_state(false);
        published.observe_activity(Some(crate::model::TerminalOutputActivity {
            sequence: 8,
            last_output_at_unix_ms: 100,
        }));
        assert!(
            !published.take_observed_output(),
            "attach history is not new output"
        );
        assert_eq!(wakes.load(Ordering::Relaxed), 0);
        published.observe_output(0);
        published.observe_output(0);
        assert_eq!(wakes.load(Ordering::Relaxed), 1);
        assert!(
            published.take_observed_output(),
            "the first hidden output must be delivered"
        );
        assert!(!published.take_observed_output());
        published
            .output_observation
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .epoch = 1;
        published.begin_event_drain();
        published.observe_output(0);
        assert!(
            !published.take_observed_output(),
            "an in-flight old scope notification is stale"
        );
        assert_eq!(wakes.load(Ordering::Relaxed), 1);
        published.observe_output(1);
        assert!(
            published.take_observed_output(),
            "acknowledgement rearms an offscreen surface"
        );
        assert_eq!(wakes.load(Ordering::Relaxed), 2);
        assert_eq!(published.generation.load(Ordering::Acquire), 0);
        assert_eq!(published.acknowledged_generation.load(Ordering::Acquire), 0);
        assert!(published.acknowledgement_rx.try_recv().is_err());
        assert_eq!(published.snapshot_cursor(), u64::MAX);
    }

    #[test]
    fn connection_loss_invalidates_activity_without_discarding_grid_or_high_water() {
        for visible in [false, true] {
            let (published, wakes) = published_state(visible);
            let fact = crate::model::TerminalOutputActivity {
                sequence: 10,
                last_output_at_unix_ms: 1_000,
            };
            published.publish(RenderSnapshot {
                generation: 1,
                activity: Some(fact),
                ..RenderSnapshot::empty()
            });
            let grid = published.snapshot.load_full();
            published.acknowledge(1);
            let before = wakes.load(Ordering::Relaxed);
            published.invalidate_activity();
            assert_eq!(published.activity(), None);
            assert!(Arc::ptr_eq(&grid, &published.snapshot.load_full()));
            assert_eq!(wakes.load(Ordering::Relaxed), before + usize::from(visible));
            published.observe_activity(Some(crate::model::TerminalOutputActivity {
                sequence: 9,
                ..fact
            }));
            assert_eq!(published.activity(), None);
            // A validated same-incarnation descriptor can restore evidence even
            // when no new visual generation needs to cross the wire.
            published.observe_activity(Some(fact));
            assert_eq!(published.activity(), Some(fact));
            published.observe_activity(None);
            published.observe_activity(Some(crate::model::TerminalOutputActivity {
                sequence: 9,
                ..fact
            }));
            assert_eq!(published.activity(), None);
        }
    }

    #[test]
    fn permanent_handshake_failure_waits_for_inbox_space_or_stop() {
        for cancel in [false, true] {
            let (events, inbox) = crossbeam_channel::bounded(1);
            events.send(TerminalEvent::Bell).unwrap();
            let (wakes, wake_rx) = crossbeam_channel::unbounded();
            let published = HostedPublishedState::new(
                Some(proof("old")),
                false,
                Arc::new(move |_| {
                    let _ = wakes.send(());
                }),
            );
            let stop = Arc::new(WorkerStop::default());
            let worker_stop = Arc::clone(&stop);
            let (finished, finish_rx) = crossbeam_channel::bounded(1);
            let worker = thread::spawn(move || {
                let end = reject_handshake(
                    &events,
                    &published,
                    &worker_stop,
                    ProtocolError::new(ProtocolErrorCode::ProofMismatch, "old daemon", false),
                );
                finished.send(end).unwrap();
            });
            wake_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            if cancel {
                stop.request();
            } else {
                assert!(matches!(inbox.recv().unwrap(), TerminalEvent::Bell));
                assert!(matches!(
                    inbox.recv_timeout(Duration::from_secs(5)).unwrap(),
                    TerminalEvent::Error {
                        operation: "connect to cagents daemon",
                        ..
                    }
                ));
            }
            assert_eq!(
                finish_rx.recv_timeout(Duration::from_secs(5)).unwrap(),
                ConnectionEnd::Stop
            );
            worker.join().unwrap();
        }
    }

    fn proof(incarnation: &str) -> HostSessionProof {
        HostSessionProof {
            host_session_id: HostSessionId::new("session"),
            boot_id: BootId::new("boot"),
            host_incarnation: HostIncarnation::new(incarnation),
        }
    }

    fn session() -> TerminalSession {
        TerminalDescriptor::relaunchable(
            TerminalId::new(),
            LaunchRecipe {
                program: "/bin/sh".to_owned(),
                arguments: Vec::new(),
                working_directory: PathBuf::from("/"),
                environment: BTreeMap::new(),
            },
            None,
        )
        .session
    }

    fn accept_connection(listener: &UnixListener) -> UnixStream {
        listener.set_nonblocking(true).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match listener.accept() {
                Ok((stream, _)) => {
                    stream
                        .set_read_timeout(Some(Duration::from_secs(2)))
                        .unwrap();
                    return stream;
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < deadline, "client did not connect");
                    thread::sleep(Duration::from_millis(1));
                }
                Err(error) => panic!("could not accept client: {error}"),
            }
        }
    }

    #[test]
    fn terminal_identity_is_published_only_after_attachment_acknowledgement() {
        for spawn in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let socket = directory.path().join("daemon.sock");
            let listener = UnixListener::bind(&socket).unwrap();
            let session = session();
            let pinned = proof("confirmed");
            let handle = if spawn {
                HostedTerminalHandle::attach_or_spawn_at(
                    socket,
                    session.clone(),
                    TerminalSpawnOptions::default(),
                    Some(pinned.clone()),
                    |_| {},
                )
            } else {
                HostedTerminalHandle::connect_at(socket, session.id, Some(pinned.clone()), |_| {})
            }
            .unwrap();
            assert_eq!(handle.proof(), None);
            let mut stream = accept_connection(&listener);
            let connect: ClientEnvelope = read_frame(&mut stream).unwrap().unwrap();
            let send = |stream: &mut UnixStream, request_id, response| {
                write_frame(
                    stream,
                    &ServerEnvelope {
                        version: PROTOCOL_VERSION,
                        request_id: Some(request_id),
                        proof: pinned.clone(),
                        message: ServerMessage::Response { response },
                    },
                )
                .unwrap();
            };
            send(
                &mut stream,
                connect.request_id,
                ServerResponse::Connected {
                    proof: pinned.clone(),
                    resumed: Vec::new(),
                },
            );
            let request: ClientEnvelope = read_frame(&mut stream).unwrap().unwrap();
            assert_eq!(request.proof, Some(pinned.clone()));
            match &request.request {
                ClientRequest::Spawn {
                    session: requested, ..
                } if spawn => {
                    assert_eq!(requested.id, session.id);
                }
                ClientRequest::Attach { terminal_id, .. } if !spawn => {
                    assert_eq!(*terminal_id, session.id);
                }
                other => panic!("unexpected attachment request: {other:?}"),
            }
            // Daemon authentication and sending a fenced request are not proof
            // that the daemon has accepted the exact terminal identity.
            assert_eq!(handle.proof(), None);
            assert!(handle.wait_until_attached(Duration::ZERO).is_err());
            let terminal = TerminalDescriptor {
                session,
                generation: 0,
                event_sequence: 0,
                activity: None,
                agent_activity: None,
                upgrade_sources: Vec::new(),
                upgrade_relaunches: Vec::new(),
                oldest_event_sequence: 1,
                attached: true,
                transcript: None,
            };
            send(
                &mut stream,
                request.request_id,
                if spawn {
                    ServerResponse::Spawned { terminal }
                } else {
                    ServerResponse::Attached { terminal }
                },
            );
            handle.wait_until_attached(Duration::from_secs(2)).unwrap();
            assert_eq!(handle.proof(), Some(pinned));
            assert!(matches!(
                handle
                    .events()
                    .recv_timeout(Duration::from_secs(2))
                    .unwrap(),
                TerminalEvent::ResumeHintChanged(_)
            ));
        }
    }

    #[test]
    fn rejected_spawn_wakes_attachment_waiter_without_consuming_failure_event() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("daemon.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let pinned = proof("rejected");
        let handle = HostedTerminalHandle::attach_or_spawn_at(
            socket,
            session(),
            TerminalSpawnOptions::default(),
            Some(pinned.clone()),
            |_| {},
        )
        .unwrap();
        let mut stream = accept_connection(&listener);
        let connect: ClientEnvelope = read_frame(&mut stream).unwrap().unwrap();
        write_frame(
            &mut stream,
            &ServerEnvelope {
                version: PROTOCOL_VERSION,
                request_id: Some(connect.request_id),
                proof: pinned.clone(),
                message: ServerMessage::Response {
                    response: ServerResponse::Connected {
                        proof: pinned.clone(),
                        resumed: Vec::new(),
                    },
                },
            },
        )
        .unwrap();
        let spawn: ClientEnvelope = read_frame(&mut stream).unwrap().unwrap();
        assert!(matches!(spawn.request, ClientRequest::Spawn { .. }));
        write_frame(
            &mut stream,
            &ServerEnvelope {
                version: PROTOCOL_VERSION,
                request_id: Some(spawn.request_id),
                proof: pinned,
                message: ServerMessage::Error(ProtocolError::new(
                    ProtocolErrorCode::PermissionDenied,
                    "terminal access denied",
                    false,
                )),
            },
        )
        .unwrap();
        let failure = handle
            .wait_until_attached(Duration::from_secs(2))
            .unwrap_err();
        assert!(failure.contains("terminal access denied"));
        assert_eq!(handle.proof(), None);
        assert!(matches!(
            handle
                .events()
                .recv_timeout(Duration::from_secs(2))
                .unwrap(),
            TerminalEvent::Error {
                operation: "spawn hosted terminal",
                ..
            }
        ));
    }

    fn assert_handshake_stops(message: ServerMessage) {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("daemon.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let pinned = proof("old");
        let (wake_tx, wake_rx) = crossbeam_channel::unbounded();
        let handle = HostedTerminalHandle::attach_or_spawn_at(
            socket,
            session(),
            TerminalSpawnOptions::default(),
            Some(pinned.clone()),
            move |wake| {
                let _ = wake_tx.send(wake);
            },
        )
        .unwrap();
        handle.input(b"must not replay".to_vec()).unwrap();
        let mut stream = accept_connection(&listener);
        let connect: ClientEnvelope = read_frame(&mut stream).unwrap().unwrap();
        assert!(matches!(
            connect.request,
            ClientRequest::Connect { expected_host: Some(expected), .. } if expected == pinned
        ));
        write_frame(
            &mut stream,
            &ServerEnvelope {
                version: PROTOCOL_VERSION,
                request_id: Some(connect.request_id),
                proof: proof("new"),
                message,
            },
        )
        .unwrap();

        let events = handle.events();
        assert!(matches!(
            events.recv_timeout(Duration::from_secs(2)).unwrap(),
            TerminalEvent::Error {
                operation: "connect to cagents daemon",
                ..
            }
        ));
        wake_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        // A second receive proves the worker exited, rather than backing off.
        assert_eq!(
            events.recv_timeout(Duration::from_secs(2)),
            Err(crossbeam_channel::RecvTimeoutError::Disconnected)
        );
        assert_eq!(handle.input(vec![b'x']), Err(TerminalSendError::Closed));
        assert_eq!(handle.proof(), None);
        assert!(handle.wait_until_attached(Duration::ZERO).is_err());
        // Not even the queued input or original spawn intent crossed the fence.
        assert_eq!(read_frame::<ClientEnvelope>(&mut stream).unwrap(), None);
        assert!(matches!(
            listener.accept(),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock
        ));
    }

    #[test]
    fn stale_handshake_stops_even_when_an_older_daemon_marks_it_retryable() {
        assert_handshake_stops(ServerMessage::Error(ProtocolError::new(
            ProtocolErrorCode::ProofMismatch,
            "the request targets a different cagents daemon incarnation",
            true,
        )));
    }

    #[test]
    fn non_retryable_handshake_failure_stops_the_client() {
        assert_handshake_stops(ServerMessage::Error(ProtocolError::new(
            ProtocolErrorCode::PermissionDenied,
            "terminal access denied",
            false,
        )));
    }

    #[test]
    fn connected_response_cannot_repin_a_stale_terminal() {
        assert_handshake_stops(ServerMessage::Response {
            response: ServerResponse::Connected {
                proof: proof("new"),
                resumed: Vec::new(),
            },
        });
    }

    #[test]
    fn committed_shutdown_waits_for_replacement_before_requesting_recovery() {
        for during_attach in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let socket = directory.path().join("daemon.sock");
            let listener = UnixListener::bind(&socket).unwrap();
            let pinned = proof("old");
            let session = session();
            let handle =
                HostedTerminalHandle::connect_at(socket, session.id, Some(pinned.clone()), |_| {})
                    .unwrap();
            let send = |stream: &mut UnixStream, request_id, proof, message| {
                write_frame(
                    stream,
                    &ServerEnvelope {
                        version: PROTOCOL_VERSION,
                        request_id: Some(request_id),
                        proof,
                        message,
                    },
                )
                .unwrap();
            };
            let shutdown = || {
                ServerMessage::Error(ProtocolError::new(
                    ProtocolErrorCode::TerminalUnavailable,
                    "Daemon upgrade/shutdown is committed; no further control mutations are accepted",
                    false,
                ))
            };
            let mut stream = accept_connection(&listener);
            let connect: ClientEnvelope = read_frame(&mut stream).unwrap().unwrap();
            send(
                &mut stream,
                connect.request_id,
                pinned.clone(),
                ServerMessage::Response {
                    response: ServerResponse::Connected {
                        proof: pinned.clone(),
                        resumed: Vec::new(),
                    },
                },
            );
            let attach: ClientEnvelope = read_frame(&mut stream).unwrap().unwrap();
            assert!(matches!(attach.request, ClientRequest::Attach { .. }));
            let visibility: ClientEnvelope = read_frame(&mut stream).unwrap().unwrap();
            assert!(matches!(
                visibility.request,
                ClientRequest::SetVisibility { .. }
            ));
            if !during_attach {
                send(
                    &mut stream,
                    attach.request_id,
                    pinned.clone(),
                    ServerMessage::Response {
                        response: ServerResponse::Attached {
                            terminal: TerminalDescriptor {
                                session,
                                generation: 0,
                                event_sequence: 0,
                                oldest_event_sequence: 1,
                                attached: true,
                                transcript: None,
                                activity: None,
                                agent_activity: None,
                                upgrade_sources: Vec::new(),
                                upgrade_relaunches: Vec::new(),
                            },
                        },
                    },
                );
                handle.wait_until_attached(Duration::from_secs(2)).unwrap();
            }
            send(
                &mut stream,
                if during_attach {
                    attach.request_id
                } else {
                    visibility.request_id
                },
                pinned.clone(),
                shutdown(),
            );

            // The old daemon may still own the socket while it drains. Its
            // handshake refusal must not destroy the UI runtime either.
            let mut draining = accept_connection(&listener);
            let connect: ClientEnvelope = read_frame(&mut draining).unwrap().unwrap();
            assert!(matches!(connect.request,
                ClientRequest::Connect { expected_host: Some(ref expected), .. } if expected == &pinned));
            send(
                &mut draining,
                connect.request_id,
                pinned.clone(),
                shutdown(),
            );

            let mut replacement = accept_connection(&listener);
            let connect: ClientEnvelope = read_frame(&mut replacement).unwrap().unwrap();
            assert!(matches!(connect.request,
                ClientRequest::Connect { expected_host: Some(ref expected), .. } if expected == &pinned));
            assert!(
                handle
                    .events()
                    .try_iter()
                    .all(|event| !matches!(event, TerminalEvent::Error { .. }))
            );
            // Queued input must never cross into a new incarnation. Recovery
            // must instead go through the UI's authenticated lineage checks.
            handle.input(b"must not replay".to_vec()).unwrap();
            send(
                &mut replacement,
                connect.request_id,
                proof("replacement"),
                ServerMessage::Error(ProtocolError::new(
                    ProtocolErrorCode::ProofMismatch,
                    "daemon was replaced",
                    false,
                )),
            );
            assert!(matches!(
                handle
                    .events()
                    .recv_timeout(Duration::from_secs(2))
                    .unwrap(),
                TerminalEvent::Error {
                    operation: "connect to cagents daemon",
                    ..
                }
            ));
            assert_eq!(
                handle.events().recv_timeout(Duration::from_secs(2)),
                Err(crossbeam_channel::RecvTimeoutError::Disconnected)
            );
            assert_eq!(
                read_frame::<ClientEnvelope>(&mut replacement).unwrap(),
                None
            );
        }
    }

    #[test]
    fn unavailable_terminal_is_not_a_committed_daemon_shutdown() {
        assert!(!is_committed_daemon_shutdown(&ProtocolError::new(
            ProtocolErrorCode::TerminalUnavailable,
            "terminal process is unavailable",
            false,
        )));
    }

    #[test]
    fn transient_disconnect_and_retryable_handshake_preserve_same_host_reconnect() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("daemon.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let pinned = proof("same");
        let terminal_id = TerminalId::new();
        let handle =
            HostedTerminalHandle::connect_at(socket, terminal_id, Some(pinned.clone()), |_| {})
                .unwrap();
        handle.input(vec![b'x']).unwrap();

        for attempt in 0..3 {
            let mut stream = accept_connection(&listener);
            let connect: ClientEnvelope = read_frame(&mut stream).unwrap().unwrap();
            assert!(matches!(
                connect.request,
                ClientRequest::Connect { expected_host: Some(expected), .. } if expected == pinned
            ));
            if attempt == 0 {
                // EOF before the handshake remains a transient transport error.
                continue;
            }
            let message = if attempt == 1 {
                ServerMessage::Error(ProtocolError::new(
                    ProtocolErrorCode::QueueFull,
                    "daemon is busy",
                    true,
                ))
            } else {
                ServerMessage::Response {
                    response: ServerResponse::Connected {
                        proof: pinned.clone(),
                        resumed: Vec::new(),
                    },
                }
            };
            write_frame(
                &mut stream,
                &ServerEnvelope {
                    version: PROTOCOL_VERSION,
                    request_id: Some(connect.request_id),
                    proof: pinned.clone(),
                    message,
                },
            )
            .unwrap();
            if attempt == 1 {
                assert!(matches!(
                    handle
                        .events()
                        .recv_timeout(Duration::from_secs(2))
                        .unwrap(),
                    TerminalEvent::Error { .. }
                ));
                continue;
            }
            let attach: ClientEnvelope = read_frame(&mut stream).unwrap().unwrap();
            assert_eq!(attach.proof, Some(pinned.clone()));
            assert!(matches!(
                attach.request,
                ClientRequest::Attach { terminal_id: id, .. } if id == terminal_id
            ));
            let visibility: ClientEnvelope = read_frame(&mut stream).unwrap().unwrap();
            assert!(matches!(
                visibility.request,
                ClientRequest::SetVisibility { .. }
            ));
            let input: ClientEnvelope = read_frame(&mut stream).unwrap().unwrap();
            assert_eq!(input.proof, Some(pinned.clone()));
            assert!(matches!(
                input.request,
                ClientRequest::Input { terminal_id: id, bytes }
                    if id == terminal_id && bytes == *b"x"
            ));
            assert_eq!(handle.proof(), None);
            drop(handle);
            break;
        }
    }

    #[test]
    fn hidden_semantic_events_wake_and_are_drainable_without_visual_wakes() {
        let (published, wake_count) = published_state(false);
        let (event_tx, event_rx) = crossbeam_channel::bounded(EVENT_CAPACITY);

        published.publish(RenderSnapshot::detached(7));
        assert_eq!(wake_count.load(Ordering::Relaxed), 0);
        assert_eq!(published.acknowledged_generation.load(Ordering::Acquire), 0);

        let exited = TerminalEvent::Exited {
            exit_code: 1,
            signal: None,
            intentional: false,
        };
        let error = TerminalEvent::Error {
            operation: "read hosted terminal",
            message: "connection closed".to_owned(),
        };
        accept_server_event(&event_tx, &published, 1, exited.clone()).unwrap();
        accept_server_event(&event_tx, &published, 2, error.clone()).unwrap();

        assert_eq!(wake_count.load(Ordering::Relaxed), 1);
        published.begin_event_drain();
        assert_eq!(event_rx.try_iter().collect::<Vec<_>>(), vec![exited, error]);
        assert_eq!(published.acknowledged_generation.load(Ordering::Acquire), 0);
    }

    #[test]
    fn semantic_wakes_coalesce_at_queue_capacity_and_rearm_after_drain() {
        let (published, wake_count) = published_state(false);
        let (event_tx, event_rx) = crossbeam_channel::bounded(EVENT_CAPACITY);

        for sequence in 1..=EVENT_CAPACITY as u64 {
            accept_server_event(&event_tx, &published, sequence, TerminalEvent::Bell).unwrap();
        }
        assert_eq!(wake_count.load(Ordering::Relaxed), 1);
        assert!(matches!(
            accept_server_event(
                &event_tx,
                &published,
                EVENT_CAPACITY as u64 + 1,
                TerminalEvent::Bell
            ),
            Err(TrySendError::Full(TerminalEvent::Bell))
        ));
        assert_eq!(wake_count.load(Ordering::Relaxed), 1);

        published.begin_event_drain();
        assert_eq!(event_rx.try_iter().count(), EVENT_CAPACITY);
        accept_server_event(
            &event_tx,
            &published,
            EVENT_CAPACITY as u64 + 1,
            TerminalEvent::Bell,
        )
        .unwrap();

        assert_eq!(wake_count.load(Ordering::Relaxed), 2);
        assert_eq!(event_rx.try_recv(), Ok(TerminalEvent::Bell));
    }

    /// A published state whose wake callback records the generation of every
    /// wake it emits, so a test can assert what the UI was actually told
    /// rather than only how often it was woken.
    fn recording_published_state(
        visible: bool,
    ) -> (HostedPublishedState, impl Fn() -> Vec<u64> + Clone) {
        let announced = Arc::new(Mutex::new(Vec::new()));
        let recorder = Arc::clone(&announced);
        let published = HostedPublishedState::new(
            None,
            visible,
            Arc::new(move |wake: TerminalWake| {
                recorder
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .push(wake.generation);
            }),
        );
        (published, move || {
            announced
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .clone()
        })
    }

    fn published_grid(generation: u64, text: &str) -> RenderSnapshot {
        use crate::terminal::{CellSnapshot, CellText, RowSnapshot};
        RenderSnapshot {
            generation,
            columns: 1,
            rows: 1,
            lines: vec![Arc::new(RowSnapshot {
                cells: vec![CellSnapshot {
                    text: CellText::new(text),
                    ..CellSnapshot::default()
                }],
            })]
            .into(),
            ..RenderSnapshot::empty()
        }
    }

    #[test]
    fn visual_and_semantic_wake_edges_are_independent() {
        let (published, wake_count) = published_state(true);
        let (event_tx, _event_rx) = crossbeam_channel::bounded(EVENT_CAPACITY);

        published.publish(RenderSnapshot::detached(1));
        accept_server_event(&event_tx, &published, 1, TerminalEvent::Bell).unwrap();

        assert_eq!(wake_count.load(Ordering::Relaxed), 2);
        assert_eq!(published.announced_generation.load(Ordering::Acquire), 1);
        assert!(published.semantic_wake_pending.load(Ordering::Acquire));
        published.begin_event_drain();
        assert_eq!(published.announced_generation.load(Ordering::Acquire), 1);
        assert!(!published.semantic_wake_pending.load(Ordering::Acquire));
        assert_eq!(published.acknowledged_generation.load(Ordering::Acquire), 0);
    }

    /// The defect this protects against: a wake the UI was handed but never
    /// answered with a draw - the card was not mounted in the view that wake
    /// produced - used to suppress every later generation, so the terminal
    /// stayed frozen until an unrelated pointer event repainted the widget
    /// from this snapshot.
    #[test]
    fn a_generation_no_draw_answered_does_not_silence_the_next_one() {
        let (published, announced) = recording_published_state(true);

        published.publish(published_grid(1, "a"));
        assert_eq!(announced(), vec![1]);

        published.publish(published_grid(2, "b"));

        assert_eq!(announced(), vec![1, 2]);
        assert_eq!(published.snapshot.load().lines[0].cells[0].text, "b");
        // The draw that second wake produces is what returns the redraw credit
        // the daemon withholds its next capture for.
        published.acknowledge(2);
        assert_eq!(published.acknowledgement_rx.try_recv(), Ok(()));
    }

    /// A terminal at full output rate must still cost one wake per drawn
    /// frame, which is the reason the announcement is a watermark rather than
    /// an unconditional edge: a capture that also carries a new activity fact
    /// is still one repaint.
    #[test]
    fn a_published_generation_is_announced_once_even_when_its_activity_changed() {
        let (published, announced) = recording_published_state(true);

        published.publish(RenderSnapshot {
            activity: Some(crate::model::TerminalOutputActivity {
                sequence: 7,
                last_output_at_unix_ms: 1,
            }),
            ..published_grid(1, "a")
        });

        assert_eq!(announced(), vec![1]);
        assert_eq!(published.activity().map(|fact| fact.sequence), Some(7));
    }

    /// Reveal owes the UI a repaint that no generation can carry: the grid it
    /// holds is a detached marker, or one the UI drew before it was hidden.
    /// The announcement for that generation has already been spent.
    #[test]
    fn a_reveal_announces_a_repaint_after_an_unanswered_generation() {
        let (published, announced) = recording_published_state(true);

        published.publish(published_grid(1, "a"));
        published.set_visible(false);
        published.set_visible(true);

        assert_eq!(announced(), vec![1, 1]);
    }

    #[test]
    fn reconnect_visibility_vote_tracks_current_handle_state() {
        let (published, _wake_count) = published_state(false);

        assert_eq!(published.desired_visibility(), TerminalVisibility::Hidden);
        published.set_visible(true);
        assert_eq!(published.desired_visibility(), TerminalVisibility::Visible);
        published.set_visible(false);
        assert_eq!(published.desired_visibility(), TerminalVisibility::Hidden);
        published.hibernate();
        assert_eq!(
            published.desired_visibility(),
            TerminalVisibility::Hibernated
        );
    }

    #[test]
    fn requested_stop_preempts_a_carried_shutdown_before_reconnect() {
        let stop = WorkerStop::default();
        let (_command_tx, command_rx) = crossbeam_channel::bounded(COMMAND_CAPACITY);
        let (completed_tx, _completed_rx) = crossbeam_channel::bounded(1);
        let mut carried = Some(ClientCommand::ShutdownTerminal(completed_tx));

        stop.request();

        assert_eq!(
            wait_before_reconnect(&stop, &command_rx, &mut carried, MAX_RECONNECT_DELAY,),
            ConnectionEnd::Stop
        );
        assert!(matches!(carried, Some(ClientCommand::ShutdownTerminal(_))));
    }

    #[test]
    fn queued_input_wakes_client_without_waiting_for_server_io() {
        let stop = WorkerStop::default();
        let (command_tx, command_rx) = crossbeam_channel::bounded(COMMAND_CAPACITY);
        let (_server_tx, server_rx) = crossbeam_channel::bounded(EVENT_CAPACITY);
        let (published, _) = published_state(true);
        command_tx
            .try_send(ClientCommand::Input(vec![b'x']))
            .unwrap();

        assert!(matches!(
            wait_for_client_work(&stop, &command_rx, &server_rx, &published.acknowledgement_rx),
            ClientWork::Command(ClientCommand::Input(bytes)) if bytes == *b"x"
        ));
    }

    #[test]
    fn redraw_credit_wakes_idle_client_and_coalesces_to_latest_generation() {
        let stop = WorkerStop::default();
        let (_command_tx, command_rx) = crossbeam_channel::bounded(COMMAND_CAPACITY);
        let (_server_tx, server_rx) = crossbeam_channel::bounded(EVENT_CAPACITY);
        let (published, _) = published_state(true);

        published.acknowledge(1);
        published.acknowledge(2);
        published.acknowledge(1);
        assert!(matches!(
            wait_for_client_work(
                &stop,
                &command_rx,
                &server_rx,
                &published.acknowledgement_rx
            ),
            ClientWork::Acknowledge
        ));
        assert_eq!(published.acknowledged_generation.load(Ordering::Acquire), 2);
        assert_eq!(
            published.acknowledgement_rx.try_recv(),
            Err(TryRecvError::Empty)
        );
        published.acknowledge(2);
        assert_eq!(
            published.acknowledgement_rx.try_recv(),
            Err(TryRecvError::Empty)
        );
        published.acknowledge(3);
        assert!(matches!(
            wait_for_client_work(
                &stop,
                &command_rx,
                &server_rx,
                &published.acknowledgement_rx
            ),
            ClientWork::Acknowledge
        ));
    }

    #[test]
    fn progressive_snapshots_return_redraw_credit_without_mouse_or_keyboard_input() {
        use crate::terminal::{CellSnapshot, CellText, RowSnapshot};

        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("daemon.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let session = session();
        let terminal_id = session.id;
        let pinned = proof("progressive");
        let (wake_tx, wake_rx) = crossbeam_channel::unbounded();
        let handle = HostedTerminalHandle::connect_at(
            socket,
            terminal_id,
            Some(pinned.clone()),
            move |wake| {
                let _ = wake_tx.send(wake);
            },
        )
        .unwrap();
        let mut stream = accept_connection(&listener);
        let connect: ClientEnvelope = read_frame(&mut stream).unwrap().unwrap();
        let send = |stream: &mut UnixStream, request_id, message| {
            write_frame(
                stream,
                &ServerEnvelope {
                    version: PROTOCOL_VERSION,
                    request_id,
                    proof: pinned.clone(),
                    message,
                },
            )
            .unwrap();
        };
        send(
            &mut stream,
            Some(connect.request_id),
            ServerMessage::Response {
                response: ServerResponse::Connected {
                    proof: pinned.clone(),
                    resumed: vec![TerminalDescriptor {
                        session,
                        generation: 0,
                        event_sequence: 0,
                        activity: None,
                        agent_activity: None,
                        upgrade_sources: Vec::new(),
                        upgrade_relaunches: Vec::new(),
                        oldest_event_sequence: 1,
                        attached: true,
                        transcript: None,
                    }],
                },
            },
        );
        let visibility: ClientEnvelope = read_frame(&mut stream).unwrap().unwrap();
        assert!(matches!(
            visibility.request,
            ClientRequest::SetVisibility { .. }
        ));
        send(
            &mut stream,
            Some(visibility.request_id),
            ServerMessage::Response {
                response: ServerResponse::Accepted,
            },
        );

        let mut previous = RenderSnapshot::empty();
        for (index, text) in ["a", "b", "c"].into_iter().enumerate() {
            let generation = index as u64 + 1;
            send(
                &mut stream,
                None,
                ServerMessage::Redraw {
                    terminal_id,
                    generation,
                },
            );
            let request: ClientEnvelope = read_frame(&mut stream).unwrap().unwrap();
            assert!(matches!(
                request.request,
                ClientRequest::Snapshot { terminal_id: id, after_generation }
                    if id == terminal_id && after_generation == generation - 1
            ));
            let snapshot = RenderSnapshot {
                generation,
                columns: 1,
                rows: 1,
                lines: vec![Arc::new(RowSnapshot {
                    cells: vec![CellSnapshot {
                        text: CellText::new(text),
                        ..CellSnapshot::default()
                    }],
                })]
                .into(),
                ..RenderSnapshot::empty()
            };
            send(
                &mut stream,
                Some(request.request_id),
                ServerMessage::Response {
                    response: ServerResponse::Snapshot {
                        terminal_id,
                        snapshot: Some(TerminalSnapshot::diff(&snapshot, &previous)),
                    },
                },
            );
            let wake = wake_rx.recv_timeout(Duration::from_secs(2)).unwrap();
            assert_eq!(wake.generation, generation);
            let observed = handle.snapshot();
            assert_eq!(observed.lines[0].cells[0].text, text);
            handle.acknowledge_redraw(observed.generation);
            let ack: ClientEnvelope = read_frame(&mut stream).unwrap().unwrap();
            assert!(matches!(
                ack.request,
                ClientRequest::AcknowledgeRedraw { terminal_id: id, generation: acknowledged }
                    if id == terminal_id && acknowledged == generation
            ));
            send(
                &mut stream,
                Some(ack.request_id),
                ServerMessage::Response {
                    response: ServerResponse::Accepted,
                },
            );
            previous = snapshot;
        }

        // Repaint traffic already in flight must not rebuild a hibernated
        // view, even when another subscriber keeps this terminal visible.
        handle.hibernate().unwrap();
        let hibernate: ClientEnvelope = read_frame(&mut stream).unwrap().unwrap();
        assert!(matches!(hibernate.request, ClientRequest::Hibernate { .. }));
        let mut background = previous.clone();
        background.generation += 1;
        send(
            &mut stream,
            None,
            ServerMessage::Snapshot {
                terminal_id,
                snapshot: TerminalSnapshot::full(&background),
            },
        );
        send(
            &mut stream,
            None,
            ServerMessage::Redraw {
                terminal_id,
                generation: background.generation,
            },
        );
        send(
            &mut stream,
            None,
            ServerMessage::Event {
                terminal_id,
                sequence: 1,
                event: crate::terminal_ipc::WireTerminalEvent::Bell,
            },
        );
        assert_eq!(
            handle
                .events()
                .recv_timeout(Duration::from_secs(2))
                .unwrap(),
            TerminalEvent::Bell
        );
        assert!(handle.snapshot().detached);
        assert_eq!(handle.generation(), previous.generation);

        // Resume before the old hibernate ACK arrives. It must not override
        // the newer visibility, and the same retained generation must restore
        // successfully without requiring another byte of process output.
        handle.restore(true).unwrap();
        let resume: ClientEnvelope = read_frame(&mut stream).unwrap().unwrap();
        assert!(matches!(
            resume.request,
            ClientRequest::Resume { visible: true, .. }
        ));
        for request_id in [hibernate.request_id, resume.request_id] {
            send(
                &mut stream,
                Some(request_id),
                ServerMessage::Response {
                    response: ServerResponse::Accepted,
                },
            );
        }
        send(
            &mut stream,
            None,
            ServerMessage::Redraw {
                terminal_id,
                generation: previous.generation,
            },
        );
        let repaint: ClientEnvelope = read_frame(&mut stream).unwrap().unwrap();
        assert!(matches!(
            repaint.request,
            ClientRequest::Snapshot {
                after_generation: 0,
                ..
            }
        ));
        send(
            &mut stream,
            Some(repaint.request_id),
            ServerMessage::Response {
                response: ServerResponse::Snapshot {
                    terminal_id,
                    snapshot: Some(TerminalSnapshot::full(&previous)),
                },
            },
        );
        send(
            &mut stream,
            None,
            ServerMessage::Event {
                terminal_id,
                sequence: 2,
                event: crate::terminal_ipc::WireTerminalEvent::Bell,
            },
        );
        assert_eq!(
            handle
                .events()
                .recv_timeout(Duration::from_secs(2))
                .unwrap(),
            TerminalEvent::Bell
        );
        let restored = handle.snapshot();
        assert!(!restored.detached);
        assert_eq!(restored.generation, previous.generation);
        assert_eq!(restored.lines[0].cells[0].text, "c");
        drop(handle);
    }

    #[test]
    fn requested_stop_wakes_server_only_wait_at_request_saturation() {
        let stop = WorkerStop::default();
        let (_server_tx, server_rx) = crossbeam_channel::bounded(EVENT_CAPACITY);

        stop.request();

        assert!(matches!(
            wait_for_server_or_stop(&stop, &server_rx),
            ClientWork::Stop
        ));
    }
}
