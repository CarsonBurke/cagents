mod close_processes;
#[cfg(test)]
mod close_tests;

use close_processes::{FrozenFamily, RootIdentity};

use std::{
    cell::{Cell, RefCell},
    io::{BufWriter, Read, Write},
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
    path::PathBuf,
    rc::Rc,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use arc_swap::ArcSwap;
use crossbeam_channel::{Receiver, RecvTimeoutError, Sender, TryRecvError, TrySendError};
use libghostty_vt::{
    Terminal, TerminalOptions, focus,
    key::{self, Encoder as KeyEncoder},
    mouse::{self, Encoder as MouseEncoder},
    style::{PaletteIndex, RgbColor},
    terminal::{ColorScheme, CompressionMode, CompressionResult, Mode, ScrollViewport},
};
use portable_pty::{
    ChildKiller, CommandBuilder, ExitStatus, MasterPty, PtySize, native_pty_system,
};

use crate::{
    model::{LaunchRecipe, TerminalOutputActivity, TerminalSession, agent_recovery::AgentProcess},
    secure_file::open_private_append_file,
};

use super::{
    snapshot::{RenderSnapshot, VisualResources},
    types::{
        KeyAction, KeyInput, KeyModifiers, MouseAction, MouseButton, MouseInput, TerminalConfig,
        TerminalEvent, TerminalKey, TerminalSendError, TerminalSize, TerminalStartError,
        TerminalTheme, TerminalWake,
    },
};

const PTY_READ_SIZE: usize = 16 * 1024;
const PTY_BATCH_LIMIT: usize = 256 * 1024;
/// Marker appended once when a transcript hits its size cap.
const TRANSCRIPT_TRUNCATED_MARKER: &[u8] = b"\n[cagents transcript truncated: size cap reached]\n";
/// Foreground observation cadence. Every tick descends `/proc` over the PTY
/// foreground subtree, so it is only worth paying while that group is moving:
/// an unchanged group backs the interval off, any change resets it, and a
/// terminal nobody is watching is never observed on the timer at all.
const FOREGROUND_POLL_MIN: Duration = Duration::from_secs(2);
const FOREGROUND_POLL_MAX: Duration = Duration::from_secs(30);
/// How long shutdown waits for the PTY reader to leave its blocking read
/// before abandoning the thread. Closing its wake pipe releases the read
/// immediately in every case we can construct; this only bounds the ones we
/// cannot, because a hung join here wedges the daemon's dispatch thread.
const READER_SHUTDOWN_GRACE: Duration = Duration::from_millis(500);

#[derive(Debug)]
enum Command {
    Input(Vec<u8>),
    Key(KeyInput),
    Mouse(MouseInput),
    Focus(bool),
    Resize(TerminalSize),
    Scroll(ScrollCommand),
    SetTheme(TerminalTheme),
    Hide,
    Reveal,
    Hibernate,
    Restore {
        visible: bool,
    },
    ObserveAgent(Sender<Option<String>>),
    PrepareUpgrade(Sender<Result<UpgradeSnapshot, String>>),
    ObserveAgentProcess(Sender<Option<AgentProcess>>),
    ApplyAgentActivity {
        process: AgentProcess,
        activity: Option<crate::agent::Activity>,
        reply: Sender<Option<crate::agent::Activity>>,
        deadline: std::time::Instant,
    },
    CloseChecked {
        allow_active: bool,
        deadline: std::time::Instant,
        reply: Sender<super::ipc::TerminalCloseResult>,
    },
    Shutdown,
}

/// Reversible process freeze. Dropping an uncommitted snapshot resumes its
/// exact pidfd-bound family; only committed handoff is allowed to kill it.
pub(crate) struct UpgradeSnapshot {
    pub(crate) hint: Option<String>,
    pub(crate) active: bool,
    pub(crate) size: TerminalSize,
    pub(crate) theme: TerminalTheme,
    family: FrozenFamily,
}

impl UpgradeSnapshot {
    pub(crate) fn only_agent_tree(&self, pid: i32) -> bool {
        self.family.only_agent_tree(pid)
    }

    pub(crate) fn stop(&mut self) -> Result<(), String> {
        self.family
            .kill(std::time::Instant::now() + std::time::Duration::from_secs(10))
    }
}

#[derive(Clone, Copy, Debug)]
enum ScrollCommand {
    Top,
    Bottom,
    Delta(isize),
    Row(usize),
}

#[derive(Debug)]
enum PtyMessage {
    Bytes(Vec<u8>),
    Eof,
    Error(String),
}

type ChildExit = Result<ExitStatus, String>;

#[derive(Debug)]
enum WorkerEvent {
    Command(Option<Command>),
    Pty(Option<PtyMessage>),
    ChildExit(Option<ChildExit>),
    Capture,
    ObserveForeground,
}

#[derive(Debug)]
enum TranscriptMessage {
    Bytes(Vec<u8>),
    Shutdown,
}

#[derive(Debug)]
enum Effect {
    PtyWrite(Vec<u8>),
    Bell,
    Title(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Visibility {
    Visible,
    Hidden,
    Hibernated,
}

type WakeCallback = dyn Fn(TerminalWake) + Send + Sync + 'static;

struct PublishedState {
    snapshot: ArcSwap<RenderSnapshot>,
    published_generation: AtomicU64,
    acknowledged_generation: AtomicU64,
    capture_tx: Sender<()>,
    capture_rx: Receiver<()>,
    activity: Mutex<Option<TerminalOutputActivity>>,
    agent_activity: Mutex<Option<crate::agent::Activity>>,
    completed: AtomicBool,
    /// The newest generation a wake has already announced to the subscriber
    /// side of this protocol.
    ///
    /// Announcing each published generation exactly once is still one wake per
    /// drawn frame, because `capture_if_ready` withholds the next capture until
    /// that generation is acknowledged. A "a wake is pending" flag measured the
    /// same thing but could only be cleared by an acknowledgement, so an edge
    /// its callback dropped - a repaint broadcast while no subscriber was
    /// visible - suppressed every later generation, including the capture a
    /// reveal forces.
    announced_generation: AtomicU64,
    visible: AtomicBool,
    wake: Arc<WakeCallback>,
    output_observer: OnceLock<Arc<dyn Fn(u64) + Send + Sync>>,
    output_armed: AtomicBool,
}

impl PublishedState {
    fn new(visible: bool, wake: Arc<WakeCallback>) -> Self {
        let (capture_tx, capture_rx) = crossbeam_channel::bounded(1);
        Self {
            snapshot: ArcSwap::from_pointee(RenderSnapshot::empty()),
            published_generation: AtomicU64::new(0),
            acknowledged_generation: AtomicU64::new(0),
            capture_tx,
            capture_rx,
            activity: Mutex::new(None),
            agent_activity: Mutex::new(None),
            completed: AtomicBool::new(false),
            announced_generation: AtomicU64::new(0),
            visible: AtomicBool::new(visible),
            wake,
            output_observer: OnceLock::new(),
            output_armed: AtomicBool::new(false),
        }
    }

    fn publish(&self, snapshot: Arc<RenderSnapshot>) {
        let generation = snapshot.generation;
        // The immutable snapshot must be observable before the edge is emitted.
        self.snapshot.store(snapshot);
        self.published_generation
            .store(generation, Ordering::Release);
        self.request_wake(generation);
    }

    fn detach(&self, generation: u64) {
        self.snapshot
            .store(Arc::new(RenderSnapshot::detached(generation)));
        self.published_generation
            .store(generation, Ordering::Release);
    }

    fn request_wake(&self, generation: u64) {
        if !self.visible.load(Ordering::Acquire) {
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

    fn suppress_wakes(&self) {
        self.visible.store(false, Ordering::Release);
    }

    /// Records the generation the subscriber drew and releases the capture
    /// credit for the next one.
    ///
    /// It re-arms nothing: a publication announces its own generation, so
    /// there is no edge for an acknowledgement to re-open.
    fn acknowledge(&self, observed_generation: u64) {
        // A subscriber cannot acknowledge a generation this worker never
        // published; that would let the capture loop run ahead of the UI.
        let observed_generation =
            observed_generation.min(self.published_generation.load(Ordering::Acquire));
        let previous = self
            .acknowledged_generation
            .fetch_max(observed_generation, Ordering::AcqRel);
        if observed_generation > previous {
            let _ = self.capture_tx.try_send(());
        }
    }
}

struct HandleInner {
    commands: Sender<Command>,
    events: Receiver<TerminalEvent>,
    published: Arc<PublishedState>,
    worker: Mutex<Option<JoinHandle<()>>>,
}

impl Drop for HandleInner {
    fn drop(&mut self) {
        let on_worker_thread = self
            .worker
            .get_mut()
            .ok()
            .and_then(|worker| worker.as_ref())
            .is_some_and(|worker| worker.thread().id() == thread::current().id());
        if on_worker_thread {
            let _ = self.commands.try_send(Command::Shutdown);
        } else {
            let _ = self.commands.send(Command::Shutdown);
        }
        if let Ok(worker) = self.worker.get_mut()
            && let Some(worker) = worker.take()
            && worker.thread().id() != thread::current().id()
        {
            let _ = worker.join();
        }
    }
}

#[derive(Clone)]
pub struct TerminalHandle {
    inner: Arc<HandleInner>,
}

impl std::fmt::Debug for TerminalHandle {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TerminalHandle")
            .field("generation", &self.generation())
            .finish_non_exhaustive()
    }
}

impl TerminalHandle {
    pub fn spawn(
        session: TerminalSession,
        config: TerminalConfig,
        wake: impl Fn(TerminalWake) + Send + Sync + 'static,
    ) -> Result<Self, TerminalStartError> {
        let (command_tx, command_rx) = command_channel(config.command_capacity);
        // Semantic events are lossless and independent from visual wakes. They
        // are low-volume metadata; an unbounded queue avoids either dropping a
        // terminal exit/error or deadlocking teardown behind the UI receiver.
        let (event_tx, event_rx) = crossbeam_channel::unbounded();
        let published = Arc::new(PublishedState::new(
            config.initially_visible,
            Arc::new(wake),
        ));
        let worker_published = Arc::clone(&published);
        let (startup_tx, startup_rx) = std::sync::mpsc::sync_channel(1);

        let worker = thread::Builder::new()
            .name(format!("terminal-{:?}", session.id))
            .spawn(move || {
                let completion = Arc::clone(&worker_published);
                let result = Worker::start(session, config, command_rx, event_tx, worker_published);
                match result {
                    Ok(worker) => {
                        let _ = startup_tx.send(Ok(()));
                        worker.run();
                    }
                    Err(error) => {
                        let _ = startup_tx.send(Err(error));
                    }
                }
                // Only completed teardown is evidence that no worker/PTY
                // remains. An unexpected panic conservatively retains live
                // inventory rather than certifying unobserved cleanup.
                completion.completed.store(true, Ordering::Release);
            })
            .map_err(|error| TerminalStartError::Worker(error.to_string()))?;

        match startup_rx.recv() {
            Ok(Ok(())) => Ok(Self {
                inner: Arc::new(HandleInner {
                    commands: command_tx,
                    events: event_rx,
                    published,
                    worker: Mutex::new(Some(worker)),
                }),
            }),
            Ok(Err(error)) => {
                let _ = worker.join();
                Err(TerminalStartError::Worker(error))
            }
            Err(_) => {
                let _ = worker.join();
                Err(TerminalStartError::WorkerExited)
            }
        }
    }

    pub fn snapshot(&self) -> Arc<RenderSnapshot> {
        self.inner.published.snapshot.load_full()
    }

    pub fn activity(&self) -> Option<TerminalOutputActivity> {
        *self
            .inner
            .published
            .activity
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }

    pub(crate) fn agent_activity(&self) -> Option<crate::agent::Activity> {
        *self
            .inner
            .published
            .agent_activity
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }

    pub(crate) fn observe_agent_process(&self) -> Option<AgentProcess> {
        let (reply, result) = crossbeam_channel::bounded(1);
        let process = self
            .try_command(Command::ObserveAgentProcess(reply))
            .ok()
            .and_then(|()| result.recv_timeout(std::time::Duration::from_secs(3)).ok())
            .flatten();
        if process.is_none() {
            self.invalidate_agent_activity();
        }
        process
    }

    pub(crate) fn apply_agent_activity(
        &self,
        process: AgentProcess,
        activity: Option<crate::agent::Activity>,
    ) -> Option<crate::agent::Activity> {
        let (reply, result) = crossbeam_channel::bounded(1);
        let timeout = std::time::Duration::from_secs(3);
        let deadline = std::time::Instant::now() + timeout;
        if self
            .try_command(Command::ApplyAgentActivity {
                process,
                activity,
                reply,
                deadline,
            })
            .is_ok()
            && let Ok(activity) = result.recv_timeout(timeout)
        {
            return activity;
        }
        self.invalidate_agent_activity();
        None
    }

    fn invalidate_agent_activity(&self) {
        // A failed owner request cannot leave attach descriptors claiming work.
        // Its hosted response independently publishes the scalar clear.
        *self
            .inner
            .published
            .agent_activity
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = None;
    }

    /// True only after final capture, PTY teardown and reader/reaper cleanup.
    /// Foreground-process exit alone does not imply completion.
    pub fn is_completed(&self) -> bool {
        self.inner.published.completed.load(Ordering::Acquire)
    }

    pub fn generation(&self) -> u64 {
        self.inner
            .published
            .published_generation
            .load(Ordering::Acquire)
    }

    /// Acknowledges a delivered redraw edge after observing its snapshot.
    /// Call this even if the card became hidden before the edge was handled;
    /// the client calls it after drawing.
    pub fn acknowledge_redraw(&self, observed_generation: u64) {
        self.inner.published.acknowledge(observed_generation);
    }

    /// Install the host's lightweight observer before arming a subscription.
    pub(crate) fn set_output_observer(&self, observer: impl Fn(u64) + Send + Sync + 'static) {
        let _ = self.inner.published.output_observer.set(Arc::new(observer));
    }

    /// Arm one output edge and return its history boundary atomically with PTY
    /// activity publication. Captures and redraw credit are deliberately unrelated.
    pub(crate) fn observe_next_output(&self) -> u64 {
        let published = &self.inner.published;
        let activity = published
            .activity
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        published.output_armed.store(true, Ordering::Release);
        activity.map_or(0, |fact| fact.sequence)
    }

    pub(crate) fn stop_observing_output(&self) {
        self.inner
            .published
            .output_armed
            .store(false, Ordering::Release);
    }

    pub fn events(&self) -> Receiver<TerminalEvent> {
        self.inner.events.clone()
    }

    pub fn input(&self, bytes: impl Into<Vec<u8>>) -> Result<(), TerminalSendError> {
        self.try_command(Command::Input(bytes.into()))
    }

    pub fn key(&self, key: KeyInput) -> Result<(), TerminalSendError> {
        self.try_command(Command::Key(key))
    }

    pub fn mouse(&self, mouse: MouseInput) -> Result<(), TerminalSendError> {
        self.try_command(Command::Mouse(mouse))
    }

    pub fn focus(&self, focused: bool) -> Result<(), TerminalSendError> {
        self.try_command(Command::Focus(focused))
    }

    pub fn resize(&self, size: TerminalSize) -> Result<(), TerminalSendError> {
        self.try_command(Command::Resize(size))
    }

    pub fn scroll_lines(&self, delta: isize) -> Result<(), TerminalSendError> {
        self.try_command(Command::Scroll(ScrollCommand::Delta(delta)))
    }

    pub fn scroll_to_top(&self) -> Result<(), TerminalSendError> {
        self.try_command(Command::Scroll(ScrollCommand::Top))
    }

    pub fn scroll_to_bottom(&self) -> Result<(), TerminalSendError> {
        self.try_command(Command::Scroll(ScrollCommand::Bottom))
    }

    pub fn scroll_to_row(&self, row: usize) -> Result<(), TerminalSendError> {
        self.try_command(Command::Scroll(ScrollCommand::Row(row)))
    }

    pub fn set_theme(&self, theme: TerminalTheme) -> Result<(), TerminalSendError> {
        self.try_command(Command::SetTheme(theme))
    }

    pub fn hide(&self) -> Result<(), TerminalSendError> {
        self.try_command(Command::Hide)
    }

    pub fn reveal(&self) -> Result<(), TerminalSendError> {
        self.try_command(Command::Reveal)
    }

    pub fn hibernate(&self) -> Result<(), TerminalSendError> {
        self.try_command(Command::Hibernate)
    }

    pub fn restore(&self, visible: bool) -> Result<(), TerminalSendError> {
        self.try_command(Command::Restore { visible })
    }

    /// Resolve fresh foreground evidence on the PTY owner thread. This never
    /// sends terminal input or starts a provider process.
    pub(crate) fn observe_agent(&self) -> Result<Option<String>, String> {
        let (sender, receiver) = crossbeam_channel::bounded(1);
        self.try_command(Command::ObserveAgent(sender))
            .map_err(|error| error.to_string())?;
        receiver
            .recv_timeout(std::time::Duration::from_secs(3))
            .map_err(|error| format!("Cannot inspect the active agent: {error}"))
    }

    pub(crate) fn prepare_upgrade(&self) -> Result<UpgradeSnapshot, String> {
        let (sender, receiver) = crossbeam_channel::bounded(1);
        self.try_command(Command::PrepareUpgrade(sender))
            .map_err(|error| error.to_string())?;
        receiver
            .recv_timeout(std::time::Duration::from_secs(5))
            .map_err(|error| format!("Cannot freeze terminal for upgrade: {error}"))?
    }

    /// Inspection and the close decision are one serialized PTY-owner command.
    pub(crate) fn close_checked(&self, allow_active: bool) -> super::ipc::TerminalCloseResult {
        use super::ipc::TerminalCloseResult;
        let (reply, result) = crossbeam_channel::bounded(1);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        if let Err(error) = self.try_command(Command::CloseChecked {
            allow_active,
            deadline,
            reply,
        }) {
            return TerminalCloseResult::Unknown(error.to_string());
        }
        let outcome = result
            .recv_timeout(std::time::Duration::from_secs(3))
            .unwrap_or_else(|error| TerminalCloseResult::Unknown(error.to_string()));
        if outcome == TerminalCloseResult::Closed {
            // Completion is not reported until the owner has released its PTY.
            match self.inner.worker.lock() {
                Ok(mut worker) => {
                    if let Some(worker) = worker.take() {
                        if worker.join().is_err() {
                            return TerminalCloseResult::Unknown(
                                "Terminal worker failed during close".into(),
                            );
                        }
                    }
                }
                Err(_) => {
                    return TerminalCloseResult::Unknown("Terminal worker unavailable".into());
                }
            }
        }
        outcome
    }

    pub fn shutdown(&self) -> Result<(), TerminalSendError> {
        self.send_command(Command::Shutdown)?;
        if let Ok(mut worker) = self.inner.worker.lock()
            && let Some(worker) = worker.take()
            && worker.thread().id() != thread::current().id()
        {
            let _ = worker.join();
        }
        Ok(())
    }

    fn try_command(&self, command: Command) -> Result<(), TerminalSendError> {
        self.inner
            .commands
            .try_send(command)
            .map_err(|error| match error {
                TrySendError::Full(_) => TerminalSendError::Full,
                TrySendError::Disconnected(_) => TerminalSendError::Closed,
            })
    }

    fn send_command(&self, command: Command) -> Result<(), TerminalSendError> {
        self.inner
            .commands
            .send(command)
            .map_err(|_| TerminalSendError::Closed)
    }
}

fn command_channel(capacity: usize) -> (Sender<Command>, Receiver<Command>) {
    crossbeam_channel::bounded(capacity.max(1))
}

struct Worker {
    terminal_id: crate::model::TerminalId,
    launch: LaunchRecipe,
    resume_hint: Option<String>,
    resume_foreground: Option<libc::pid_t>,
    /// Current timer interval for foreground observation. Grows while the
    /// foreground process group is unchanged and no agent was found.
    foreground_poll: Duration,
    agent_provider: Option<crate::agent::Provider>,
    agent_process: Option<AgentProcess>,
    display_title: String,
    agent_exited: bool,
    terminal: Terminal<'static, 'static>,
    theme: Rc<Cell<TerminalTheme>>,
    key_encoder: KeyEncoder<'static>,
    mouse_encoder: MouseEncoder<'static>,
    effects: Rc<RefCell<Vec<Effect>>>,
    visual: Option<VisualResources>,
    visibility: Visibility,
    generation: u64,
    capture_dirty: bool,
    force_capture: bool,
    /// Size the widget last asked for; the size a resumed or upgraded terminal
    /// is rebuilt with.
    current_size: TerminalSize,
    /// Sizes actually in effect. A failed ioctl must never be remembered as
    /// applied: the widget re-sends the same steady-state size, and a cached
    /// failure would short-circuit that retry and leave the child on its old
    /// SIGWINCH dimensions forever.
    applied_pty_size: TerminalSize,
    applied_parser_size: TerminalSize,
    commands: Receiver<Command>,
    pty_rx: Receiver<PtyMessage>,
    events: Sender<TerminalEvent>,
    transcript: Option<TranscriptWriter>,
    published: Arc<PublishedState>,
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    child_killer: Box<dyn ChildKiller + Send + Sync>,
    child_exit_rx: Receiver<ChildExit>,
    child_identity: Option<RootIdentity>,
    checked_shutdown: bool,
    child_reaper: Option<JoinHandle<()>>,
    reader: Option<JoinHandle<()>>,
    /// Write end of the reader's wake pipe. Closing it releases a read that a
    /// disowned grandchild would otherwise hold open indefinitely.
    reader_wake: Option<OwnedFd>,
    /// Disconnects when the reader thread returns, so shutdown can wait for
    /// it with a deadline instead of joining unconditionally.
    reader_done: Receiver<()>,
}

impl Worker {
    fn start(
        session: TerminalSession,
        config: TerminalConfig,
        commands: Receiver<Command>,
        events: Sender<TerminalEvent>,
        published: Arc<PublishedState>,
    ) -> Result<Self, String> {
        let current_size = TerminalSize {
            columns: config.columns.max(1),
            rows: config.rows.max(1),
            cell_width_px: config.cell_width_px.max(1),
            cell_height_px: config.cell_height_px.max(1),
        };
        let mut terminal = Terminal::new(TerminalOptions {
            cols: current_size.columns,
            rows: current_size.rows,
            max_scrollback: config.max_scrollback,
        })
        .map_err(|error| format!("create Ghostty terminal: {error}"))?;
        apply_theme(&mut terminal, config.theme)
            .map_err(|error| format!("configure terminal theme: {error}"))?;
        let theme = Rc::new(Cell::new(config.theme));
        terminal
            .on_color_scheme({
                let theme = Rc::clone(&theme);
                move |_| Some(theme_scheme(theme.get()))
            })
            .map_err(|error| format!("register color scheme query: {error}"))?;

        let effects = Rc::new(RefCell::new(Vec::new()));
        terminal
            .on_pty_write({
                let effects = Rc::clone(&effects);
                move |_terminal, bytes| {
                    effects.borrow_mut().push(Effect::PtyWrite(bytes.to_vec()));
                }
            })
            .map_err(|error| format!("register PTY response effect: {error}"))?;
        terminal
            .on_bell({
                let effects = Rc::clone(&effects);
                move |_terminal| effects.borrow_mut().push(Effect::Bell)
            })
            .map_err(|error| format!("register bell effect: {error}"))?;
        terminal
            .on_title_changed({
                let effects = Rc::clone(&effects);
                move |terminal| {
                    let title = terminal.title().unwrap_or_default().to_owned();
                    effects.borrow_mut().push(Effect::Title(title));
                }
            })
            .map_err(|error| format!("register title effect: {error}"))?;

        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(pty_size(current_size))
            .map_err(|error| format!("open PTY: {error:#}"))?;
        let mut command = match &config.execution_environment {
            Some(environment) => {
                let mut command = CommandBuilder::new(&session.launch.program);
                command.args(&session.launch.arguments);
                command.cwd(&session.launch.working_directory);
                command.env_clear();
                for (name, value) in &environment.0 {
                    command.env(name, value);
                }
                scrub_inherited_markers(&mut command);
                terminal_environment(&mut command);
                command
            }
            None => command_from_recipe(&session.launch),
        };
        command.env("CAGENTS_TERMINAL_ID", session.id.to_string());
        let executable = interactive_shell_launch(&session.launch)
            .then(|| RootIdentity::launch_executable(&command))
            .flatten();
        let mut child = pair
            .slave
            .spawn_command(command)
            .map_err(|error| format!("spawn {}: {error:#}", session.launch.program))?;
        let child_identity = child
            .process_id()
            .and_then(|pid| RootIdentity::capture(pid as libc::pid_t, executable).ok());
        drop(pair.slave);
        let mut reader = pair
            .master
            .try_clone_reader()
            .map_err(|error| format!("clone PTY reader: {error:#}"))?;
        let writer = pair
            .master
            .take_writer()
            .map_err(|error| format!("take PTY writer: {error:#}"))?;

        // `try_clone_reader` hides its descriptor, and closing the master does
        // not release a read already blocked on the clone: a PTY master read
        // only fails with EIO once the last slave descriptor closes, which a
        // disowned grandchild can defer forever. Wait for readiness on a
        // descriptor this worker hands over instead, alongside a pipe whose
        // closure is the shutdown signal, so the reader always has a way out.
        let readiness = duplicate_descriptor(&*pair.master)?;
        let (wake_rx, wake_tx) = wake_pipe()?;
        let (pty_tx, pty_rx) = crossbeam_channel::bounded(config.pty_capacity.max(1));
        let (reader_done_tx, reader_done) = crossbeam_channel::bounded::<()>(0);
        let reader_thread = thread::Builder::new()
            .name(format!("terminal-pty-reader-{:?}", session.id))
            .spawn(move || {
                let _done = reader_done_tx;
                read_pty(&mut reader, &pty_tx, readiness, wake_rx);
            })
            .map_err(|error| format!("start PTY reader: {error}"))?;

        let transcript = session
            .transcript_path
            .map(|path| {
                TranscriptWriter::start(
                    path,
                    config.transcript_capacity.max(1),
                    config.transcript_max_bytes.max(1),
                )
            })
            .transpose()?;
        let visibility = if config.initially_visible {
            Visibility::Visible
        } else {
            Visibility::Hidden
        };
        let visual = if visibility == Visibility::Visible {
            Some(VisualResources::new().map_err(|error| format!("create render state: {error}"))?)
        } else {
            None
        };
        let key_encoder =
            KeyEncoder::new().map_err(|error| format!("create key encoder: {error}"))?;
        let mouse_encoder =
            MouseEncoder::new().map_err(|error| format!("create mouse encoder: {error}"))?;

        let child_killer = child.clone_killer();
        let (child_exit_tx, child_exit_rx) = crossbeam_channel::bounded(1);
        let child_reaper = thread::Builder::new()
            .name(format!("terminal-child-reaper-{:?}", session.id))
            .spawn(move || {
                let result = child.wait().map_err(|error| error.to_string());
                let _ = child_exit_tx.send(result);
            })
            .map_err(|error| format!("start child reaper: {error}"))?;

        Ok(Self {
            terminal_id: session.id,
            resume_hint: crate::model::agent_recovery::observed_hint_from_launch(&session.launch),
            resume_foreground: None,
            foreground_poll: FOREGROUND_POLL_MIN,
            agent_provider: None,
            agent_process: None,
            display_title: String::new(),
            agent_exited: false,
            launch: session.launch,
            terminal,
            theme,
            key_encoder,
            mouse_encoder,
            effects,
            visual,
            visibility,
            generation: 0,
            capture_dirty: false,
            force_capture: false,
            current_size,
            applied_pty_size: current_size,
            applied_parser_size: current_size,
            commands,
            pty_rx,
            events,
            transcript,
            published,
            master: pair.master,
            writer,
            child_killer,
            child_exit_rx,
            child_identity,
            checked_shutdown: false,
            child_reaper: Some(child_reaper),
            reader: Some(reader_thread),
            reader_wake: Some(wake_tx),
            reader_done,
        })
    }

    fn run(mut self) {
        if self.visibility == Visibility::Visible {
            self.publish_snapshot(true);
        }

        let mut pty_open = true;
        let mut exited = false;
        let mut shutting_down = false;
        let mut carried_command = None;
        while !shutting_down && (pty_open || !exited) {
            let event = carried_command
                .take()
                .map(|command| WorkerEvent::Command(Some(command)))
                .unwrap_or_else(|| {
                    wait_for_worker_event(
                        &self.commands,
                        &self.pty_rx,
                        &self.child_exit_rx,
                        &self.published.capture_rx,
                        pty_open,
                        !exited,
                        self.foreground_poll,
                    )
                });
            match event {
                WorkerEvent::Command(Some(command)) => {
                    // Wheel bursts otherwise queue one repaint per event. Fold
                    // adjacent viewport deltas before publishing a snapshot.
                    let command =
                        coalesce_scroll_command(command, &self.commands, &mut carried_command);
                    shutting_down = self.handle_command(command);
                }
                WorkerEvent::Command(None) => shutting_down = true,
                WorkerEvent::Capture => self.capture_if_ready(),
                WorkerEvent::ObserveForeground => self.observe_foreground(),
                WorkerEvent::Pty(message) => {
                    pty_open = self.handle_pty_message(message);
                }
                WorkerEvent::ChildExit(message) => {
                    self.handle_child_exit(
                        &mut exited,
                        message.unwrap_or_else(|| Err("child reaper closed".into())),
                    );
                }
            }
        }

        // The parser is about to disappear. Preserve its final grid even when
        // a consumer has not returned credit for the preceding frame.
        if self.capture_dirty {
            // A hidden terminal still needs a recoverable final grid once its
            // parser is destroyed. Do not re-enable its visual wake callback.
            if self.visual.is_none() {
                match VisualResources::new() {
                    Ok(visual) => self.visual = Some(visual),
                    Err(error) => self.error("build final terminal snapshot", error.to_string()),
                }
            }
            self.visibility = Visibility::Visible;
            self.published
                .acknowledged_generation
                .store(self.generation, Ordering::Release);
            self.capture_if_ready();
        }
        if shutting_down && !exited {
            match self.child_exit_rx.try_recv() {
                Ok(result) => self.handle_child_exit(&mut exited, result),
                Err(TryRecvError::Disconnected) => {
                    self.handle_child_exit(&mut exited, Err("child reaper closed".into()));
                }
                Err(TryRecvError::Empty) if self.checked_shutdown => {
                    let result = self
                        .child_exit_rx
                        .recv()
                        .unwrap_or_else(|_| Err("child reaper closed".into()));
                    self.handle_child_exit(&mut exited, result);
                }
                Err(TryRecvError::Empty) => match self.child_killer.kill() {
                    Ok(()) => {
                        let result = self
                            .child_exit_rx
                            .recv()
                            .unwrap_or_else(|_| Err("child reaper closed".into()));
                        self.handle_child_exit(&mut exited, result);
                    }
                    Err(error) => match self.child_exit_rx.try_recv() {
                        Ok(result) => self.handle_child_exit(&mut exited, result),
                        Err(_) => self.error("terminate child", error.to_string()),
                    },
                },
            }
        }
        if exited && let Some(child_reaper) = self.child_reaper.take() {
            let _ = child_reaper.join();
        }
        if let Some(transcript) = self.transcript.take() {
            transcript.finish();
        }

        // Release the reader's blocking wait before waiting on it, then bound
        // that wait: a reader still stuck on a descriptor a disowned
        // grandchild holds open is abandoned rather than joined forever,
        // because a hung join here wedges the daemon's dispatch thread.
        drop(self.reader_wake.take());
        if let Some(reader) = self.reader.take() {
            match self.reader_done.recv_timeout(READER_SHUTDOWN_GRACE) {
                Err(RecvTimeoutError::Timeout) => tracing::warn!(
                    terminal_id = %self.terminal_id,
                    "abandoning PTY reader thread still blocked on its descriptor"
                ),
                _ => {
                    let _ = reader.join();
                }
            }
        }
        drop(self.writer);
        drop(self.master);
        // Terminal, render state, and every Ghostty encoder drop here, on the
        // same OS thread on which they were constructed and used.
    }

    fn handle_child_exit(&mut self, exited: &mut bool, result: ChildExit) {
        let intentional = self
            .child_identity
            .as_ref()
            .is_some_and(RootIdentity::intentionally_killed);
        if let Some(event) = child_exit_event(exited, result, intentional) {
            self.agent_exited = true;
            self.agent_provider = None;
            self.agent_process = None;
            self.set_agent_activity(None);
            self.emit(event);
        }
    }

    fn handle_pty_message(&mut self, message: Option<PtyMessage>) -> bool {
        match message {
            Some(PtyMessage::Bytes(mut bytes)) => {
                while bytes.len() < PTY_BATCH_LIMIT {
                    match self.pty_rx.try_recv() {
                        Ok(PtyMessage::Bytes(next)) => bytes.extend_from_slice(&next),
                        Ok(PtyMessage::Eof) => {
                            self.process_output(bytes);
                            return false;
                        }
                        Ok(PtyMessage::Error(error)) => {
                            self.error("read PTY", error);
                            self.process_output(bytes);
                            return false;
                        }
                        Err(TryRecvError::Empty) => break,
                        Err(TryRecvError::Disconnected) => {
                            self.process_output(bytes);
                            return false;
                        }
                    }
                }
                self.process_output(bytes);
                true
            }
            Some(PtyMessage::Eof) | None => false,
            Some(PtyMessage::Error(error)) => {
                self.error("read PTY", error);
                false
            }
        }
    }

    fn process_output(&mut self, bytes: Vec<u8>) {
        if bytes.is_empty() {
            return;
        }
        let observed_sequence = {
            let mut activity = self
                .published
                .activity
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            let sequence = activity.map_or(1, |fact| fact.sequence.saturating_add(1));
            let last_output_at_unix_ms = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|elapsed| elapsed.as_millis().min(i64::MAX as u128) as i64)
                .unwrap_or(-1);
            *activity = Some(TerminalOutputActivity {
                sequence,
                last_output_at_unix_ms,
            });
            (self.published.output_armed.load(Ordering::Acquire)
                && self.published.output_armed.swap(false, Ordering::AcqRel))
            .then_some(sequence)
        };
        if let Some(sequence) = observed_sequence
            && let Some(observer) = self.published.output_observer.get()
        {
            observer(sequence);
        }

        let parse_started =
            tracing::enabled!(target: "cagents::terminal_pipeline", tracing::Level::TRACE)
                .then(std::time::Instant::now);
        self.terminal.vt_write(&bytes);
        if let Some(started) = parse_started {
            tracing::trace!(target: "cagents::terminal_pipeline", stage = "parse",
                terminal_id = %self.terminal_id, bytes = bytes.len(),
                elapsed_ns = started.elapsed().as_nanos() as u64);
        }

        // Hand the batch over instead of copying up to PTY_BATCH_LIMIT bytes
        // into a channel that drops them under exactly the flood that makes
        // the copy expensive, and that records nothing at all past its cap.
        if let Some(transcript) = &self.transcript
            && transcript.accepts()
            && transcript.send(bytes).is_err()
        {
            self.error("write transcript", "transcript writer closed".into());
            self.transcript = None;
        }

        self.drain_effects();
        self.publish_snapshot(false);
    }

    /// Returns true when shutdown was requested.
    fn handle_command(&mut self, command: Command) -> bool {
        match command {
            Command::ApplyAgentActivity {
                process,
                activity,
                reply,
                deadline,
            } => {
                let current = self.observe_agent_process();
                let activity = if std::time::Instant::now() < deadline && current == Some(&process)
                {
                    activity
                } else {
                    None
                };
                self.set_agent_activity(activity);
                let _ = reply.send(
                    *self
                        .published
                        .agent_activity
                        .lock()
                        .unwrap_or_else(|error| error.into_inner()),
                );
            }
            Command::CloseChecked {
                allow_active,
                deadline,
                reply,
            } => {
                use super::ipc::TerminalCloseResult;
                let outcome = match self.inspect_close(deadline) {
                    Ok((true, _)) if !allow_active => TerminalCloseResult::Active,
                    Ok((_, mut family)) => match family.kill(deadline) {
                        Ok(()) => {
                            self.checked_shutdown = true;
                            TerminalCloseResult::Closed
                        }
                        Err(detail) => TerminalCloseResult::Unknown(detail),
                    },
                    Err(detail) => TerminalCloseResult::Unknown(detail),
                };
                let close = outcome == TerminalCloseResult::Closed;
                let _ = reply.send(outcome);
                return close;
            }
            Command::PrepareUpgrade(reply) => {
                let result = self
                    .inspect_close(std::time::Instant::now() + std::time::Duration::from_secs(3))
                    .and_then(|(active, family)| {
                        self.refresh_resume_hint(true);
                        if active && self.resume_hint.as_deref()
                            .and_then(crate::model::agent_recovery::recipe_from_hint).is_some()
                            && !self.agent_process.as_ref().is_some_and(|process| family.only_agent_tree(process.pid))
                        {
                            return Err("Terminal also owns unrelated background jobs; finish or close them before upgrading".into());
                        }
                        Ok(UpgradeSnapshot {
                            hint: self.resume_hint.clone(),
                            size: self.current_size,
                            theme: self.theme.get(),
                            active,
                            family,
                        })
                    });
                let _ = reply.send(result);
            }
            Command::ObserveAgent(sender) => {
                self.refresh_resume_hint(true);
                let _ = sender.send(self.resume_hint.clone());
            }
            Command::ObserveAgentProcess(sender) => {
                let process = self.observe_agent_process().cloned();
                let _ = sender.send(process);
            }
            Command::Input(bytes) => self.write_pty(&bytes, "write terminal input"),
            Command::Key(input) => self.encode_key(input),
            Command::Mouse(input) => self.encode_mouse(input),
            Command::Focus(focused) => {
                if focused {
                    self.refresh_resume_hint(true);
                }
                match self.terminal.mode(Mode::FOCUS_EVENT) {
                    Ok(true) => {}
                    Ok(false) => return false,
                    Err(error) => {
                        self.error("read focus-reporting mode", error.to_string());
                        return false;
                    }
                }
                let mut buffer = [0u8; 8];
                let event = if focused {
                    focus::Event::Gained
                } else {
                    focus::Event::Lost
                };
                match event.encode(&mut buffer) {
                    Ok(written) => self.write_pty(&buffer[..written], "write focus report"),
                    Err(error) => self.error("encode focus report", error.to_string()),
                }
            }
            Command::Resize(size) => self.resize(size),
            Command::Scroll(scroll) => {
                self.terminal.scroll_viewport(match scroll {
                    ScrollCommand::Top => ScrollViewport::Top,
                    ScrollCommand::Bottom => ScrollViewport::Bottom,
                    ScrollCommand::Delta(delta) => ScrollViewport::Delta(delta),
                    ScrollCommand::Row(row) => ScrollViewport::Row(row),
                });
                if self.visibility == Visibility::Visible {
                    self.publish_snapshot(false);
                }
            }
            Command::SetTheme(theme) => {
                if self.theme.get() != theme {
                    match apply_theme(&mut self.terminal, theme) {
                        Ok(()) => {
                            self.theme.set(theme);
                            if self
                                .terminal
                                .mode(Mode::COLOR_SCHEME_REPORT)
                                .unwrap_or(false)
                            {
                                let mut buffer = [0; 32];
                                if let Ok(len) = theme_scheme(theme).encode_report(&mut buffer) {
                                    self.write_pty(&buffer[..len], "report terminal color scheme");
                                }
                            }
                            self.publish_snapshot(true);
                        }
                        Err(error) => self.error("set terminal theme", error.to_string()),
                    }
                }
            }
            Command::Hide => self.set_hidden(),
            Command::Reveal => self.reveal(),
            Command::Hibernate => self.hibernate_visuals(),
            Command::Restore { visible } => self.restore_visuals(visible),
            Command::Shutdown => return true,
        }
        false
    }

    fn encode_key(&mut self, input: KeyInput) {
        match encode_key_input(&self.terminal, &mut self.key_encoder, input) {
            Ok(encoded) => self.write_pty(&encoded, "write encoded key"),
            Err(error) => self.error("encode key", error.to_string()),
        }
    }

    fn encode_mouse(&mut self, input: MouseInput) {
        let mut event = match mouse::Event::new() {
            Ok(event) => event,
            Err(error) => {
                self.error("create mouse event", error.to_string());
                return;
            }
        };
        event
            .set_action(map_mouse_action(input.action))
            .set_button(input.button.map(map_mouse_button))
            .set_mods(map_modifiers(input.modifiers))
            .set_position(mouse::Position {
                x: input.x,
                y: input.y,
            });
        self.mouse_encoder
            .set_options_from_terminal(&self.terminal)
            .set_any_button_pressed(input.any_button_pressed)
            .set_size(mouse::EncoderSize {
                screen_width: input.screen_width.max(1),
                screen_height: input.screen_height.max(1),
                cell_width: input.cell_width.max(1),
                cell_height: input.cell_height.max(1),
                padding_top: 0,
                padding_bottom: 0,
                padding_right: 0,
                padding_left: 0,
            });
        let mut encoded = Vec::with_capacity(32);
        match self.mouse_encoder.encode_to_vec(&event, &mut encoded) {
            Ok(()) => self.write_pty(&encoded, "write encoded mouse event"),
            Err(error) => self.error("encode mouse event", error.to_string()),
        }
    }

    fn resize(&mut self, mut size: TerminalSize) {
        size.columns = size.columns.max(1);
        size.rows = size.rows.max(1);
        size.cell_width_px = size.cell_width_px.max(1);
        size.cell_height_px = size.cell_height_px.max(1);
        if size == self.applied_pty_size && size == self.applied_parser_size {
            return;
        }
        // Record only what actually took effect. A failed ioctl cached as the
        // current size would swallow every later identical request, including
        // the steady-state size the widget re-sends, and the child would keep
        // its old SIGWINCH dimensions for the rest of its life.
        if size != self.applied_pty_size {
            match self.master.resize(pty_size(size)) {
                Ok(()) => self.applied_pty_size = size,
                Err(error) => self.error("resize PTY", format!("{error:#}")),
            }
        }
        if size != self.applied_parser_size {
            match self.terminal.resize(
                size.columns,
                size.rows,
                size.cell_width_px,
                size.cell_height_px,
            ) {
                Ok(()) => self.applied_parser_size = size,
                Err(error) => self.error("resize terminal", error.to_string()),
            }
        }
        self.current_size = size;
        if self.visibility == Visibility::Visible {
            self.publish_snapshot(true);
        }
    }

    fn set_hidden(&mut self) {
        self.visibility = Visibility::Hidden;
        self.published.suppress_wakes();
    }

    fn reveal(&mut self) {
        self.published
            .acknowledged_generation
            .store(self.generation, Ordering::Release);
        if self.visual.is_none() {
            match VisualResources::new() {
                Ok(visual) => self.visual = Some(visual),
                Err(error) => {
                    self.error("restore render state", error.to_string());
                    return;
                }
            }
        }
        self.visibility = Visibility::Visible;
        // Observe promptly again: whatever ran in the background while this
        // card was away is what the user is now looking at.
        self.foreground_poll = FOREGROUND_POLL_MIN;
        self.published.visible.store(true, Ordering::Release);
        self.publish_snapshot(true);
    }

    fn hibernate_visuals(&mut self) {
        self.visibility = Visibility::Hibernated;
        self.published.suppress_wakes();
        self.visual = None;
        self.generation = self.generation.saturating_add(1);
        self.published.detach(self.generation);
        // Render state is the small half. The parser's scrollback pool is tens
        // of MiB for a wide grid and, with `max_scrollback` fixed at
        // construction, is otherwise retained for as long as the card exists.
        //
        // libghostty-vt 0.2.1 exposes no way to lower that limit or drop the
        // history tail, and rebuilding the parser from the viewport is not an
        // option for a card that can be revealed again: its child is still
        // running, so modes, the alternate screen, cursor state and keyboard
        // and mouse protocol state would all be lost under a live TUI. A full
        // compression pass is the reclamation that keeps the contract - it
        // changes only the storage representation, never the logical
        // contents - and its one synchronous scan is affordable exactly here,
        // where nothing is waiting on a frame.
        match self.terminal.compress(CompressionMode::Full) {
            Ok(CompressionResult::Unsupported) => tracing::debug!(
                terminal_id = %self.terminal_id,
                "scrollback reclamation is unavailable on this target"
            ),
            Ok(_) => {}
            Err(error) => tracing::debug!(
                terminal_id = %self.terminal_id,
                %error,
                "hibernated terminal could not compress its scrollback"
            ),
        }
    }

    fn restore_visuals(&mut self, visible: bool) {
        if visible {
            self.reveal();
        } else {
            self.set_hidden();
        }
    }

    fn publish_snapshot(&mut self, force: bool) {
        self.capture_dirty = true;
        self.force_capture |= force;
        self.capture_if_ready();
    }

    fn capture_if_ready(&mut self) {
        if !self.capture_dirty
            || self
                .published
                .acknowledged_generation
                .load(Ordering::Acquire)
                < self.generation
        {
            return;
        }
        if self.visibility != Visibility::Visible {
            return;
        }
        let Some(visual) = self.visual.as_mut() else {
            return;
        };
        let next_generation = self.generation.saturating_add(1);
        let activity = *self
            .published
            .activity
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let capture_started =
            tracing::enabled!(target: "cagents::terminal_pipeline", tracing::Level::TRACE)
                .then(std::time::Instant::now);
        let captured = visual.capture(
            &self.terminal,
            next_generation,
            self.force_capture,
            activity,
        );
        if let Some(started) = capture_started {
            tracing::trace!(target: "cagents::terminal_pipeline", stage = "capture",
                terminal_id = %self.terminal_id, generation = next_generation,
                activity_sequence = activity.map_or(0, |fact| fact.sequence),
                published = matches!(&captured, Ok(Some(_))),
                elapsed_ns = started.elapsed().as_nanos() as u64);
        }
        match captured {
            Ok(Some(snapshot)) => {
                self.capture_dirty = false;
                self.force_capture = false;
                self.generation = next_generation;
                self.published.publish(snapshot);
            }
            Ok(None) => {
                self.capture_dirty = false;
                self.force_capture = false;
            }
            Err(error) => self.error("build terminal snapshot", error.to_string()),
        }
    }

    fn drain_effects(&mut self) {
        let effects = std::mem::take(&mut *self.effects.borrow_mut());
        for effect in effects {
            match effect {
                Effect::PtyWrite(bytes) => self.write_pty(&bytes, "write terminal response"),
                Effect::Bell => self.emit(TerminalEvent::Bell),
                Effect::Title(title) => {
                    let activity = self.agent_provider.and_then(|provider| {
                        provider
                            .activity_from_title(&title)
                            .map(|state| crate::agent::Activity { provider, state })
                    });
                    let display_changed = match agent_title_label(&title, activity) {
                        Some("") => self.display_title != "π",
                        Some(label) => self.display_title.strip_prefix("π ") != Some(label),
                        None => self.display_title != title,
                    };
                    let previous = *self
                        .published
                        .agent_activity
                        .lock()
                        .unwrap_or_else(|error| error.into_inner());
                    if display_changed || activity != previous {
                        self.refresh_resume_hint(true);
                    }
                    let observed = self.agent_provider.and_then(|provider| {
                        provider
                            .activity_from_title(&title)
                            .map(|state| crate::agent::Activity { provider, state })
                    });
                    if self.agent_provider == Some(crate::agent::Provider::Omp) {
                        self.set_agent_activity(
                            self.agent_process
                                .as_ref()
                                .filter(|process| process.provider == crate::agent::Provider::Omp)
                                .and(observed),
                        );
                    }
                    if display_changed || observed != activity {
                        let display = canonical_agent_title(&title, observed);
                        if display != self.display_title {
                            self.display_title = display.clone();
                            self.emit(TerminalEvent::TitleChanged(display));
                        }
                    }
                }
            }
        }
    }

    fn inspect_close(&self, deadline: std::time::Instant) -> Result<(bool, FrozenFamily), String> {
        let root = self
            .child_identity
            .as_ref()
            .ok_or("Child identity unavailable")?;
        let family = FrozenFamily::capture(root, deadline)?;
        let fd = self
            .master
            .as_raw_fd()
            .ok_or("PTY descriptor unavailable")?;
        // SAFETY: this worker owns the live PTY throughout the frozen census.
        let foreground = unsafe { libc::tcgetpgrp(fd) };
        if foreground <= 0 {
            return Err("Foreground process group unavailable".into());
        }
        // Process-only policy deliberately permits terminating shell builtins.
        // The original executable check still prevents an exec'd agent or
        // command from inheriting an idle shell's authority.
        let idle_shell = interactive_shell_launch(&self.launch) && root.original_executable();
        Ok((
            family.has_jobs() || foreground != root.process.group || !idle_shell,
            family,
        ))
    }

    fn foreground_group(&self) -> Option<libc::pid_t> {
        self.master.as_raw_fd().and_then(|fd| {
            // SAFETY: this worker owns the live master descriptor.
            let group = unsafe { libc::tcgetpgrp(fd) };
            (group > 0).then_some(group)
        })
    }

    fn observe_agent_process(&mut self) -> Option<&AgentProcess> {
        let group = self.foreground_group();
        let process = if self.agent_exited {
            None
        } else {
            group
                .and_then(crate::model::agent_recovery::foreground_agent)
                // A group that moved mid-walk invalidates what the walk saw.
                .filter(|_| self.foreground_group() == group)
        };
        self.adopt_agent_process(process);
        self.agent_process.as_ref()
    }

    fn adopt_agent_process(&mut self, process: Option<AgentProcess>) {
        if process.is_none() || process != self.agent_process {
            self.set_agent_activity(None);
        }
        self.agent_process = process;
    }

    /// Timer-driven observation. The descent is only worth its cost while the
    /// foreground group is moving, so an unchanged group backs the interval
    /// off and skips the walk outright once it has found no agent to track.
    /// A terminal nobody is watching is never observed here at all: it can
    /// only change what a snapshot nobody reads would have shown.
    fn observe_foreground(&mut self) {
        if self.visibility != Visibility::Visible || self.agent_exited {
            // Nothing here can change anything anyone reads, so let the timer
            // settle to its longest interval rather than waking every 2s for
            // the rest of this terminal's life.
            self.foreground_poll = FOREGROUND_POLL_MAX;
            return;
        }
        if self.foreground_group() == self.resume_foreground {
            self.foreground_poll = (self.foreground_poll * 2).min(FOREGROUND_POLL_MAX);
            if self.agent_process.is_none() {
                return;
            }
        }
        self.refresh_resume_hint(true);
    }

    fn refresh_resume_hint(&mut self, force: bool) {
        let foreground = self.foreground_group();
        if !force && self.resume_foreground == foreground {
            return;
        }
        if self.resume_foreground != foreground {
            // The group moved: observe promptly again until it settles.
            self.foreground_poll = FOREGROUND_POLL_MIN;
        }
        self.resume_foreground = foreground;
        // One descent answers both questions. Resolving them separately meant
        // two full /proc walks - stat, cmdline, cwd, environ and children per
        // node - for a single observation.
        let observation = if self.agent_exited {
            None
        } else {
            foreground
                .map(|group| {
                    crate::model::agent_recovery::foreground_observation(group, &self.launch)
                })
                // A group that moved mid-walk invalidates what the walk saw.
                .filter(|_| self.foreground_group() == foreground)
        };
        let (hint, process) = observation
            .map(|observation| (observation.hint, observation.agent))
            .unwrap_or_default();
        self.adopt_agent_process(process);
        self.agent_provider = self.agent_process.as_ref().map(|process| process.provider);
        if hint != self.resume_hint {
            self.resume_hint = hint.clone();
            self.emit(TerminalEvent::ResumeHintChanged(hint));
        }
    }

    fn set_agent_activity(&self, activity: Option<crate::agent::Activity>) {
        let mut current = self
            .published
            .agent_activity
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if *current == activity {
            return;
        }
        *current = activity;
        drop(current);
        self.emit(TerminalEvent::AgentActivityChanged(activity));
    }

    fn write_pty(&mut self, bytes: &[u8], operation: &'static str) {
        if bytes.is_empty() {
            return;
        }
        if let Err(error) = self
            .writer
            .write_all(bytes)
            .and_then(|()| self.writer.flush())
        {
            self.error(operation, error.to_string());
        }
    }

    fn emit(&self, event: TerminalEvent) {
        let _ = self.events.send(event);
    }

    fn error(&self, operation: &'static str, message: String) {
        self.set_agent_activity(None);
        self.emit(TerminalEvent::Error { operation, message });
    }
}

fn canonical_agent_title(title: &str, activity: Option<crate::agent::Activity>) -> String {
    if let Some(label) = agent_title_label(title, activity) {
        if label.is_empty() {
            "π".into()
        } else {
            format!("π {label}")
        }
    } else {
        title.to_owned()
    }
}

fn agent_title_label(title: &str, activity: Option<crate::agent::Activity>) -> Option<&str> {
    if !activity.is_some_and(|activity| activity.provider == crate::agent::Provider::Omp) {
        return None;
    }
    // The lifecycle glyph is view state, never a durable title or recovery ID.
    let rest = title.strip_prefix("π ")?;
    let glyph = rest.chars().next()?;
    Some(rest[glyph.len_utf8()..].strip_prefix(' ').unwrap_or(""))
}

fn theme_scheme(theme: TerminalTheme) -> ColorScheme {
    if theme == TerminalTheme::Light {
        ColorScheme::Light
    } else {
        ColorScheme::Dark
    }
}
fn coalesce_scroll_command(
    command: Command,
    commands: &Receiver<Command>,
    carried: &mut Option<Command>,
) -> Command {
    let Command::Scroll(ScrollCommand::Delta(mut delta)) = command else {
        return command;
    };
    while let Ok(next) = commands.try_recv() {
        match next {
            Command::Scroll(ScrollCommand::Delta(next_delta)) => {
                delta = delta.saturating_add(next_delta);
            }
            other => {
                *carried = Some(other);
                break;
            }
        }
    }
    Command::Scroll(ScrollCommand::Delta(delta))
}

/// Set parser defaults, not resolved UI RGB values: truecolor and application
/// OSC overrides retain their meaning, and OSC queries observe the same colors.
fn apply_theme(
    terminal: &mut Terminal<'_, '_>,
    theme: TerminalTheme,
) -> Result<(), libghostty_vt::Error> {
    let (foreground, background, ansi) = match theme {
        TerminalTheme::Dark => (
            0xd8d8d8,
            0x101010,
            [
                0x3b4252, 0xbf616a, 0xa3be8c, 0xebcb8b, 0x81a1c1, 0xb48ead, 0x88c0d0, 0xe5e9f0,
                0x4c566a, 0xd08770, 0xb1d196, 0xf0d399, 0x8fbcdb, 0xc5a0c0, 0x93ccdc, 0xeceff4,
            ],
        ),
        TerminalTheme::Light => (
            0x24292f,
            0xffffff,
            [
                0x24292f, 0xcf222e, 0x116329, 0x7d4e00, 0x0969da, 0x8250df, 0x1b7c83, 0x6e7781,
                0x57606a, 0xa40e26, 0x1a7f37, 0x9a6700, 0x0550ae, 0x6639ba, 0x0a6269, 0x8c959f,
            ],
        ),
        TerminalTheme::SolarizedDark => (
            0x839496,
            0x002b36,
            [
                0x073642, 0xdc322f, 0x859900, 0xb58900, 0x268bd2, 0xd33682, 0x2aa198, 0xeee8d5,
                0x002b36, 0xcb4b16, 0x586e75, 0x657b83, 0x839496, 0x6c71c4, 0x93a1a1, 0xfdf6e3,
            ],
        ),
    };
    let rgb = |value: u32| RgbColor {
        r: (value >> 16) as u8,
        g: (value >> 8) as u8,
        b: value as u8,
    };
    terminal
        .set_default_fg_color(Some(rgb(foreground)))?
        .set_default_bg_color(Some(rgb(background)))?
        .set_default_cursor_color(Some(rgb(foreground)))?;
    let mut palette = terminal.default_color_palette()?;
    for (index, value) in ansi.into_iter().enumerate() {
        palette.set(PaletteIndex(index as u8), rgb(value));
    }
    terminal.set_default_color_palette(Some(palette))?;
    Ok(())
}

fn wait_for_worker_event(
    commands: &Receiver<Command>,
    pty: &Receiver<PtyMessage>,
    child_exit: &Receiver<ChildExit>,
    capture: &Receiver<()>,
    pty_open: bool,
    child_running: bool,
    observe_after: Duration,
) -> WorkerEvent {
    match (pty_open, child_running) {
        (true, true) => crossbeam_channel::select! {
            recv(commands) -> message => WorkerEvent::Command(message.ok()),
            recv(capture) -> _ => WorkerEvent::Capture,
            recv(pty) -> message => WorkerEvent::Pty(message.ok()),
            recv(child_exit) -> message => WorkerEvent::ChildExit(message.ok()),
            default(observe_after) => WorkerEvent::ObserveForeground,
        },
        (true, false) => crossbeam_channel::select! {
            recv(commands) -> message => WorkerEvent::Command(message.ok()),
            recv(capture) -> _ => WorkerEvent::Capture,
            recv(pty) -> message => WorkerEvent::Pty(message.ok()),
        },
        (false, true) => crossbeam_channel::select! {
            recv(commands) -> message => WorkerEvent::Command(message.ok()),
            recv(capture) -> _ => WorkerEvent::Capture,
            recv(child_exit) -> message => WorkerEvent::ChildExit(message.ok()),
            default(observe_after) => WorkerEvent::ObserveForeground,
        },
        (false, false) => WorkerEvent::Command(commands.recv().ok()),
    }
}

fn child_exit_event(
    exited: &mut bool,
    result: ChildExit,
    kill_requested: bool,
) -> Option<TerminalEvent> {
    use std::os::unix::process::ExitStatusExt;

    if *exited {
        return None;
    }
    *exited = true;
    Some(match result {
        Ok(status) => TerminalEvent::Exited {
            exit_code: status.exit_code(),
            signal: status.signal().map(str::to_owned),
            // A successful close request must not hide a crash already waiting
            // in the reaper channel, nor turn wait/reaper failures into success.
            intentional: kill_requested
                && status.signal().is_some()
                && status.signal()
                    == ExitStatus::from(std::process::ExitStatus::from_raw(libc::SIGKILL)).signal(),
        },
        Err(message) => TerminalEvent::Error {
            operation: "wait for child",
            message,
        },
    })
}

/// Variables the daemon may have inherited from whatever terminal (or desktop
/// session) launched it. Left in place, a child would believe it is talking to
/// that emulator and emit control sequences libghostty-vt does not implement.
///
/// Some entries are worse than a wrong identity, because they name a live
/// control channel into something of the user's that a card is not inside:
/// `KITTY_LISTEN_ON` fully controls their real kitty instance, `TMUX` and
/// `STY` and `ZELLIJ` reach every pane of their multiplexer session, the
/// `KONSOLE_DBUS_*` triple drives their real Konsole over the session bus, and
/// `NVIM`, `VSCODE_IPC_HOOK_CLI` and `EMACS_SERVER_FILE` each run commands in
/// their editor. A child that inherited any of them could act outside its own
/// card - the exact containment this scrub exists to establish. `LC_TERMINAL`
/// and `LC_TERMINAL_VERSION` match sshd's default `AcceptEnv LC_*`, so they
/// carry the wrong identity onto every host the child then reaches.
///
/// Every emulator here contributes both halves where it has them, identity and
/// channel, because a channel is only reachable once and an identity is only
/// misleading once.
///
/// Color policy from a noninteractive launcher is not the policy of this PTY:
/// inherited NO_COLOR/CLICOLOR can disable ANSI despite TERM and COLORTERM, and
/// COLORFGBG describes the launcher's background rather than our OSC defaults.
/// CI also short-circuits common ANSI capability detectors before truecolor
/// checks; it describes the launcher, not this newly allocated interactive PTY.
/// Explicit recipe entries are restored after this scrub.
const INHERITED_TERMINAL_MARKERS: &[&str] = &[
    "TERM_PROGRAM",
    "TERM_PROGRAM_VERSION",
    "TERM_SESSION_ID",
    "TERMINAL_EMULATOR",
    "TERMINFO",
    "TERMINFO_DIRS",
    "NO_COLOR",
    "FORCE_COLOR",
    "CLICOLOR",
    "CLICOLOR_FORCE",
    "COLORFGBG",
    "CI",
    "KITTY_WINDOW_ID",
    "KITTY_PID",
    "KITTY_INSTALLATION_DIR",
    "KITTY_LISTEN_ON",
    "KITTY_PUBLIC_KEY",
    "KITTY_SHELL_INTEGRATION",
    "GHOSTTY_RESOURCES_DIR",
    "GHOSTTY_BIN_DIR",
    "GHOSTTY_SHELL_FEATURES",
    "GHOSTTY_SHELL_INTEGRATION_NO_SUDO",
    "VTE_VERSION",
    "ALACRITTY_WINDOW_ID",
    "ALACRITTY_SOCKET",
    "ALACRITTY_LOG",
    "WEZTERM_PANE",
    "WEZTERM_UNIX_SOCKET",
    "WEZTERM_EXECUTABLE",
    "WEZTERM_EXECUTABLE_DIR",
    "WEZTERM_CONFIG_DIR",
    "WEZTERM_CONFIG_FILE",
    "ITERM_SESSION_ID",
    "LC_TERMINAL",
    "LC_TERMINAL_VERSION",
    "KONSOLE_VERSION",
    "KONSOLE_DBUS_SERVICE",
    "KONSOLE_DBUS_SESSION",
    "KONSOLE_DBUS_WINDOW",
    "WT_SESSION",
    "WT_PROFILE_ID",
    "ZED_TERM",
    "VSCODE_INJECTION",
    "VSCODE_IPC_HOOK_CLI",
    "VSCODE_GIT_IPC_HANDLE",
    "VSCODE_GIT_ASKPASS_NODE",
    "VSCODE_GIT_ASKPASS_MAIN",
    "EMACS_SERVER_FILE",
    "TMUX",
    "TMUX_PANE",
    "STY",
    "ZELLIJ",
    "ZELLIJ_SESSION_NAME",
    "ZELLIJ_PANE_ID",
    "NVIM",
    "NVIM_LISTEN_ADDRESS",
    "COLORTERM",
];

/// Drops every inherited marker from a command's environment.
///
/// Separate from [`command_from_recipe`] so a test can plant all of them and
/// watch them go, rather than observing that a host which never set them does
/// not have them.
fn scrub_inherited_markers(command: &mut CommandBuilder) {
    command.env_remove("CAGENTS_TERMINAL_ID");
    for name in INHERITED_TERMINAL_MARKERS {
        command.env_remove(name);
    }
}

/// With a PTY, a recognized shell without a command or script operand is an
/// interactive launch. Unknown invocation forms stay active conservatively.
fn interactive_shell_launch(recipe: &LaunchRecipe) -> bool {
    let Some(name) = std::path::Path::new(&recipe.program)
        .file_name()
        .and_then(|name| name.to_str())
    else {
        return false;
    };
    if !matches!(
        name,
        "sh" | "bash"
            | "zsh"
            | "fish"
            | "dash"
            | "ash"
            | "ksh"
            | "ksh93"
            | "mksh"
            | "pdksh"
            | "yash"
            | "csh"
            | "tcsh"
    ) {
        return false;
    }
    let mut arguments = recipe.arguments.iter();
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--" => return arguments.next().is_none(),
            "--login" | "--interactive" | "--noprofile" | "--norc" | "--no-config" | "--posix" => {}
            "--rcfile" | "--init-file" | "-o" | "+o" | "-O" | "+O" => {
                if arguments.next().is_none() {
                    return false;
                }
            }
            option
                if option.starts_with('-')
                    && !option.starts_with("--")
                    && option.len() > 1
                    && option[1..]
                        .bytes()
                        .all(|byte| byte.is_ascii_alphabetic() && byte != b'c') => {}
            _ => return false,
        }
    }
    true
}

fn command_from_recipe(recipe: &LaunchRecipe) -> CommandBuilder {
    let mut command = CommandBuilder::new(&recipe.program);
    command.args(&recipe.arguments);
    command.cwd(&recipe.working_directory);
    scrub_inherited_markers(&mut command);
    terminal_environment(&mut command);
    // Last, so an explicit per-session entry still wins over our defaults.
    for (name, value) in &recipe.environment {
        command.env(name, value);
    }
    command
}

fn terminal_environment(command: &mut CommandBuilder) {
    // We ship no private terminfo entry; advertise the emulator we implement.
    command.env("TERM", "xterm-256color");
    command.env("COLORTERM", "truecolor");
    command.env("TERM_PROGRAM", "cagents");
    command.env("TERM_PROGRAM_VERSION", crate::VERSION);
}

fn pty_size(size: TerminalSize) -> PtySize {
    PtySize {
        rows: size.rows,
        cols: size.columns,
        pixel_width: u16::try_from(u32::from(size.columns).saturating_mul(size.cell_width_px))
            .unwrap_or(u16::MAX),
        pixel_height: u16::try_from(u32::from(size.rows).saturating_mul(size.cell_height_px))
            .unwrap_or(u16::MAX),
    }
}

/// Duplicate a master PTY descriptor for the reader's readiness wait.
///
/// Readiness is a property of the terminal, not of a particular descriptor, so
/// a duplicate answers for the clone the reader actually reads from - and the
/// reader owning it means shutdown never closes a descriptor out from under a
/// thread that is still polling it.
fn duplicate_descriptor(master: &dyn MasterPty) -> Result<OwnedFd, String> {
    let fd = master.as_raw_fd().ok_or("PTY descriptor unavailable")?;
    // SAFETY: `master` keeps `fd` open across this call, and F_DUPFD_CLOEXEC
    // returns a fresh descriptor nobody else owns.
    let duplicate = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 0) };
    if duplicate < 0 {
        return Err(format!(
            "duplicate PTY descriptor: {}",
            std::io::Error::last_os_error()
        ));
    }
    // SAFETY: `fcntl` just returned an owned descriptor.
    Ok(unsafe { OwnedFd::from_raw_fd(duplicate) })
}

/// Read and write ends of the reader's shutdown pipe. Nothing is ever written
/// to it: dropping the write end hangs the read end up, which is the one event
/// a blocking reader can always observe.
fn wake_pipe() -> Result<(OwnedFd, OwnedFd), String> {
    let mut ends = [0 as libc::c_int; 2];
    // SAFETY: `ends` is the two-element array `pipe2` writes into.
    if unsafe { libc::pipe2(ends.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(format!(
            "create PTY reader wake pipe: {}",
            std::io::Error::last_os_error()
        ));
    }
    // SAFETY: `pipe2` filled both entries with fresh owned descriptors.
    unsafe { Ok((OwnedFd::from_raw_fd(ends[0]), OwnedFd::from_raw_fd(ends[1]))) }
}

enum Readiness {
    Readable,
    Shutdown,
    Failed(String),
}

/// Wait until the terminal has output, or until shutdown hangs up the wake
/// pipe. A PTY master read only fails with EIO once the last slave descriptor
/// closes, so a disowned grandchild can keep a blocking read - and therefore
/// shutdown - waiting indefinitely; the pipe is the way out.
fn wait_readable(readiness: &OwnedFd, wake: &OwnedFd) -> Readiness {
    let mut fds = [
        libc::pollfd {
            fd: readiness.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        },
        libc::pollfd {
            fd: wake.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        },
    ];
    loop {
        // SAFETY: this thread owns both descriptors for the whole call.
        let ready = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, -1) };
        if ready < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Readiness::Failed(error.to_string());
        }
        if fds[1].revents != 0 {
            return Readiness::Shutdown;
        }
        if fds[0].revents != 0 {
            // Hangup still needs one more read: the parser must see everything
            // the child wrote before its slave closed, and an unreadable
            // descriptor surfaces as EOF or an error from the read itself.
            return Readiness::Readable;
        }
    }
}

fn read_pty(reader: &mut dyn Read, sender: &Sender<PtyMessage>, readiness: OwnedFd, wake: OwnedFd) {
    let mut buffer = vec![0u8; PTY_READ_SIZE];
    loop {
        match wait_readable(&readiness, &wake) {
            Readiness::Readable => {}
            Readiness::Shutdown => return,
            Readiness::Failed(error) => {
                let _ = sender.send(PtyMessage::Error(error));
                return;
            }
        }
        match reader.read(&mut buffer) {
            Ok(0) => {
                let _ = sender.send(PtyMessage::Eof);
                return;
            }
            Ok(read) => {
                if sender
                    .send(PtyMessage::Bytes(buffer[..read].to_vec()))
                    .is_err()
                {
                    return;
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => {
                let _ = sender.send(PtyMessage::Error(error.to_string()));
                return;
            }
        }
    }
}

struct TranscriptWriter {
    sender: Sender<TranscriptMessage>,
    /// Published by the writer once the size cap is reached, so the worker can
    /// stop handing over batches the writer would only drop.
    capped: Arc<AtomicBool>,
    worker: JoinHandle<()>,
}

impl TranscriptWriter {
    fn start(path: PathBuf, capacity: usize, max_bytes: u64) -> Result<Self, String> {
        let file = open_private_append_file(&path)
            .map_err(|error| format!("open transcript {}: {error}", path.display()))?;
        let written = file.metadata().map(|metadata| metadata.len()).unwrap_or(0);
        let (sender, receiver) = crossbeam_channel::bounded(capacity);
        let capped_flag = Arc::new(AtomicBool::new(false));
        let published_cap = Arc::clone(&capped_flag);
        let worker = thread::Builder::new()
            .name("terminal-transcript-writer".into())
            .spawn(move || {
                let mut writer = BufWriter::new(file);
                let mut written = written;
                // Already over cap (e.g. an older uncapped session): the file
                // stays as-is and nothing more is recorded.
                let mut capped = written >= max_bytes;
                if capped {
                    published_cap.store(true, Ordering::Relaxed);
                    let _ = writer.write_all(TRANSCRIPT_TRUNCATED_MARKER);
                    let _ = writer.flush();
                }
                while let Ok(message) = receiver.recv() {
                    match message {
                        TranscriptMessage::Bytes(bytes) => {
                            if capped {
                                continue;
                            }
                            let within_cap =
                                written.saturating_add(bytes.len() as u64) <= max_bytes;
                            if !within_cap {
                                capped = true;
                                published_cap.store(true, Ordering::Relaxed);
                                let _ = writer.write_all(TRANSCRIPT_TRUNCATED_MARKER);
                                let _ = writer.flush();
                                continue;
                            }
                            if writer.write_all(&bytes).is_err() {
                                break;
                            }
                            written += bytes.len() as u64;
                        }
                        TranscriptMessage::Shutdown => break,
                    }
                }
                let _ = writer.flush();
            })
            .map_err(|error| format!("start transcript writer: {error}"))?;
        Ok(Self {
            sender,
            capped: capped_flag,
            worker,
        })
    }

    /// Whether handing over a batch can still record anything. Racy by
    /// design and deliberately cheap: a full channel or a capped file means
    /// the batch is dropped, and dropping it here costs nothing at all.
    fn accepts(&self) -> bool {
        !self.sender.is_full() && !self.capped.load(Ordering::Relaxed)
    }

    /// Best-effort and never blocking: output bursts faster than the disk
    /// writer are dropped instead of stalling PTY processing. Only a dead
    /// writer thread reports an error.
    fn send(&self, bytes: Vec<u8>) -> Result<(), ()> {
        match self.sender.try_send(TranscriptMessage::Bytes(bytes)) {
            Ok(()) | Err(TrySendError::Full(_)) => Ok(()),
            Err(TrySendError::Disconnected(_)) => Err(()),
        }
    }

    fn finish(self) {
        let _ = self.sender.send(TranscriptMessage::Shutdown);
        drop(self.sender);
        let _ = self.worker.join();
    }
}

fn encode_key_input(
    terminal: &Terminal<'_, '_>,
    encoder: &mut KeyEncoder<'_>,
    input: KeyInput,
) -> Result<Vec<u8>, libghostty_vt::Error> {
    // Legacy encodings have no repeat event type, so a repeated event must be
    // encoded as another press. Keep Repeat only when the child asked for it.
    let report_events = terminal
        .kitty_keyboard_flags()?
        .contains(key::KittyKeyFlags::REPORT_EVENTS);
    let mut event = key::Event::new()?;
    event
        .set_action(map_key_action(input.action, report_events))
        .set_key(map_key(input.key))
        .set_mods(map_modifiers(input.modifiers))
        .set_utf8(input.text);
    if let Some(codepoint) = input.unshifted_codepoint {
        event.set_unshifted_codepoint(codepoint);
    }

    encoder.set_options_from_terminal(terminal);
    let mut encoded = Vec::with_capacity(32);
    encoder.encode_to_vec(&event, &mut encoded)?;
    Ok(encoded)
}

fn map_key_action(action: KeyAction, report_events: bool) -> key::Action {
    match action {
        KeyAction::Press => key::Action::Press,
        KeyAction::Release => key::Action::Release,
        KeyAction::Repeat if report_events => key::Action::Repeat,
        KeyAction::Repeat => key::Action::Press,
    }
}

fn map_key(input: TerminalKey) -> key::Key {
    match input {
        TerminalKey::Unidentified => key::Key::Unidentified,
        TerminalKey::Character(character) => match character.to_ascii_lowercase() {
            'a' => key::Key::A,
            'b' => key::Key::B,
            'c' => key::Key::C,
            'd' => key::Key::D,
            'e' => key::Key::E,
            'f' => key::Key::F,
            'g' => key::Key::G,
            'h' => key::Key::H,
            'i' => key::Key::I,
            'j' => key::Key::J,
            'k' => key::Key::K,
            'l' => key::Key::L,
            'm' => key::Key::M,
            'n' => key::Key::N,
            'o' => key::Key::O,
            'p' => key::Key::P,
            'q' => key::Key::Q,
            'r' => key::Key::R,
            's' => key::Key::S,
            't' => key::Key::T,
            'u' => key::Key::U,
            'v' => key::Key::V,
            'w' => key::Key::W,
            'x' => key::Key::X,
            'y' => key::Key::Y,
            'z' => key::Key::Z,
            '0' => key::Key::Digit0,
            '1' => key::Key::Digit1,
            '2' => key::Key::Digit2,
            '3' => key::Key::Digit3,
            '4' => key::Key::Digit4,
            '5' => key::Key::Digit5,
            '6' => key::Key::Digit6,
            '7' => key::Key::Digit7,
            '8' => key::Key::Digit8,
            '9' => key::Key::Digit9,
            ' ' => key::Key::Space,
            '-' => key::Key::Minus,
            '=' => key::Key::Equal,
            ',' => key::Key::Comma,
            '.' => key::Key::Period,
            '/' => key::Key::Slash,
            ';' => key::Key::Semicolon,
            '\'' => key::Key::Quote,
            '`' => key::Key::Backquote,
            '[' => key::Key::BracketLeft,
            ']' => key::Key::BracketRight,
            '\\' => key::Key::Backslash,
            _ => key::Key::Unidentified,
        },
        TerminalKey::Backspace => key::Key::Backspace,
        TerminalKey::Enter => key::Key::Enter,
        TerminalKey::Tab => key::Key::Tab,
        TerminalKey::Escape => key::Key::Escape,
        TerminalKey::Delete => key::Key::Delete,
        TerminalKey::Insert => key::Key::Insert,
        TerminalKey::Home => key::Key::Home,
        TerminalKey::End => key::Key::End,
        TerminalKey::PageUp => key::Key::PageUp,
        TerminalKey::PageDown => key::Key::PageDown,
        TerminalKey::ArrowUp => key::Key::ArrowUp,
        TerminalKey::ArrowDown => key::Key::ArrowDown,
        TerminalKey::ArrowLeft => key::Key::ArrowLeft,
        TerminalKey::ArrowRight => key::Key::ArrowRight,
        TerminalKey::Function(number) => match number {
            1 => key::Key::F1,
            2 => key::Key::F2,
            3 => key::Key::F3,
            4 => key::Key::F4,
            5 => key::Key::F5,
            6 => key::Key::F6,
            7 => key::Key::F7,
            8 => key::Key::F8,
            9 => key::Key::F9,
            10 => key::Key::F10,
            11 => key::Key::F11,
            12 => key::Key::F12,
            13 => key::Key::F13,
            14 => key::Key::F14,
            15 => key::Key::F15,
            16 => key::Key::F16,
            17 => key::Key::F17,
            18 => key::Key::F18,
            19 => key::Key::F19,
            20 => key::Key::F20,
            21 => key::Key::F21,
            22 => key::Key::F22,
            23 => key::Key::F23,
            24 => key::Key::F24,
            25 => key::Key::F25,
            _ => key::Key::Unidentified,
        },
    }
}

fn map_modifiers(input: KeyModifiers) -> key::Mods {
    let mut output = key::Mods::empty();
    output.set(key::Mods::SHIFT, input.shift);
    output.set(key::Mods::ALT, input.alt);
    output.set(key::Mods::CTRL, input.control);
    output.set(key::Mods::SUPER, input.super_key);
    output.set(key::Mods::CAPS_LOCK, input.caps_lock);
    output
}

fn map_mouse_action(action: MouseAction) -> mouse::Action {
    match action {
        MouseAction::Press => mouse::Action::Press,
        MouseAction::Release => mouse::Action::Release,
        MouseAction::Motion => mouse::Action::Motion,
    }
}

fn map_mouse_button(button: MouseButton) -> mouse::Button {
    match button {
        MouseButton::Left => mouse::Button::Left,
        MouseButton::Right => mouse::Button::Right,
        MouseButton::Middle => mouse::Button::Middle,
        MouseButton::Back => mouse::Button::Four,
        MouseButton::Forward => mouse::Button::Five,
        MouseButton::WheelUp => mouse::Button::Four,
        MouseButton::WheelDown => mouse::Button::Five,
        MouseButton::Other(number) => match number {
            4 => mouse::Button::Four,
            5 => mouse::Button::Five,
            6 => mouse::Button::Six,
            7 => mouse::Button::Seven,
            8 => mouse::Button::Eight,
            9 => mouse::Button::Nine,
            10 => mouse::Button::Ten,
            11 => mouse::Button::Eleven,
            _ => mouse::Button::Unknown,
        },
    }
}

#[cfg(test)]
mod tests {

    use std::{
        collections::BTreeMap,
        ffi::OsStr,
        fs,
        os::unix::fs::{PermissionsExt, symlink},
        sync::atomic::{AtomicUsize, Ordering},
        time::Duration,
    };

    use tempfile::tempdir;

    use super::*;

    #[test]
    fn omp_display_title_drops_lifecycle_animation_but_preserves_labels() {
        use crate::agent::{Activity, Provider};
        let title = |value: &str| {
            canonical_agent_title(
                value,
                Provider::Omp
                    .activity_from_title(value)
                    .map(|state| Activity {
                        provider: Provider::Omp,
                        state,
                    }),
            )
        };
        assert_eq!(title("π ⠋ project"), title("π ⠙ project"));
        assert_eq!(title("π > project"), "π project");
        assert_eq!(title("π ! renamed"), "π renamed");
        assert_eq!(title("custom title"), "custom title");
        assert_eq!(canonical_agent_title("π ⠋ project", None), "π ⠋ project");
    }

    #[test]
    fn changing_parser_theme_preserves_truecolor_and_osc_overrides_and_queries() {
        let mut terminal = Terminal::new(TerminalOptions {
            cols: 8,
            rows: 2,
            max_scrollback: 0,
        })
        .unwrap();
        let replies = Rc::new(RefCell::new(Vec::new()));
        terminal
            .on_pty_write({
                let replies = Rc::clone(&replies);
                move |_, bytes| replies.borrow_mut().extend_from_slice(bytes)
            })
            .unwrap();
        apply_theme(&mut terminal, TerminalTheme::Dark).unwrap();
        terminal.vt_write(b"\x1b[38;2;18;52;86mT\x1b[0mD\x1b[31mR\x1b[0m");
        let mut visual = VisualResources::new().unwrap();
        let dark = visual.capture(&terminal, 1, true, None).unwrap().unwrap();
        terminal.vt_write(b"\x1b]10;?\x07\x1b]11;?\x07\x1b]4;1;?\x07");
        let dark_queries = std::mem::take(&mut *replies.borrow_mut());
        apply_theme(&mut terminal, TerminalTheme::Light).unwrap();
        let light = visual.capture(&terminal, 2, true, None).unwrap().unwrap();
        assert_eq!(
            dark.lines[0].cells[0].foreground,
            light.lines[0].cells[0].foreground
        );
        assert_ne!(
            dark.lines[0].cells[1].foreground,
            light.lines[0].cells[1].foreground
        );
        assert_ne!(
            dark.lines[0].cells[2].foreground,
            light.lines[0].cells[2].foreground
        );
        terminal.vt_write(b"\x1b]10;?\x07\x1b]11;?\x07\x1b]4;1;?\x07");
        let light_queries = std::mem::take(&mut *replies.borrow_mut());
        assert!(!light_queries.is_empty());
        assert_ne!(dark_queries, light_queries);
        terminal.vt_write(b"\x1b]10;#123456\x07\x1b]11;#345678\x07\x1b]4;1;#56789a\x07");
        terminal.vt_write(b"\x1b]10;?\x07\x1b]11;?\x07\x1b]4;1;?\x07");
        let overridden = std::mem::take(&mut *replies.borrow_mut());
        apply_theme(&mut terminal, TerminalTheme::SolarizedDark).unwrap();
        terminal.vt_write(b"\x1b]10;?\x07\x1b]11;?\x07\x1b]4;1;?\x07");
        assert_eq!(*replies.borrow(), overridden);
        terminal.vt_write(b"\x1b]110\x07\x1b]111\x07\x1b]104;1\x07");
        assert_eq!(
            terminal.fg_color().unwrap(),
            terminal.default_fg_color().unwrap()
        );
        assert_eq!(
            terminal.bg_color().unwrap(),
            terminal.default_bg_color().unwrap()
        );
        assert_eq!(
            terminal.color_palette().unwrap().get(PaletteIndex::RED),
            terminal
                .default_color_palette()
                .unwrap()
                .get(PaletteIndex::RED)
        );
    }

    fn capture_worker() -> (Worker, Receiver<TerminalEvent>) {
        let recipe = LaunchRecipe {
            program: "/bin/true".into(),
            arguments: Vec::new(),
            working_directory: PathBuf::from("/"),
            environment: BTreeMap::new(),
        };
        capture_worker_for(recipe)
    }

    fn capture_worker_for(recipe: LaunchRecipe) -> (Worker, Receiver<TerminalEvent>) {
        capture_worker_with(recipe, TerminalConfig::default())
    }

    fn capture_worker_with(
        recipe: LaunchRecipe,
        config: TerminalConfig,
    ) -> (Worker, Receiver<TerminalEvent>) {
        let session = TerminalSession {
            id: crate::model::TerminalId::new(),
            launch: recipe.clone(),
            transcript_path: None,
            resume_hint: None,
            continuity: crate::model::TerminalContinuity::RelaunchAvailable {
                recipe,
                last_exit: None,
            },
        };
        let (_tx, commands) = command_channel(1);
        let (events, receiver) = crossbeam_channel::unbounded();
        let published = Arc::new(PublishedState::new(true, Arc::new(|_| {})));
        (
            Worker::start(session, config, commands, events, published).unwrap(),
            receiver,
        )
    }

    #[test]
    fn fresh_omp_activity_does_not_require_a_resumable_session() {
        use crate::agent::{Activity, ActivityState, Provider};
        use std::time::Instant;
        let directory = tempdir().unwrap();
        let executable = directory.path().join("omp");
        symlink("/bin/sleep", &executable).unwrap();
        let (mut worker, events) = capture_worker_for(LaunchRecipe {
            program: executable.to_str().unwrap().into(),
            arguments: vec!["10".into()],
            working_directory: directory.path().into(),
            environment: BTreeMap::from([
                ("HOME".into(), directory.path().to_str().unwrap().into()),
                (
                    "PI_CODING_AGENT_DIR".into(),
                    directory.path().to_str().unwrap().into(),
                ),
            ]),
        });
        let deadline = Instant::now() + Duration::from_secs(3);
        let process = loop {
            if let Some(process) = worker.observe_agent_process().cloned() {
                break process;
            }
            assert!(Instant::now() < deadline, "PTY foreground never became OMP");
            std::thread::sleep(Duration::from_millis(1));
        };
        let tty = std::fs::read_link(format!("/proc/{}/fd/0", process.pid)).unwrap();
        let markers = directory.path().join("terminal-sessions");
        std::fs::create_dir(&markers).unwrap();
        std::fs::write(
            markers.join(format!(
                "pts-{}",
                tty.file_name().unwrap().to_str().unwrap()
            )),
            format!(
                "{}\n{}\n",
                directory.path().display(),
                directory
                    .path()
                    .join("sessions/fresh_12345678-1234-1234-1234-123456789abc.jsonl")
                    .display()
            ),
        )
        .unwrap();
        worker.handle_command(Command::Hide);
        let generation = worker.generation;
        for title in ["π ⠋ fresh", "π ⠙ fresh", "π ! fresh", "π > fresh"] {
            worker.process_output(format!("\x1b]2;{title}\x07").into_bytes());
            assert!(
                worker
                    .resume_hint
                    .as_deref()
                    .and_then(crate::model::agent_recovery::recipe_from_hint)
                    .is_none(),
                "recognizing a fresh agent must not grant resume authority"
            );
        }
        let activity = events
            .try_iter()
            .filter_map(|event| match event {
                TerminalEvent::AgentActivityChanged(activity) => Some(activity),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            activity,
            [
                ActivityState::Working,
                ActivityState::Waiting,
                ActivityState::Idle
            ]
            .map(|state| Some(Activity {
                provider: Provider::Omp,
                state
            }))
        );
        assert_eq!(
            worker.generation, generation,
            "hidden activity must not render"
        );
        worker.run();
    }

    #[test]
    fn activity_application_rejects_replaced_processes_and_expired_results() {
        use crate::agent::{Activity, ActivityState, Provider};
        use std::time::Instant;
        let directory = tempdir().unwrap();
        let executable = directory.path().join("claude");
        // A harmless native executable with recognizable argv; no provider I/O.
        symlink("/bin/sleep", &executable).unwrap();
        let (mut worker, _) = capture_worker_for(LaunchRecipe {
            program: executable.to_str().unwrap().into(),
            arguments: vec!["10".into()],
            working_directory: directory.path().into(),
            environment: BTreeMap::new(),
        });
        let startup_deadline = Instant::now() + Duration::from_secs(3);
        let process = loop {
            if let Some(process) = worker.observe_agent_process().cloned() {
                break process;
            }
            assert!(
                Instant::now() < startup_deadline,
                "PTY foreground never became Claude"
            );
            std::thread::sleep(Duration::from_millis(1));
        };
        let working = Some(Activity {
            provider: Provider::Claude,
            state: ActivityState::Working,
        });
        let apply = |worker: &mut Worker, process: AgentProcess, deadline| {
            let (reply, result) = crossbeam_channel::bounded(1);
            worker.handle_command(Command::ApplyAgentActivity {
                process,
                activity: working,
                reply,
                deadline,
            });
            result.recv().unwrap()
        };
        let deadline = || std::time::Instant::now() + Duration::from_secs(3);
        assert_eq!(apply(&mut worker, process.clone(), deadline()), working);
        let mut replaced = process.clone();
        replaced.start = replaced.start.wrapping_add(1);
        assert_eq!(apply(&mut worker, replaced, deadline()), None);
        assert_eq!(
            *worker
                .published
                .agent_activity
                .lock()
                .unwrap_or_else(|error| error.into_inner()),
            None
        );
        assert_eq!(apply(&mut worker, process.clone(), deadline()), working);
        let mut other_process = process.clone();
        other_process.pid += 1;
        assert_eq!(apply(&mut worker, other_process, deadline()), None);
        assert_eq!(apply(&mut worker, process.clone(), deadline()), working);
        assert_eq!(apply(&mut worker, process, std::time::Instant::now()), None);
        // The disconnected command channel drives normal kill/reap teardown.
        worker.run();
    }

    #[test]
    fn capture_credit_coalesces_batches_and_preserves_semantic_events() {
        let (mut worker, events) = capture_worker();
        worker.publish_snapshot(true);
        let first = worker.generation;
        worker.process_output(b"old".to_vec());
        worker.process_output(b"\r\x1b[2Klatest\x07\x1b]2;title\x07".to_vec());
        assert_eq!(worker.generation, first);
        assert_eq!(
            worker
                .published
                .activity
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .unwrap()
                .sequence,
            2
        );
        assert_eq!(
            events.try_iter().collect::<Vec<_>>(),
            vec![
                TerminalEvent::Bell,
                TerminalEvent::TitleChanged("title".into())
            ]
        );
        worker.published.acknowledge(first);
        worker.capture_if_ready();
        let snapshot = worker.published.snapshot.load_full();
        assert_eq!(
            snapshot.lines[0]
                .cells
                .iter()
                .take(6)
                .map(|cell| cell.text.as_str())
                .collect::<String>(),
            "latest"
        );
        assert_eq!(snapshot.activity.unwrap().sequence, 2);
        // Returning old credit cannot buy an extra snapshot.
        worker.process_output(b"!".to_vec());
        worker.published.acknowledge(first);
        worker.capture_if_ready();
        assert_eq!(worker.generation, first + 1);
        worker.published.acknowledge(first + 1);
        worker.capture_if_ready();
        assert_eq!(worker.published.snapshot.load().lines[0].cells[6].text, "!");
        // An ACK arriving before the next output also cannot strand that output.
        worker.published.acknowledge(worker.generation);
        worker.process_output(b"?".to_vec());
        assert_eq!(worker.published.snapshot.load().lines[0].cells[7].text, "?");
        worker.published.acknowledge(worker.generation);
        let previous = worker.published.snapshot.load_full();
        worker.process_output(b"\x1b[31m".to_vec());
        let activity_only = worker.published.snapshot.load_full();
        assert_eq!(activity_only.lines, previous.lines);
        assert!(activity_only.activity.unwrap().sequence > previous.activity.unwrap().sequence);
        worker.run();
    }

    #[test]
    fn eof_preserves_final_grid_without_outstanding_consumer_credit() {
        let (mut worker, _) = capture_worker();
        worker.publish_snapshot(true);
        worker.process_output(b"final".to_vec());
        let published = Arc::clone(&worker.published);
        // Keep the command channel connected so child EOF, not client teardown,
        // ends the worker loop.
        let (_commands, receiver) = command_channel(1);
        worker.commands = receiver;
        worker.run();
        assert_eq!(
            published.snapshot.load().lines[0]
                .cells
                .iter()
                .take(5)
                .map(|cell| cell.text.as_str())
                .collect::<String>(),
            "final"
        );
    }

    #[test]
    fn hidden_eof_preserves_final_contents_without_visual_wakes() {
        let (mut worker, _) = capture_worker();
        worker.set_hidden();
        worker.process_output(b"hidden".to_vec());
        assert_eq!(worker.generation, 0);
        let published = Arc::clone(&worker.published);
        let (_commands, receiver) = command_channel(1);
        worker.commands = receiver;
        worker.run();
        assert_eq!(published.announced_generation.load(Ordering::Acquire), 0);
        assert_eq!(
            published.snapshot.load().lines[0]
                .cells
                .iter()
                .take(6)
                .map(|cell| cell.text.as_str())
                .collect::<String>(),
            "hidden"
        );
    }

    #[test]
    fn foreground_observation_backs_off_and_skips_unwatched_terminals() {
        // A long-lived child keeps the foreground group stable for the whole
        // test, so every tick below takes its "nothing changed" path.
        let (mut worker, _) = capture_worker_for(LaunchRecipe {
            program: "/bin/sleep".into(),
            arguments: vec!["10".into()],
            working_directory: PathBuf::from("/"),
            environment: BTreeMap::new(),
        });
        worker.resume_foreground = worker.foreground_group();
        assert!(worker.agent_process.is_none());

        // With the same group and no agent to track, the walk is skipped and
        // the interval stretches instead of descending /proc every 2s forever.
        worker.observe_foreground();
        assert_eq!(worker.foreground_poll, FOREGROUND_POLL_MIN * 2);
        worker.observe_foreground();
        assert_eq!(worker.foreground_poll, FOREGROUND_POLL_MIN * 4);
        assert_eq!(worker.resume_foreground, worker.foreground_group());

        // A terminal nobody is watching is never observed on the timer.
        worker.set_hidden();
        worker.observe_foreground();
        assert_eq!(worker.foreground_poll, FOREGROUND_POLL_MAX);

        // Revealing it makes the next observation prompt again.
        worker.reveal();
        assert_eq!(worker.foreground_poll, FOREGROUND_POLL_MIN);
    }

    #[test]
    fn shutdown_preserves_already_parsed_dirty_contents() {
        let (mut worker, _) = capture_worker();
        worker.publish_snapshot(true);
        worker.process_output(b"before shutdown".to_vec());
        let published = Arc::clone(&worker.published);
        let (commands, receiver) = command_channel(1);
        commands.send(Command::Shutdown).unwrap();
        worker.commands = receiver;
        worker.run();
        assert_eq!(
            published.snapshot.load().lines[0]
                .cells
                .iter()
                .take(15)
                .map(|cell| cell.text.as_str())
                .collect::<String>(),
            "before shutdown"
        );
    }

    #[test]
    fn leader_exit_does_not_end_output_capture_or_redraw_credit() {
        let (mut worker, events) = capture_worker();
        worker.publish_snapshot(true);
        let mut exited = false;
        worker.handle_child_exit(&mut exited, Ok(ExitStatus::with_exit_code(0)));
        assert!(matches!(
            events.try_recv(),
            Ok(TerminalEvent::Exited { .. })
        ));
        worker.process_output(b"descendant".to_vec());
        worker.published.acknowledge(worker.generation);
        worker.capture_if_ready();
        assert_eq!(
            worker.published.snapshot.load().lines[0]
                .cells
                .iter()
                .take(10)
                .map(|cell| cell.text.as_str())
                .collect::<String>(),
            "descendant"
        );
        worker.run();
    }

    fn test_published(visible: bool, wakes: Arc<AtomicUsize>) -> PublishedState {
        PublishedState::new(
            visible,
            Arc::new(move |_| {
                wakes.fetch_add(1, Ordering::SeqCst);
            }),
        )
    }

    fn snapshot(generation: u64) -> Arc<RenderSnapshot> {
        Arc::new(RenderSnapshot {
            generation,
            ..RenderSnapshot::empty()
        })
    }

    fn key_input(
        action: KeyAction,
        key: TerminalKey,
        text: Option<&str>,
        unshifted_codepoint: Option<char>,
    ) -> KeyInput {
        KeyInput {
            action,
            key,
            modifiers: KeyModifiers::default(),
            text: text.map(str::to_owned),
            unshifted_codepoint,
        }
    }

    #[test]
    fn legacy_repeats_encode_once_as_ordinary_key_presses() {
        let terminal = Terminal::new(TerminalOptions {
            cols: 80,
            rows: 24,
            max_scrollback: 1_000,
        })
        .unwrap();
        let mut encoder = KeyEncoder::new().unwrap();
        let cases = [
            (TerminalKey::ArrowDown, None, None, &b"\x1b[B"[..]),
            (TerminalKey::Character('x'), Some("x"), Some('x'), &b"x"[..]),
            (TerminalKey::Backspace, None, None, &b"\x7f"[..]),
        ];

        for (key, text, unshifted_codepoint, expected) in cases {
            let press = encode_key_input(
                &terminal,
                &mut encoder,
                key_input(KeyAction::Press, key, text, unshifted_codepoint),
            )
            .unwrap();
            let repeat = encode_key_input(
                &terminal,
                &mut encoder,
                key_input(KeyAction::Repeat, key, text, unshifted_codepoint),
            )
            .unwrap();

            assert_eq!(press, expected, "unexpected press encoding for {key:?}");
            assert_eq!(repeat, expected, "unexpected repeat encoding for {key:?}");
        }
    }

    #[test]
    fn event_reporting_protocol_preserves_repeat_semantics() {
        let mut terminal = Terminal::new(TerminalOptions {
            cols: 80,
            rows: 24,
            max_scrollback: 1_000,
        })
        .unwrap();
        terminal.vt_write(b"\x1b[>2u");
        assert!(
            terminal
                .kitty_keyboard_flags()
                .unwrap()
                .contains(key::KittyKeyFlags::REPORT_EVENTS)
        );
        let mut encoder = KeyEncoder::new().unwrap();

        let press = encode_key_input(
            &terminal,
            &mut encoder,
            key_input(KeyAction::Press, TerminalKey::ArrowDown, None, None),
        )
        .unwrap();
        let repeat = encode_key_input(
            &terminal,
            &mut encoder,
            key_input(KeyAction::Repeat, TerminalKey::ArrowDown, None, None),
        )
        .unwrap();

        assert_eq!(press, b"\x1b[1;1:1B");
        assert_eq!(repeat, b"\x1b[1;1:2B");
    }

    /// A generation the subscriber has not acknowledged must not silence the
    /// next one. `capture_if_ready` is what holds this protocol to one wake
    /// per drawn frame, so the announcement itself must not also wait for a
    /// draw: an edge the callback dropped - a repaint broadcast while no
    /// subscriber was visible - would otherwise freeze the terminal until
    /// something unrelated repainted it.
    #[test]
    fn each_published_generation_is_announced_once_without_an_acknowledgement() {
        let wakes = Arc::new(AtomicUsize::new(0));
        let published = test_published(true, Arc::clone(&wakes));

        published.publish(snapshot(1));
        published.publish(snapshot(2));
        assert_eq!(wakes.load(Ordering::SeqCst), 2);

        // Acknowledging returns capture credit and announces nothing.
        published.acknowledge(2);
        assert_eq!(wakes.load(Ordering::SeqCst), 2);
        assert_eq!(published.capture_rx.try_recv(), Ok(()));
    }

    #[test]
    fn hidden_publication_suppresses_visual_wakes() {
        let wakes = Arc::new(AtomicUsize::new(0));
        let published = test_published(false, Arc::clone(&wakes));

        published.publish(snapshot(1));

        assert_eq!(published.snapshot.load().generation, 1);
        assert_eq!(wakes.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn visibility_reveal_emits_latest_snapshot_once() {
        let wakes = Arc::new(AtomicUsize::new(0));
        let published = test_published(false, Arc::clone(&wakes));
        published.publish(snapshot(4));

        published.visible.store(true, Ordering::Release);
        published.request_wake(4);
        published.request_wake(4);

        assert_eq!(wakes.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn transcript_writer_privatizes_storage_and_preserves_append_behavior() {
        let root = tempdir().unwrap();
        let directory = root.path().join("transcripts");
        fs::create_dir(&directory).unwrap();
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o755)).unwrap();
        let path = directory.join("terminal.log");
        fs::write(&path, b"before\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();

        let writer = TranscriptWriter::start(path.clone(), 1, 1 << 20).unwrap();
        writer.send(b"after\n".to_vec()).unwrap();
        writer.finish();

        assert_eq!(fs::read(&path).unwrap(), b"before\nafter\n");
        assert_eq!(
            fs::metadata(&directory).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn transcript_writer_creates_private_storage() {
        let root = tempdir().unwrap();
        let directory = root.path().join("new-transcripts");
        let path = directory.join("terminal.log");

        TranscriptWriter::start(path.clone(), 1, 1 << 20)
            .unwrap()
            .finish();

        assert_eq!(
            fs::metadata(&directory).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn transcript_writer_stops_at_size_cap_with_marker() {
        let root = tempdir().unwrap();
        let path = root.path().join("terminal.log");

        let writer = TranscriptWriter::start(path.clone(), 64, 16).unwrap();
        writer.send(vec![b'x'; 64]).unwrap();
        writer.send(b"discarded\n".to_vec()).unwrap();
        writer.finish();

        let content = fs::read(&path).unwrap();
        assert!(
            content.ends_with(TRANSCRIPT_TRUNCATED_MARKER),
            "capped transcript should end with the truncation marker"
        );
        assert!(
            content.len() <= 16 + TRANSCRIPT_TRUNCATED_MARKER.len(),
            "capped transcript should not grow past the cap plus marker"
        );
    }

    #[test]
    fn transcript_writer_caps_preexisting_oversized_file() {
        let root = tempdir().unwrap();
        let path = root.path().join("terminal.log");
        fs::write(&path, vec![b'y'; 32]).unwrap();

        let writer = TranscriptWriter::start(path.clone(), 64, 16).unwrap();
        writer.send(b"new output\n".to_vec()).unwrap();
        writer.finish();

        let content = fs::read(&path).unwrap();
        assert_eq!(
            content,
            [vec![b'y'; 32], TRANSCRIPT_TRUNCATED_MARKER.to_vec()].concat(),
            "oversized transcript should keep existing bytes and record nothing more"
        );
    }

    #[test]
    fn transcript_writer_rejects_file_and_directory_symlinks() {
        let root = tempdir().unwrap();
        let directory = root.path().join("transcripts");
        fs::create_dir(&directory).unwrap();
        let victim = directory.join("victim");
        fs::write(&victim, b"unchanged").unwrap();
        let linked_file = directory.join("linked.log");
        symlink(&victim, &linked_file).unwrap();

        assert!(TranscriptWriter::start(linked_file, 1, 1 << 20).is_err());
        assert_eq!(fs::read(&victim).unwrap(), b"unchanged");

        let real_directory = root.path().join("real");
        fs::create_dir(&real_directory).unwrap();
        let linked_directory = root.path().join("linked-directory");
        symlink(&real_directory, &linked_directory).unwrap();
        assert!(
            TranscriptWriter::start(linked_directory.join("terminal.log"), 1, 1 << 20).is_err()
        );
    }

    #[test]
    fn command_and_pty_channels_are_bounded() {
        let (commands, _receiver) = command_channel(1);
        commands.try_send(Command::Hide).unwrap();
        assert!(matches!(
            commands.try_send(Command::Reveal),
            Err(TrySendError::Full(_))
        ));

        let (pty, _receiver) = crossbeam_channel::bounded(1);
        pty.try_send(PtyMessage::Bytes(vec![1])).unwrap();
        assert!(matches!(
            pty.try_send(PtyMessage::Bytes(vec![2])),
            Err(TrySendError::Full(_))
        ));
    }
    #[test]
    fn hidden_idle_wait_is_driven_by_commands_and_child_exit_channels() {
        let (command_tx, command_rx) = command_channel(1);
        let (_pty_tx, pty_rx) = crossbeam_channel::bounded(1);
        let (child_exit_tx, child_exit_rx) = crossbeam_channel::bounded(1);

        command_tx.send(Command::Hide).unwrap();
        assert!(matches!(
            wait_for_worker_event(
                &command_rx,
                &pty_rx,
                &child_exit_rx,
                &crossbeam_channel::never(),
                false,
                true,
                FOREGROUND_POLL_MIN,
            ),
            WorkerEvent::Command(Some(Command::Hide))
        ));

        child_exit_tx
            .send(Ok(ExitStatus::with_exit_code(7)))
            .unwrap();
        assert!(matches!(
            wait_for_worker_event(
                &command_rx,
                &pty_rx,
                &child_exit_rx,
                &crossbeam_channel::never(),
                false,
                true,
                FOREGROUND_POLL_MIN,
            ),
            WorkerEvent::ChildExit(Some(Ok(status))) if status.exit_code() == 7
        ));
    }

    #[test]
    fn child_exit_is_published_exactly_once() {
        let deliveries = AtomicUsize::new(0);
        let (event_tx, event_rx) = crossbeam_channel::unbounded();
        let mut exited = false;

        for status in [7, 8] {
            if let Some(event) =
                child_exit_event(&mut exited, Ok(ExitStatus::with_exit_code(status)), false)
            {
                deliveries.fetch_add(1, Ordering::SeqCst);
                event_tx.send(event).unwrap();
            }
        }

        assert_eq!(deliveries.load(Ordering::SeqCst), 1);
        assert!(matches!(
            event_rx.try_recv(),
            Ok(TerminalEvent::Exited {
                exit_code: 7,
                signal: None,
                intentional: false,
            })
        ));
        assert!(event_rx.try_recv().is_err());
    }

    #[test]
    fn close_receipt_suppresses_only_its_requested_kill_not_crashes_or_wait_errors() {
        use std::os::unix::process::ExitStatusExt;

        let killed = || ExitStatus::from(std::process::ExitStatus::from_raw(libc::SIGKILL));
        assert!(matches!(
            child_exit_event(&mut false, Ok(killed()), true),
            Some(TerminalEvent::Exited {
                intentional: true,
                signal: Some(_),
                ..
            })
        ));
        assert!(matches!(
            child_exit_event(&mut false, Ok(killed()), false),
            Some(TerminalEvent::Exited {
                intentional: false,
                ..
            })
        ));
        let crashed = ExitStatus::from(std::process::ExitStatus::from_raw(libc::SIGSEGV));
        assert!(matches!(
            child_exit_event(&mut false, Ok(crashed), true),
            Some(TerminalEvent::Exited {
                intentional: false,
                ..
            })
        ));
        assert!(matches!(
            child_exit_event(&mut false, Ok(ExitStatus::with_exit_code(7)), true),
            Some(TerminalEvent::Exited {
                exit_code: 7,
                intentional: false,
                ..
            })
        ));
        assert!(matches!(
            child_exit_event(&mut false, Err("reaper failed".into()), true),
            Some(TerminalEvent::Error {
                operation: "wait for child",
                ..
            })
        ));
    }

    fn env_recipe(environment: BTreeMap<String, String>) -> LaunchRecipe {
        LaunchRecipe {
            program: "/bin/sh".to_owned(),
            arguments: Vec::new(),
            working_directory: PathBuf::from("/"),
            environment,
        }
    }

    #[test]
    fn child_environment_advertises_our_own_terminal_identity() {
        let command = command_from_recipe(&env_recipe(BTreeMap::new()));

        assert_eq!(command.get_env("TERM"), Some(OsStr::new("xterm-256color")));
        assert_eq!(command.get_env("COLORTERM"), Some(OsStr::new("truecolor")));
        assert_eq!(command.get_env("TERM_PROGRAM"), Some(OsStr::new("cagents")));
        assert_eq!(
            command.get_env("TERM_PROGRAM_VERSION"),
            Some(OsStr::new(crate::VERSION))
        );
        // The test below proves the scrub removes names; this one proves
        // `command_from_recipe` still calls it. Only markers this host
        // actually set can witness that, so the assertion is conditional
        // rather than absent: on a host inside kitty or tmux it is real
        // coverage of the call site, and on a bare host it costs nothing.
        for name in INHERITED_TERMINAL_MARKERS {
            if matches!(*name, "TERM_PROGRAM" | "TERM_PROGRAM_VERSION" | "COLORTERM") {
                // Set again, deliberately, after the scrub.
                continue;
            }
            if std::env::var_os(name).is_some() {
                assert_eq!(command.get_env(name), None, "{name} reached the child");
            }
        }
    }

    #[test]
    fn every_inherited_marker_is_dropped_from_the_child_environment() {
        // Planted rather than inherited: `CommandBuilder::new` seeds its map
        // from `std::env::vars_os`, so asserting absence on a fresh command
        // would pass on any host that never set these - which is every host
        // for most of the list, and so would pass with the scrub deleted.
        let mut command = CommandBuilder::new("/bin/sh");
        command.env_clear();
        for name in INHERITED_TERMINAL_MARKERS {
            command.env(name, "planted");
        }

        scrub_inherited_markers(&mut command);

        for name in INHERITED_TERMINAL_MARKERS {
            assert_eq!(command.get_env(name), None, "{name} survived the scrub");
        }
        // Each control channel named as well, so that shortening the list is a
        // test failure and not merely a smaller loop: these reach the user's
        // real kitty instance, multiplexer session, Konsole and editor.
        for channel in [
            "KITTY_LISTEN_ON",
            "TMUX",
            "TMUX_PANE",
            "STY",
            "ZELLIJ",
            "ZELLIJ_SESSION_NAME",
            "ZELLIJ_PANE_ID",
            "KONSOLE_DBUS_SERVICE",
            "KONSOLE_DBUS_SESSION",
            "NVIM",
            "NVIM_LISTEN_ADDRESS",
            "VSCODE_IPC_HOOK_CLI",
            "EMACS_SERVER_FILE",
        ] {
            assert!(
                INHERITED_TERMINAL_MARKERS.contains(&channel),
                "{channel} is not scrubbed"
            );
        }
    }

    #[test]
    fn inherited_color_policy_does_not_disable_or_misidentify_the_pty() {
        let mut command = CommandBuilder::new("/bin/sh");
        command.env_clear();
        for (name, value) in [
            ("NO_COLOR", "1"),
            ("FORCE_COLOR", "0"),
            ("CLICOLOR", "0"),
            ("CLICOLOR_FORCE", "1"),
            ("COLORFGBG", "0;15"),
        ] {
            command.env(name, value);
        }
        scrub_inherited_markers(&mut command);
        for name in [
            "NO_COLOR",
            "FORCE_COLOR",
            "CLICOLOR",
            "CLICOLOR_FORCE",
            "COLORFGBG",
        ] {
            assert_eq!(command.get_env(name), None, "{name} reached the PTY");
        }
    }

    #[test]
    fn explicit_recipe_environment_overrides_terminal_defaults() {
        let command = command_from_recipe(&env_recipe(BTreeMap::from([
            ("TERM".to_owned(), "dumb".to_owned()),
            ("KITTY_WINDOW_ID".to_owned(), "3".to_owned()),
            ("NO_COLOR".to_owned(), "1".to_owned()),
            ("FORCE_COLOR".to_owned(), "0".to_owned()),
            ("CLICOLOR".to_owned(), "0".to_owned()),
            ("CLICOLOR_FORCE".to_owned(), "1".to_owned()),
            ("COLORFGBG".to_owned(), "0;15".to_owned()),
            ("COLORTERM".to_owned(), "24bit".to_owned()),
        ])));

        assert_eq!(command.get_env("TERM"), Some(OsStr::new("dumb")));
        assert_eq!(command.get_env("KITTY_WINDOW_ID"), Some(OsStr::new("3")));
        assert_eq!(command.get_env("COLORTERM"), Some(OsStr::new("24bit")));
        assert_eq!(command.get_env("NO_COLOR"), Some(OsStr::new("1")));
        assert_eq!(command.get_env("FORCE_COLOR"), Some(OsStr::new("0")));
        assert_eq!(command.get_env("CLICOLOR"), Some(OsStr::new("0")));
        assert_eq!(command.get_env("CLICOLOR_FORCE"), Some(OsStr::new("1")));
        assert_eq!(command.get_env("COLORFGBG"), Some(OsStr::new("0;15")));
    }

    const SCROLLBACK_LINES: usize = 2000;
    /// A 0.67s flick at 60 Hz presentation.
    const FRAMES: usize = 40;
    /// What a 120 Hz high-resolution trackpad delivers inside one 60 Hz frame,
    /// which is the situation issue #101 is about.
    const NOTCHES_PER_FRAME: usize = 8;
    const LINES_PER_NOTCH: isize = 3;

    /// One measured scroll run: what the pipeline actually did, what it cost,
    /// where it started and where it ended up.
    #[derive(Debug)]
    struct ScrollRun {
        commands: usize,
        captures: usize,
        wire_bytes: usize,
        origin: (u64, String),
        offset: u64,
        screen: String,
        latency: crate::performance::LatencySummary,
    }

    /// Applies `deltas` to a filled scrollback, driving every command through
    /// the viewport re-read, the snapshot capture and the wire encoding the
    /// daemon would have performed for it.
    fn scroll_run(deltas: impl IntoIterator<Item = isize>) -> ScrollRun {
        use crate::performance::{Action, Recorder};
        use crate::terminal::ipc::{TerminalSnapshot, write_frame};
        use std::time::Instant;

        // libghostty sizes scrollback from its own page pool rather than from
        // a row count, so the pool is asked for generously and the workload
        // then checks that it never reached a bound by accident.
        let (mut worker, _events) = capture_worker_with(
            LaunchRecipe {
                program: "/bin/true".into(),
                arguments: Vec::new(),
                working_directory: PathBuf::from("/"),
                environment: BTreeMap::new(),
            },
            TerminalConfig {
                max_scrollback: 4_000_000,
                ..TerminalConfig::default()
            },
        );
        // A deterministic 80-column history, wide enough that a viewport
        // re-read and its wire encoding both do realistic per-cell work.
        let mut history = Vec::with_capacity(SCROLLBACK_LINES * 80);
        for line in 0..SCROLLBACK_LINES {
            history.extend_from_slice(format!("{line:<78}\r\n").as_bytes());
        }
        worker.process_output(history);

        let screen = |snapshot: &RenderSnapshot| {
            snapshot
                .lines
                .iter()
                .map(|line| {
                    line.cells
                        .iter()
                        .map(|cell| cell.text.as_str())
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
                .join("\n")
        };
        let mut recorder = Recorder::default();
        let mut previous = worker.published.snapshot.load_full();
        let origin = (previous.scroll.offset, screen(&previous));
        let mut commands = 0;
        let mut captures = 0;
        let mut wire_bytes = 0;
        for delta in deltas {
            // Returning credit first is what the UI does after drawing, so the
            // worker is free to capture the command that follows.
            worker.published.acknowledge(worker.generation);
            let generation = recorder.commit(Action::TerminalScroll).unwrap();
            assert!(
                !worker.handle_command(Command::Scroll(ScrollCommand::Delta(delta))),
                "a scroll command must not request shutdown"
            );
            let published = worker.published.snapshot.load_full();
            if published.generation != previous.generation {
                captures += 1;
                let mut frame = Vec::new();
                write_frame(&mut frame, &TerminalSnapshot::diff(&published, &previous)).unwrap();
                wire_bytes += frame.len();
            }
            assert!(recorder.redraw_scheduled(generation, Instant::now()));
            previous = published;
            commands += 1;
        }

        let mut durations: Vec<_> = recorder
            .samples()
            .filter_map(|sample| sample.redraw_scheduling_ns.map(Duration::from_nanos))
            .collect();
        ScrollRun {
            commands,
            captures,
            wire_bytes,
            origin,
            offset: previous.scroll.offset,
            screen: screen(&previous),
            latency: crate::performance::LatencySummary::from_durations(&mut durations).unwrap(),
        }
    }

    /// Deterministic scroll workload for issue #101, plus the equivalence that
    /// makes coalescing safe.
    ///
    /// The widget lives in the `ui` feature graph and the emulator in `host`,
    /// so no single build can drive a wheel gesture end to end. This half
    /// measures the expensive half of the pipeline - applying a viewport move
    /// to ghostty, re-reading the viewport into a snapshot, and encoding that
    /// snapshot for the socket - and proves that folding a frame's notches
    /// into one command lands on exactly the same screen. The folding itself
    /// is covered by `terminal::widget::tests`.
    ///
    /// Timings are printed, never asserted: a `test`-profile build with an
    /// unoptimized parser gives a ratio, not a latency budget, and this crate
    /// treats even release numbers as non-gating.
    #[test]
    fn coalescing_a_frame_of_notches_costs_one_capture_and_lands_on_the_same_screen() {
        let notches = FRAMES * NOTCHES_PER_FRAME;
        let per_event = scroll_run(std::iter::repeat_n(-LINES_PER_NOTCH, notches));
        let per_frame = scroll_run(std::iter::repeat_n(
            -LINES_PER_NOTCH * NOTCHES_PER_FRAME as isize,
            FRAMES,
        ));

        // The whole point: the same gesture reaches the same place, showing
        // the same rows, for an eighth of the commands and captures.
        assert_eq!(per_frame.offset, per_event.offset);
        assert_eq!(per_frame.screen, per_event.screen);
        assert_eq!(per_event.captures, notches);
        assert_eq!(per_frame.captures, FRAMES);
        // A run that clamped at the top would satisfy the equality above
        // without having measured scrolling at all.
        assert!(
            per_event.offset > 0,
            "the workload must stay inside scrollback rather than clamp"
        );

        let budget = crate::performance::PerformanceBudget {
            // One coalesced move has to fit inside a 60 Hz frame alongside the
            // rest of that frame's work, so a quarter of a frame at p95.
            interaction_p95: Duration::from_micros(4_166),
            interaction_p99: Duration::from_micros(8_333),
            ..crate::performance::PerformanceBudget::default()
        };
        for (policy, run) in [("per-event", &per_event), ("per-frame", &per_frame)] {
            println!(
                "scroll workload [{policy}] commands={} captures={} wire_bytes={} \
                 p50={:?} p95={:?} p99={:?} worst={:?} within_budget={} (non-gating)",
                run.commands,
                run.captures,
                run.wire_bytes,
                run.latency.median,
                run.latency.p95,
                run.latency.p99,
                run.latency.worst,
                budget.accepts(run.latency)
            );
        }
        println!(
            "scroll workload [saving] commands x{:.2} wire_bytes x{:.2} total_capture_time \
             {:?} -> {:?}",
            per_event.commands as f64 / per_frame.commands as f64,
            per_event.wire_bytes as f64 / per_frame.wire_bytes as f64,
            per_event.latency.median * per_event.commands as u32,
            per_frame.latency.median * per_frame.commands as u32
        );
    }

    /// Folding must not change where a gesture stops, which is the one place
    /// a summed delta could plausibly differ from the notches it replaced.
    #[test]
    fn overshooting_either_bound_clamps_the_same_whether_or_not_it_was_coalesced() {
        let overshoot = SCROLLBACK_LINES as isize * 2;
        let notches = (overshoot / LINES_PER_NOTCH) as usize;

        let top_per_event = scroll_run(std::iter::repeat_n(-LINES_PER_NOTCH, notches));
        let top_per_frame = scroll_run(std::iter::repeat_n(
            -LINES_PER_NOTCH * NOTCHES_PER_FRAME as isize,
            notches / NOTCHES_PER_FRAME,
        ));
        assert_eq!(top_per_event.offset, 0, "the top of scrollback is row zero");
        assert_eq!(top_per_frame.offset, 0);
        assert_eq!(top_per_frame.screen, top_per_event.screen);

        // Coming back down has to land on the live screen again, not one row
        // short of it or past the end of history.
        let bottom_per_event = scroll_run(
            std::iter::repeat_n(-LINES_PER_NOTCH, notches)
                .chain(std::iter::repeat_n(LINES_PER_NOTCH, notches)),
        );
        let bottom_per_frame = scroll_run(
            std::iter::repeat_n(
                -LINES_PER_NOTCH * NOTCHES_PER_FRAME as isize,
                notches / NOTCHES_PER_FRAME,
            )
            .chain(std::iter::repeat_n(
                LINES_PER_NOTCH * NOTCHES_PER_FRAME as isize,
                notches / NOTCHES_PER_FRAME,
            )),
        );
        assert_eq!(bottom_per_frame.offset, bottom_per_event.offset);
        assert_eq!(bottom_per_frame.screen, bottom_per_event.screen);
        assert_eq!(
            (bottom_per_event.offset, &bottom_per_event.screen),
            (bottom_per_event.origin.0, &bottom_per_event.origin.1),
            "returning to the bottom must show the live screen again"
        );
    }
}
