use std::{
    collections::BTreeMap,
    io,
    net::Shutdown,
    os::unix::{net::UnixStream, process::CommandExt},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        Arc, Mutex, OnceLock,
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
    KeyInput, MouseInput, RenderSnapshot, TerminalEvent, TerminalSendError, TerminalSize,
    TerminalWake,
    ipc::{
        ClientEnvelope, ClientRequest, FrameError, PROTOCOL_VERSION, ProtocolError, ResumeCursor,
        ServerEnvelope, ServerMessage, ServerResponse, TerminalScroll, TerminalSnapshot,
        TerminalSpawnOptions, TerminalVisibility, WireTerminalSize, daemon_socket_path, read_frame,
        write_frame,
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
}

impl Default for WorkerStop {
    fn default() -> Self {
        let (notification_tx, notification_rx) = crossbeam_channel::bounded(1);
        Self {
            requested: AtomicBool::new(false),
            worker: OnceLock::new(),
            notification_tx,
            notification_rx,
        }
    }
}

impl WorkerStop {
    fn register_current_thread(&self) {
        let _ = self.worker.set(thread::current());
    }

    fn request(&self) {
        self.requested.store(true, Ordering::Release);
        let _ = self.notification_tx.try_send(());
        if let Some(worker) = self.worker.get() {
            worker.unpark();
        }
    }

    fn is_requested(&self) -> bool {
        self.requested.load(Ordering::Acquire)
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
    #[error("could not connect to the cagents daemon socket at {path}: {source}")]
    HostSocket {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

struct HostedPublishedState {
    snapshot: ArcSwap<RenderSnapshot>,
    generation: AtomicU64,
    event_sequence: AtomicU64,
    acknowledged_generation: AtomicU64,
    wake_pending: AtomicBool,
    semantic_wake_pending: AtomicBool,
    visibility: AtomicU8,
    proof: Mutex<Option<HostSessionProof>>,
    wake: Arc<WakeCallback>,
}

impl HostedPublishedState {
    fn new(
        expected_host: Option<HostSessionProof>,
        visible: bool,
        wake: Arc<WakeCallback>,
    ) -> Self {
        Self {
            snapshot: ArcSwap::from_pointee(RenderSnapshot::empty()),
            generation: AtomicU64::new(0),
            event_sequence: AtomicU64::new(0),
            acknowledged_generation: AtomicU64::new(0),
            wake_pending: AtomicBool::new(false),
            semantic_wake_pending: AtomicBool::new(false),
            visibility: AtomicU8::new(Self::visibility_code(if visible {
                TerminalVisibility::Visible
            } else {
                TerminalVisibility::Hidden
            })),
            proof: Mutex::new(expected_host),
            wake,
        }
    }

    fn publish(&self, snapshot: RenderSnapshot) {
        let generation = snapshot.generation;
        if generation <= self.generation.load(Ordering::Acquire) {
            return;
        }
        self.snapshot.store(Arc::new(snapshot));
        self.generation.store(generation, Ordering::Release);
        if self.is_visible() {
            self.request_wake(generation);
        }
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
        let current = self.snapshot.load();
        match snapshot.apply(Some(current.as_ref())) {
            Ok(resolved) => {
                self.publish(resolved);
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
        self.request_wake(generation);
    }

    fn request_wake(&self, generation: u64) {
        if !self.is_visible() {
            return;
        }
        if self
            .wake_pending
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            (self.wake)(TerminalWake { generation });
        }
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

    fn begin_event_drain(&self) {
        self.semantic_wake_pending.store(false, Ordering::Release);
    }

    fn hibernate(&self) {
        self.set_visibility(TerminalVisibility::Hibernated);
        let generation = self.generation.load(Ordering::Acquire);
        self.snapshot
            .store(Arc::new(RenderSnapshot::detached(generation)));
    }

    fn acknowledge(&self, observed_generation: u64) {
        self.acknowledged_generation
            .fetch_max(observed_generation, Ordering::AcqRel);
        self.wake_pending.store(false, Ordering::Release);
        let latest = self.generation.load(Ordering::Acquire);
        if latest > observed_generation && self.is_visible() {
            self.request_wake(latest);
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
        let generation = self.generation.load(Ordering::Acquire);
        if visibility == TerminalVisibility::Visible {
            self.request_wake(generation);
        } else {
            self.wake_pending.store(false, Ordering::Release);
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
    Key(KeyInput),
    Mouse(MouseInput),
    Focus(bool),
    Resize(TerminalSize),
    Scroll(TerminalScroll),
    Visibility(TerminalVisibility),
    Hibernate,
    Resume { visible: bool },
    ShutdownTerminal(Sender<()>),
    Stop,
}

impl ClientCommand {
    fn operation(&self) -> &'static str {
        match self {
            Self::Input(_) => "write terminal input",
            Self::Key(_) => "send terminal key",
            Self::Mouse(_) => "send terminal mouse input",
            Self::Focus(_) => "set terminal focus",
            Self::Resize(_) => "resize terminal",
            Self::Scroll(_) => "scroll terminal",
            Self::Visibility(_) => "set terminal visibility",
            Self::Hibernate => "hibernate terminal",
            Self::Resume { .. } => "resume terminal",
            Self::ShutdownTerminal(_) => "shutdown terminal",
            Self::Stop => "detach terminal",
        }
    }

    fn into_request(self, terminal_id: TerminalId) -> ClientRequest {
        match self {
            Self::Input(bytes) => ClientRequest::Input { terminal_id, bytes },
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
            Self::Visibility(visibility) => ClientRequest::SetVisibility {
                terminal_id,
                visibility,
            },
            Self::Hibernate => ClientRequest::Hibernate { terminal_id },
            Self::Resume { visible } => ClientRequest::Resume {
                terminal_id,
                visible,
            },
            Self::ShutdownTerminal(_) => ClientRequest::ShutdownTerminal { terminal_id },
            Self::Stop => ClientRequest::Detach { terminal_id },
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
        let on_worker_thread = self
            .worker
            .get_mut()
            .ok()
            .and_then(|worker| worker.as_ref())
            .is_some_and(|worker| worker.thread().id() == thread::current().id());
        self.stop.request();
        if on_worker_thread {
            let _ = self.commands.try_send(ClientCommand::Stop);
        } else {
            let _ = self.commands.send(ClientCommand::Stop);
        }
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

    pub fn proof(&self) -> Option<HostSessionProof> {
        self.inner.published.proof()
    }

    pub fn snapshot(&self) -> Arc<RenderSnapshot> {
        self.inner.published.snapshot.load_full()
    }

    pub fn generation(&self) -> u64 {
        self.inner.published.generation.load(Ordering::Acquire)
    }

    pub fn acknowledge_redraw(&self, observed_generation: u64) {
        self.inner.published.acknowledge(observed_generation);
    }

    pub fn events(&self) -> Receiver<TerminalEvent> {
        self.inner.published.begin_event_drain();
        self.inner.events.clone()
    }

    pub fn input(&self, bytes: impl Into<Vec<u8>>) -> Result<(), TerminalSendError> {
        self.try_command(ClientCommand::Input(bytes.into()))
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

    pub fn hide(&self) -> Result<(), TerminalSendError> {
        self.try_command(ClientCommand::Visibility(TerminalVisibility::Hidden))?;
        self.inner.published.set_visible(false);
        Ok(())
    }

    pub fn reveal(&self) -> Result<(), TerminalSendError> {
        self.try_command(ClientCommand::Visibility(TerminalVisibility::Visible))?;
        self.inner.published.set_visible(true);
        Ok(())
    }

    pub fn hibernate(&self) -> Result<(), TerminalSendError> {
        self.try_command(ClientCommand::Hibernate)?;
        self.inner
            .published
            .set_visibility(TerminalVisibility::Hibernated);
        Ok(())
    }

    pub fn restore(&self, visible: bool) -> Result<(), TerminalSendError> {
        self.try_command(ClientCommand::Resume { visible })?;
        self.inner.published.set_visible(visible);
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
}
/// Returns the per-user daemon socket, starting the sibling `cagentsd` if necessary.
///
/// Startup is bounded to five seconds. The daemon is placed in its own process group
/// so closing the launching CLI, GUI, or terminal does not terminate it.
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

    let current_exe =
        std::env::current_exe().map_err(HostedTerminalStartError::CurrentExecutable)?;
    let Some(binary_directory) = current_exe.parent() else {
        return Err(HostedTerminalStartError::HostBinaryMissing(current_exe));
    };
    let daemon_binary = binary_directory.join(format!("cagentsd{}", std::env::consts::EXE_SUFFIX));
    if !daemon_binary.is_file() {
        return Err(HostedTerminalStartError::HostBinaryMissing(daemon_binary));
    }

    let mut child = Command::new(&daemon_binary)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .map_err(|source| HostedTerminalStartError::LaunchHost {
            path: daemon_binary,
            source,
        })?;
    let _ = thread::Builder::new()
        .name("cagentsd-reaper".into())
        .spawn(move || {
            let _ = child.wait();
        });

    let deadline = Instant::now() + DAEMON_READY_TIMEOUT;
    loop {
        match UnixStream::connect(socket) {
            Ok(stream) => {
                drop(stream);
                return Ok(());
            }
            Err(_) if Instant::now() < deadline => thread::sleep(DAEMON_READY_POLL_INTERVAL),
            Err(_) => {
                return Err(HostedTerminalStartError::HostReadyTimeout {
                    path: socket.to_owned(),
                    timeout: DAEMON_READY_TIMEOUT,
                });
            }
        }
    }
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
                loop {
                    let message = match read_frame::<ServerEnvelope>(&mut reader_stream) {
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
            incoming: Some(incoming_rx),
            reader: Some(reader),
        })
    }

    fn writer(&mut self) -> &mut UnixStream {
        &mut self.writer
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
    Command(ClientCommand),
    Server(Box<ServerEnvelope>),
    CommandsClosed,
    ServerClosed,
}

fn wait_for_client_work(
    stop: &WorkerStop,
    commands: &Receiver<ClientCommand>,
    incoming: &Receiver<ServerRead>,
) -> ClientWork {
    crossbeam_channel::select_biased! {
        recv(stop.notification()) -> _ => ClientWork::Stop,
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
            return;
        }
        match serve_connection(&connection, &mut carried) {
            ConnectionEnd::Stop => return,
            ConnectionEnd::Retry => {}
        }

        if wait_before_reconnect(&stop, &commands, &mut carried, reconnect_delay)
            == ConnectionEnd::Stop
        {
            return;
        }
        reconnect_delay = (reconnect_delay * 2).min(MAX_RECONNECT_DELAY);
    }
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

fn serve_connection(
    context: &ConnectionContext<'_>,
    carried: &mut Option<ClientCommand>,
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
                generation: published.generation.load(Ordering::Acquire),
                event_sequence: published.event_sequence.load(Ordering::Acquire),
            }],
        },
    };
    if write_frame(&mut stream, &connect).is_err() {
        return ConnectionEnd::Retry;
    }

    let (proof, mut attached) = loop {
        if stop.is_requested() {
            return ConnectionEnd::Stop;
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
        if envelope.request_id != Some(connect_id) {
            return ConnectionEnd::Retry;
        }
        match envelope.message {
            ServerMessage::Response {
                response: ServerResponse::Connected { proof, resumed },
            } if envelope.proof == proof && published.pin_proof(&proof) => {
                let attached = resumed
                    .iter()
                    .any(|terminal| terminal.session.id == terminal_id && terminal.attached);
                break (proof, attached);
            }
            ServerMessage::Error(error) => {
                publish_protocol_error(events, published, "connect to cagents daemon", error);
                return ConnectionEnd::Retry;
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
                    after_generation: published.generation.load(Ordering::Acquire),
                    after_event_sequence: published.event_sequence.load(Ordering::Acquire),
                },
            ),
            ConnectionIntent::Spawn(spawn) => (
                PendingRequest::Spawn,
                ClientRequest::Spawn {
                    session: spawn.session.clone(),
                    options: spawn.options.clone(),
                },
            ),
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
            let command = match command {
                Ok(command) => command,
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => ClientCommand::Stop,
            };
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
            wait_for_client_work(stop, commands, connection.incoming())
        } else {
            wait_for_server_or_stop(stop, connection.incoming())
        };
        let envelope = match work {
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
                    (PendingRequest::Attach, ServerResponse::Attached { terminal })
                        if terminal.session.id == terminal_id =>
                    {
                        attached = true;
                    }
                    (PendingRequest::Spawn, ServerResponse::Spawned { terminal })
                        if terminal.session.id == terminal_id =>
                    {
                        attached = true;
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
                    (PendingRequest::Hibernate, ServerResponse::Accepted) => {
                        published.hibernate();
                    }
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
                let Some(request_id) = envelope.request_id else {
                    return ConnectionEnd::Retry;
                };
                let operation = match pending.remove(&request_id) {
                    Some(PendingRequest::Attach) => {
                        published.mark_detached();
                        "attach hosted terminal"
                    }
                    Some(PendingRequest::Spawn) => {
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
                let retry = error.retryable;
                publish_protocol_error(events, published, operation, error);
                if retry {
                    return ConnectionEnd::Retry;
                }
            }
            ServerMessage::Redraw {
                terminal_id: redraw_id,
                generation,
            } if envelope.request_id.is_none() && redraw_id == terminal_id => {
                if generation > published.generation.load(Ordering::Acquire) && !snapshot_pending {
                    if send_request(
                        connection.writer(),
                        &proof,
                        &mut next_request_id,
                        &mut pending,
                        PendingRequest::Snapshot,
                        ClientRequest::Snapshot {
                            terminal_id,
                            after_generation: published.generation.load(Ordering::Acquire),
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
                if generation > published.generation.load(Ordering::Acquire) && !snapshot_pending {
                    if send_request(
                        connection.writer(),
                        &proof,
                        &mut next_request_id,
                        &mut pending,
                        PendingRequest::Snapshot,
                        ClientRequest::Snapshot {
                            terminal_id,
                            after_generation: published.generation.load(Ordering::Acquire),
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
    stream: &mut UnixStream,
    proof: &HostSessionProof,
    next_request_id: &mut u64,
    pending: &mut BTreeMap<u64, PendingRequest>,
    kind: PendingRequest,
    request: ClientRequest,
) -> Result<(), FrameError> {
    let request_id = *next_request_id;
    *next_request_id = (*next_request_id).wrapping_add(1).max(2);
    write_frame(
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
    events.try_send(event)?;
    published.event_sequence.store(sequence, Ordering::Release);
    published.request_semantic_wake();
    Ok(())
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
    use std::sync::atomic::AtomicUsize;

    use super::*;

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
    fn hidden_semantic_events_wake_and_are_drainable_without_visual_wakes() {
        let (published, wake_count) = published_state(false);
        let (event_tx, event_rx) = crossbeam_channel::bounded(EVENT_CAPACITY);

        published.publish(RenderSnapshot::detached(7));
        assert_eq!(wake_count.load(Ordering::Relaxed), 0);
        assert_eq!(published.acknowledged_generation.load(Ordering::Acquire), 0);

        let exited = TerminalEvent::Exited {
            exit_code: 1,
            signal: None,
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

    #[test]
    fn visual_and_semantic_wake_edges_are_independent() {
        let (published, wake_count) = published_state(true);
        let (event_tx, _event_rx) = crossbeam_channel::bounded(EVENT_CAPACITY);

        published.publish(RenderSnapshot::detached(1));
        accept_server_event(&event_tx, &published, 1, TerminalEvent::Bell).unwrap();

        assert_eq!(wake_count.load(Ordering::Relaxed), 2);
        assert!(published.wake_pending.load(Ordering::Acquire));
        assert!(published.semantic_wake_pending.load(Ordering::Acquire));
        published.begin_event_drain();
        assert!(published.wake_pending.load(Ordering::Acquire));
        assert!(!published.semantic_wake_pending.load(Ordering::Acquire));
        assert_eq!(published.acknowledged_generation.load(Ordering::Acquire), 0);
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
        command_tx
            .try_send(ClientCommand::Input(vec![b'x']))
            .unwrap();

        assert!(matches!(
            wait_for_client_work(&stop, &command_rx, &server_rx),
            ClientWork::Command(ClientCommand::Input(bytes)) if bytes == *b"x"
        ));
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
