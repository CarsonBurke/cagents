mod admission;
mod archive;
mod carry;
mod close_processes;
#[cfg(test)]
mod close_tests;
#[cfg(test)]
mod event_dispatch_tests;
pub(crate) mod holder;
mod input_queue;
mod joins;
#[cfg(test)]
mod pty_reader_tests;

use close_processes::{FrozenFamily, RootIdentity};

use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, VecDeque},
    fs::File,
    io::{BufWriter, Read, Write},
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
    path::PathBuf,
    rc::Rc,
    sync::{
        Arc, Condvar, Mutex, OnceLock, Weak,
        atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
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
    model::{
        LaunchRecipe, TerminalId, TerminalOutputActivity, TerminalSession,
        agent_recovery::AgentProcess,
    },
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
/// Encoded ordinary input plus pre-reserved physical-release capacity.
const PTY_WRITE_CAPACITY: usize = admission::ORDINARY_BYTES + admission::CLEANUP_LIMIT * 512 + 128;
const PTY_WRITE_RETRY: Duration = Duration::from_millis(10);
/// Marker appended once when a transcript hits its size cap.
const TRANSCRIPT_TRUNCATED_MARKER: &[u8] = b"\n[cagents transcript truncated: size cap reached]\n";
/// Foreground observation cadence. Every tick descends `/proc` over the PTY
/// foreground subtree, so it is only worth paying while that group is moving:
/// an unchanged group backs the interval off, any change resets it, and a
/// terminal nobody is watching is never observed on the timer at all.
const FOREGROUND_POLL_MIN: Duration = Duration::from_secs(2);
const FOREGROUND_POLL_MAX: Duration = Duration::from_secs(30);
/// Allow a root's HUP handler to finish before forcing its pinned family.
const CHILD_HANGUP_GRACE: Duration = Duration::from_millis(500);
/// Total budget for hangup, escalation and an authoritative holder exit report.
const CHILD_SHUTDOWN_DEADLINE: Duration = Duration::from_secs(3);
/// Keep a bounded tail for exact-child exit and PTY/session confirmation.
const CHILD_SHUTDOWN_CONFIRMATION_RESERVE: Duration = CHILD_HANGUP_GRACE;
/// Within that tail, the final session census keeps this much for itself.
const CHILD_SHUTDOWN_CENSUS_RESERVE: Duration = Duration::from_millis(250);
/// Actual OS joins happen off-caller; ordinary shutdown shares this deadline.
const WORKER_JOIN_TIMEOUT: Duration = Duration::from_secs(5);

/// The wall-clock budget ordinary shutdown gives a terminal's family, and
/// itself, to end and confirm that end. The product always runs with
/// [`ShutdownBudget::PRODUCT`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct ShutdownBudget {
    /// Total for hangup, escalation and an authoritative holder exit report.
    deadline: Duration,
    /// How long the family may run its hangup handlers before it is stopped.
    hangup_grace: Duration,
    /// The tail of `deadline` kept to confirm the exact child's exit and the
    /// PTY session's end.
    confirmation_reserve: Duration,
    /// Within that tail, what the final session census keeps for itself.
    census_reserve: Duration,
    /// How long a caller of [`TerminalHandle::shutdown`] waits for the worker.
    join: Duration,
}

impl ShutdownBudget {
    const PRODUCT: Self = Self {
        deadline: CHILD_SHUTDOWN_DEADLINE,
        hangup_grace: CHILD_HANGUP_GRACE,
        confirmation_reserve: CHILD_SHUTDOWN_CONFIRMATION_RESERVE,
        census_reserve: CHILD_SHUTDOWN_CENSUS_RESERVE,
        join: WORKER_JOIN_TIMEOUT,
    };

    /// For a test of what shutdown decides rather than how fast a shared,
    /// loaded host runs it. Each wait in shutdown ends as soon as its
    /// condition holds, so only a step that never finishes can exhaust these
    /// budgets. The hangup grace stays the product's, since it decides when a
    /// family that ignores its hangup is stopped.
    #[cfg(test)]
    pub(super) const PATIENT: Self = Self {
        deadline: Duration::from_secs(30),
        hangup_grace: CHILD_HANGUP_GRACE,
        confirmation_reserve: Duration::from_secs(15),
        census_reserve: Duration::from_secs(5),
        join: Duration::from_secs(45),
    };
}

/// How long shutdown waits for the PTY reader to leave its blocking read
/// before abandoning the thread. Closing its wake pipe releases the read
/// immediately in every case we can construct; this only bounds the ones we
/// cannot, because a hung join here wedges the daemon's dispatch thread.
const READER_SHUTDOWN_GRACE: Duration = Duration::from_millis(500);
/// A full parser inbox must not hide closure of the reader's original wake
/// pipe. Keep backpressure lossless while the owner is running, and check its
/// cancellation between bounded channel waits.
const READER_SEND_POLL: Duration = Duration::from_millis(10);
/// How long a synchronized update (DEC mode 2026) may withhold output from the
/// published grid. A program brackets a frame it has to emit in several writes -
/// clearing the screen, then repainting it - so that no half-drawn frame is
/// shown; reads split that frame arbitrarily, and publishing between them
/// flashes an empty screen. Ghostty bounds the hold the same way, so a program
/// that dies or never ends the update cannot freeze its card.
const SYNCHRONIZED_OUTPUT_TIMEOUT: Duration = Duration::from_secs(1);
/// Bells signal attention, not a count of bytes. Publish the first promptly,
/// then at most one pending reminder per interval, independently of PTY rate.
const BELL_INTERVAL: Duration = Duration::from_millis(100);

#[derive(Debug)]
enum Command {
    InputClaim {
        claim: Arc<admission::Cleanup>,
        ticket: Option<admission::QueuedPress>,
        command: Box<Command>,
    },
    Admitted {
        permit: Arc<admission::Permit>,
        command: Box<Command>,
    },
    Interrupt(Option<KeyInput>),
    OrderedInput {
        generation: u64,
        // Lives through encoding/cancellation, not through PTY consumption.
        admission: Option<Arc<u64>>,
        input: Box<Command>,
    },

    Input(Vec<u8>),
    Paste(String, PasteOrigin),
    Key(KeyInput),
    Mouse(MouseInput),
    ReadViewport {
        offset: Option<u64>,
        /// `None` reads as many rows as the terminal has.
        rows: Option<u16>,
        reply: Sender<Result<Arc<RenderSnapshot>, String>>,
    },
    Focus(bool),
    Resize(TerminalSize),
    Scroll(ScrollCommand),
    SetTheme(TerminalTheme),
    ClearScrollback,
    CopySelection {
        request: uuid::Uuid,
        selection: super::SearchMatch,
    },
    Search {
        owner: Option<u64>,
        request: u64,
        query: String,
        case_sensitive: bool,
        latest: Arc<AtomicU64>,
    },
    Hide,
    Reveal,
    Hibernate,
    Restore {
        visible: bool,
    },
    ObserveAgent(Sender<Option<String>>),
    PrepareInputHandoff {
        token: Arc<AtomicBool>,
        resumed: Sender<Result<(), String>>,
        reply: Sender<Result<(), String>>,
    },
    PrepareUpgrade(Sender<Result<UpgradeSnapshot, String>>),
    Carry {
        socket: PathBuf,
        reply: Sender<Result<Carried, String>>,
    },
    PreserveForRestart(Sender<Result<(), String>>),
    ObserveAgentProcess(Sender<Option<AgentProcess>>),
    ApplyAgentActivity {
        process: Box<AgentProcess>,
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
    #[cfg(test)]
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
    /// Foreground observation or a synchronized update's hold came due.
    Timer,
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
    WorkingDirectoryChanged,
}

/// Collapse parser metadata before it can allocate one effect per escape or
/// bell byte. PTY replies remain ordered and lossless; titles and cwd describe
/// the final parser state of this batch.
#[derive(Default)]
struct Effects {
    ordered: Vec<Effect>,
    bell: bool,
    title: Option<usize>,
    cwd: bool,
}

impl Effects {
    fn push(&mut self, effect: Effect) {
        match effect {
            Effect::Bell => {
                if !std::mem::replace(&mut self.bell, true) {
                    self.ordered.push(Effect::Bell);
                }
            }
            Effect::WorkingDirectoryChanged => {
                if !std::mem::replace(&mut self.cwd, true) {
                    self.ordered.push(Effect::WorkingDirectoryChanged);
                }
            }
            Effect::Title(title) => {
                if let Some(index) = self.title {
                    self.ordered[index] = Effect::Title(title);
                } else {
                    self.title = Some(self.ordered.len());
                    self.ordered.push(Effect::Title(title));
                }
            }
            effect => self.ordered.push(effect),
        }
    }

    fn retain_title(&mut self) {
        let old = std::mem::take(self);
        for effect in old.ordered {
            if matches!(effect, Effect::Title(_)) {
                self.push(effect);
            }
        }
    }
}

#[derive(Default)]
struct Bell {
    pending: bool,
    next: Option<std::time::Instant>,
}

impl Bell {
    fn take_ready(&mut self, now: std::time::Instant) -> bool {
        if self.pending && self.next.is_none_or(|next| now >= next) {
            self.pending = false;
            self.next = Some(now + BELL_INTERVAL);
            true
        } else {
            false
        }
    }

    fn release_after(&self) -> Option<Duration> {
        self.pending.then(|| {
            self.next.map_or(Duration::ZERO, |next| {
                next.saturating_duration_since(std::time::Instant::now())
            })
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Visibility {
    Visible,
    Hidden,
    Hibernated,
}

/// The visibility the host last asked a live worker for, until the worker
/// applies it.
///
/// Host visibility is a level, not an edge: only the newest request matters,
/// so it is a slot the worker reconciles rather than a command in its bounded
/// queue. A queued request could be refused when the queue is full, and a
/// refused reveal left the worker hidden, capturing nothing, while a client
/// showed it. It could also be overtaken: a reveal accepted while an earlier
/// hide waited in the queue was undone by that hide.
struct PendingVisibility(AtomicU8);

impl PendingVisibility {
    const NONE: u8 = 0;

    fn new() -> Self {
        Self(AtomicU8::new(Self::NONE))
    }

    fn set(&self, visibility: Visibility) {
        let encoded = match visibility {
            Visibility::Visible => 1,
            Visibility::Hidden => 2,
            Visibility::Hibernated => 3,
        };
        self.0.store(encoded, Ordering::Release);
    }

    fn take(&self) -> Option<Visibility> {
        Self::decode(self.0.swap(Self::NONE, Ordering::AcqRel))
    }

    fn decode(encoded: u8) -> Option<Visibility> {
        match encoded {
            1 => Some(Visibility::Visible),
            2 => Some(Visibility::Hidden),
            3 => Some(Visibility::Hibernated),
            _ => None,
        }
    }
}

type WakeCallback = dyn Fn(TerminalWake) + Send + Sync + 'static;

enum ArchivePhase {
    Live,
    Preparing,
    Frozen {
        history: Result<archive::History, String>,
        events: Arc<Sender<TerminalEvent>>,
    },
    Released,
}

type ArchiveObserver = dyn Fn(TerminalEvent) + Send + Sync;

/// Dispatches live and completed terminals' subscriber callbacks off-thread.
///
/// A completed terminal has no worker, so its commands execute on whichever
/// thread sends them, and the daemon sends them while holding the host mutex
/// its wake and event callbacks lock. Calling either inline deadlocks that
/// thread against itself and, behind it, every other user of the host. The
/// same shared thread delivers live semantic events without an idle monitor
/// thread per terminal. Each terminal's events retain their publication order.
type Notice = Box<dyn FnOnce() + Send>;

fn event_notifier() -> Result<&'static Sender<Notice>, String> {
    static NOTIFIER: OnceLock<Result<Sender<Notice>, String>> = OnceLock::new();
    NOTIFIER
        .get_or_init(|| {
            let (notices, pending) = crossbeam_channel::unbounded::<Notice>();
            thread::Builder::new()
                .name("terminal-event-notify".into())
                .spawn(move || {
                    for notice in pending {
                        // A subscriber panic must not lose later terminals'
                        // metadata or completion notifications.
                        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(notice));
                    }
                })
                .map_err(|error| format!("start shared terminal event monitor: {error}"))?;
            Ok(notices)
        })
        .as_ref()
        .map_err(Clone::clone)
}

fn notify_archived(notice: impl FnOnce() + Send + 'static) {
    match event_notifier() {
        Ok(notifier) => {
            let _ = notifier.send(Box::new(notice));
        }
        Err(error) => tracing::error!(%error, "terminals cannot notify their subscribers"),
    }
}

/// Returns once every notice queued before this call has run.
#[cfg(test)]
pub(crate) fn flush_archived_notices() {
    let (done, flushed) = crossbeam_channel::bounded(1);
    notify_archived(move || {
        let _ = done.send(());
    });
    flushed
        .recv_timeout(Duration::from_secs(5))
        .expect("the archive notifier stalled");
}

#[derive(Default)]
struct InputOrder {
    /// Ownership change or checked close has fenced admission until its outcome is known.
    handoff: bool,
    prepared_input: Option<Arc<AtomicBool>>,
    cleanup_sealed: bool,
    generation: u64,
    interrupt_key: Option<KeyInput>,
    interrupt_claim: Option<Arc<admission::Cleanup>>,
    interrupt_ticket: Option<admission::QueuedPress>,
    next_sequence: u64,
    pending: VecDeque<Weak<u64>>,
}

impl InputOrder {
    fn next_sequence(&mut self) -> u64 {
        self.next_sequence = self
            .next_sequence
            .checked_add(1)
            .expect("terminal input admission order exhausted");
        self.next_sequence
    }

    fn track_input(&mut self) -> Arc<u64> {
        // At most one ticket per accepted ordinary input command, plus the
        // current admission attempt while this same lock is held. Tickets do
        // not retain encoded byte credits or wait for the child to read.
        self.pending.retain(|ticket| ticket.strong_count() != 0);
        let ticket = Arc::new(self.next_sequence());
        self.pending.push_back(Arc::downgrade(&ticket));
        ticket
    }

    fn earliest_pending(&mut self) -> Option<u64> {
        while let Some(front) = self.pending.front() {
            if let Some(ticket) = front.upgrade() {
                return Some(*ticket);
            }
            self.pending.pop_front();
        }
        None
    }
}

struct PublishedState {
    execution_instance: uuid::Uuid,
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
    searches: Mutex<HashMap<Option<u64>, Weak<AtomicU64>>>,
    archive: Mutex<ArchivePhase>,
    archive_ready: Condvar,
    archive_observer: OnceLock<Arc<ArchiveObserver>>,
    event_observer: Mutex<Option<Arc<ArchiveObserver>>>,
    completed: AtomicBool,
    /// An adopted original has not proved its startup commit. Its raw prefix
    /// and parked reader remain on the original worker until explicit Close.
    startup_unresolved: AtomicBool,
    startup_diagnostic: Mutex<Option<String>>,
    #[cfg(test)]
    test_fail_startup_commit: AtomicBool,
    #[cfg(test)]
    test_startup_probe: Mutex<Option<Sender<(Vec<u8>, bool, bool)>>>,
    shutdown_requested: AtomicBool,
    #[cfg(test)]
    shutdown_census_failure: AtomicBool,
    #[cfg(test)]
    shutdown_census_delay: Mutex<Option<Sender<Instant>>>,
    #[cfg(test)]
    shutdown_budget: Mutex<ShutdownBudget>,
    #[cfg(test)]
    test_holder_reservation: Mutex<Option<crate::daemon::child_reaper::Reservation>>,
    interrupt_requested: AtomicBool,
    input_order: Mutex<InputOrder>,
    command_budget: OnceLock<Arc<admission::Budget>>,
    input_storage: admission::StoragePool,
    priority_permit: Arc<admission::Permit>,
    cleanup_pool: admission::CleanupPool,
    cleanup_commands: Mutex<VecDeque<(Arc<admission::Cleanup>, u64, Command)>>,
    priority_close: Mutex<Option<Command>>,
    #[cfg(test)]
    close_inspection_gate: Mutex<Option<(Sender<()>, Receiver<()>)>>,
    #[cfg(test)]
    test_input_root_pidfd: OnceLock<File>,
    /// Final result ownership survives emulator teardown and daemon crashes.
    retained_holder: Mutex<Option<holder::Holder>>,
    holder_identity: OnceLock<super::holder::Record>,
    launch_directory: OnceLock<(holder::LaunchDirectoryPin, File)>,
    original_holder_process: OnceLock<Arc<File>>,
    original_holder_role: OnceLock<crate::daemon::holder_registry::OriginalRole>,
    // The source descriptors pin both identities through terminal retirement;
    // numeric inode receipts alone cannot distinguish later inode reuse.
    transcript_location: OnceLock<crate::transcript_location::ValidatedTranscript>,
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
    /// What [`TerminalHandle::set_host_visibility`] last asked a live worker
    /// for, until the worker applies it.
    host_visibility: PendingVisibility,
    /// The newest generation published before the worker last stopped
    /// showing the terminal. `visible` turns on when a reveal starts, before
    /// its capture is published or while a synchronized update holds it back;
    /// until a later generation is published, the screen is from before.
    hidden_through: AtomicU64,
    wake: Arc<WakeCallback>,
    output_observer: OnceLock<Arc<dyn Fn(u64) + Send + Sync>>,
    output_armed: AtomicBool,
}

impl PublishedState {
    fn has_retained_holder(&self) -> bool {
        self.retained_holder
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .is_some()
    }

    fn publish_released_archive_if_available(&self) {
        let archive = match self.archive.try_lock() {
            Ok(guard) => Some(guard),
            Err(std::sync::TryLockError::Poisoned(error)) => Some(error.into_inner()),
            Err(std::sync::TryLockError::WouldBlock) => None,
        };
        if let Some(mut archive) = archive {
            *archive = ArchivePhase::Released;
            self.archive_ready.notify_all();
        }
    }

    fn release_completed_holder(&self) {
        *self.archive.lock().unwrap_or_else(|p| p.into_inner()) = ArchivePhase::Released;
        self.archive_ready.notify_all();
        let holder = self
            .retained_holder
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take();
        drop(holder);
    }

    fn new(visible: bool, wake: Arc<WakeCallback>) -> Self {
        let (capture_tx, capture_rx) = crossbeam_channel::bounded(1);
        let cleanup_pool = admission::CleanupPool::new(capture_tx.clone());
        Self {
            snapshot: ArcSwap::from_pointee(RenderSnapshot::empty()),
            execution_instance: uuid::Uuid::new_v4(),
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
            searches: Mutex::new(HashMap::new()),
            archive: Mutex::new(ArchivePhase::Live),
            archive_ready: Condvar::new(),
            archive_observer: OnceLock::new(),
            event_observer: Mutex::new(None),
            completed: AtomicBool::new(false),
            startup_unresolved: AtomicBool::new(false),
            startup_diagnostic: Mutex::new(None),
            #[cfg(test)]
            test_fail_startup_commit: AtomicBool::new(false),
            #[cfg(test)]
            test_startup_probe: Mutex::new(None),
            shutdown_requested: AtomicBool::new(false),
            #[cfg(test)]
            shutdown_census_failure: AtomicBool::new(false),
            #[cfg(test)]
            shutdown_census_delay: Mutex::new(None),
            #[cfg(test)]
            shutdown_budget: Mutex::new(ShutdownBudget::PRODUCT),
            #[cfg(test)]
            test_holder_reservation: Mutex::new(None),
            interrupt_requested: AtomicBool::new(false),
            input_order: Mutex::new(InputOrder::default()),
            command_budget: OnceLock::new(),
            input_storage: admission::StoragePool::ordinary(),
            priority_permit: admission::Permit::generated(
                admission::StoragePool::ordinary().reserve(128, 4).unwrap(),
            ),
            cleanup_pool,
            cleanup_commands: Mutex::new(VecDeque::new()),
            priority_close: Mutex::new(None),
            #[cfg(test)]
            close_inspection_gate: Mutex::new(None),
            #[cfg(test)]
            test_input_root_pidfd: OnceLock::new(),
            retained_holder: Mutex::new(None),
            holder_identity: OnceLock::new(),
            launch_directory: OnceLock::new(),
            original_holder_process: OnceLock::new(),
            original_holder_role: OnceLock::new(),
            transcript_location: OnceLock::new(),
            announced_generation: AtomicU64::new(0),
            visible: AtomicBool::new(visible),
            host_visibility: PendingVisibility::new(),
            hidden_through: AtomicU64::new(0),
            wake,
            output_observer: OnceLock::new(),
            output_armed: AtomicBool::new(false),
        }
    }

    fn request_interrupt(&self, key: Option<KeyInput>) -> bool {
        let mut order = self.input_order.lock().unwrap_or_else(|p| p.into_inner());
        if order.handoff || self.shutdown_requested.load(Ordering::Acquire) {
            return false;
        }
        let prepared = if let Some(input) = &key {
            let Some(prepared) = self.cleanup_pool.press(admission::Identity::key(input)) else {
                return false;
            };
            Some(prepared)
        } else {
            None
        };
        if let Some((claim, ticket)) = prepared {
            order.interrupt_ticket = ticket;
            order.interrupt_claim = Some(claim);
        } else {
            order.interrupt_ticket = None;
            order.interrupt_claim = None;
        }
        order.generation = order
            .generation
            .checked_add(1)
            .expect("terminal input generation exhausted");
        order.interrupt_key = key;
        self.interrupt_requested.store(true, Ordering::Release);
        let _ = self.capture_tx.try_send(());
        true
    }

    fn take_cleanup_command(&self, ready_only: bool) -> Option<Command> {
        // Match producer lock order: admission before the reserved mailbox.
        // A later owned up cannot overtake earlier accepted input merely
        // because its own down has already encoded. Earlier input only needs
        // to encode or be explicitly canceled; backpressured bytes stay FIFO.
        let mut order = self.input_order.lock().unwrap_or_else(|p| p.into_inner());
        let earliest = order.earliest_pending();
        let mut commands = self
            .cleanup_commands
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let index = commands.iter().position(|(claim, sequence, _)| {
            !ready_only || claim.ready() && earliest.is_none_or(|pending| pending >= *sequence)
        })?;
        let (claim, _, command) = commands.remove(index).unwrap();
        Some(Command::InputClaim {
            claim,
            ticket: None,
            command: Box::new(command),
        })
    }

    fn shutdown_budget(&self) -> ShutdownBudget {
        #[cfg(test)]
        return *self
            .shutdown_budget
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        #[cfg(not(test))]
        ShutdownBudget::PRODUCT
    }

    fn request_shutdown(&self) {
        self.shutdown_requested.store(true, Ordering::Release);
        // Wake the same select loop as capture credit. A full wake channel
        // already guarantees a pending event; shutdown cannot wait on input.
        let _ = self.capture_tx.try_send(());
    }

    fn prepare_search(&self, owner: Option<u64>, request: u64) -> Arc<AtomicU64> {
        let mut searches = self.searches.lock().unwrap_or_else(|p| p.into_inner());
        // Only queued or active searches retain their cancellation token.
        searches.retain(|_, token| token.strong_count() > 0);
        let latest = searches
            .get(&owner)
            .and_then(Weak::upgrade)
            .unwrap_or_else(|| {
                let latest = Arc::new(AtomicU64::new(request));
                searches.insert(owner, Arc::downgrade(&latest));
                latest
            });
        latest.store(request, Ordering::Release);
        latest
    }

    fn archived_command(self: &Arc<Self>, command: Command) -> Result<(), TerminalSendError> {
        match command {
            Command::Admitted {
                permit: _permit,
                command,
            } => return self.archived_command(*command),
            Command::InputClaim {
                claim: _claim,
                ticket: _ticket,
                command,
            } => return self.archived_command(*command),
            Command::OrderedInput {
                admission: _admission,
                input,
                ..
            } => return self.archived_command(*input),
            _ => {}
        }
        if self.shutdown_requested.load(Ordering::Acquire) {
            return Err(TerminalSendError::Closed);
        }
        // Visibility and focus belong to the retained surface, independent
        // of whether its completed scrollback could be archived. They do not
        // need the parser or history, and must not repeat a retention failure.
        let visible = match &command {
            Command::Hide | Command::Hibernate => Some(false),
            Command::Reveal => Some(true),
            Command::Restore { visible } => Some(*visible),
            Command::Focus(_) => return Ok(()),
            _ => None,
        };
        if let Some(visible) = visible {
            // HostState already offers the retained generation on reveal.
            // A wake here could reenter the host mutex held by its caller.
            self.visible.store(visible, Ordering::Release);
            return Ok(());
        }
        if !matches!(
            &command,
            Command::ReadViewport { .. }
                | Command::Search { .. }
                | Command::CopySelection { .. }
                | Command::Scroll(_)
                | Command::Resize(_)
                | Command::SetTheme(_)
                | Command::ClearScrollback
                | Command::Hide
                | Command::Hibernate
                | Command::Reveal
                | Command::Restore { .. }
                | Command::Focus(_)
        ) {
            return Err(TerminalSendError::Closed);
        }
        let mut phase = self.archive.lock().unwrap_or_else(|p| p.into_inner());
        let ArchivePhase::Frozen { history, events } = &mut *phase else {
            return Err(TerminalSendError::Closed);
        };
        let events = events.clone();
        let result = match history {
            Err(message) => match command {
                Command::ReadViewport { reply, .. } => {
                    // Completion already reported the loss once. The parser
                    // is gone; its final retained screen is still readable,
                    // even when no scrollback could replace it.
                    let _ = reply.send(Ok(self.snapshot.load_full()));
                    return Ok(());
                }
                Command::Scroll(_)
                | Command::Resize(_)
                | Command::SetTheme(_)
                | Command::ClearScrollback => return Ok(()),
                _ => Some(TerminalEvent::Error {
                    operation: "review completed terminal",
                    message: message.clone(),
                }),
            },
            Ok(history) => {
                match command {
                    Command::Search {
                        owner,
                        request,
                        query,
                        case_sensitive,
                        latest,
                    } => {
                        let source = history.source();
                        drop(phase);
                        let result = source
                            .search(&query, case_sensitive, || {
                                latest.load(Ordering::Acquire) != request
                            })
                            .map(|(matches, truncated)| TerminalEvent::SearchResults {
                                owner,
                                request,
                                query,
                                matches,
                                truncated,
                            });
                        if let Some(event) = result {
                            self.emit_archived(&events, event);
                        }
                        return Ok(());
                    }
                    Command::CopySelection { request, selection } => {
                        let source = history.source();
                        drop(phase);
                        let event = match source.copy(selection) {
                            Ok(text) => TerminalEvent::ClipboardText { request, text },
                            Err(message) => TerminalEvent::Error {
                                operation: "copy completed terminal selection",
                                message,
                            },
                        };
                        self.emit_archived(&events, event);
                        return Ok(());
                    }
                    Command::ReadViewport {
                        offset,
                        rows,
                        reply,
                    } => {
                        let _ = reply.send(Ok(history.snapshot_at(
                            offset,
                            rows,
                            self.published_generation.load(Ordering::Acquire),
                        )));
                        return Ok(());
                    }
                    Command::Scroll(scroll) => history.scroll(&scroll),
                    Command::Resize(size) => history.resize(size),
                    Command::SetTheme(theme) => history.set_theme(theme),
                    Command::ClearScrollback => history.clear(),
                    _ => return Err(TerminalSendError::Closed),
                }
                let snapshot = history.snapshot(
                    self.published_generation
                        .load(Ordering::Acquire)
                        .saturating_add(1),
                );
                // Serialize mutations and publication; wake outside the lock
                // so subscriber callbacks can inspect the retained surface,
                // and off this thread because the sender may hold the host.
                let generation = snapshot.generation;
                self.snapshot.store(snapshot);
                self.published_generation
                    .store(generation, Ordering::Release);
                drop(phase);
                let published = Arc::downgrade(self);
                notify_archived(move || {
                    // Announce whatever is newest by now, so a burst of
                    // commands queued behind the host costs one redraw.
                    if let Some(published) = published.upgrade() {
                        published
                            .request_wake(published.published_generation.load(Ordering::Acquire));
                    }
                });
                return Ok(());
            }
        };
        drop(phase);
        if let Some(event) = result {
            self.emit_archived(&events, event);
        }
        Ok(())
    }

    fn emit_event(&self, events: &Sender<TerminalEvent>, event: TerminalEvent) {
        // Registration drains the startup queue under this same lock, so an
        // event cannot overtake the startup replay or be delivered twice.
        let observer = self
            .event_observer
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        if let Some(observer) = observer.as_ref().cloned() {
            // Callers can hold Host or agent-state locks. Only queue here;
            // user callbacks run on the shared dispatcher after those release.
            notify_archived(move || observer(event));
        } else {
            let _ = events.send(event);
        }
    }

    fn emit_archived(&self, events: &Sender<TerminalEvent>, event: TerminalEvent) {
        if let Some(observer) = self.archive_observer.get().cloned() {
            notify_archived(move || observer(event));
        } else {
            self.emit_event(events, event);
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
                self.emit_event(&events, TerminalEvent::AgentActivityChanged(activity));
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

    /// Stops announcing frames and marks what is published as the screen
    /// from before, which a reveal must replace before reads can trust it.
    fn stop_presenting(&self) {
        self.visible.store(false, Ordering::Release);
        self.hidden_through.store(
            self.published_generation.load(Ordering::Acquire),
            Ordering::Release,
        );
    }

    /// Admits commands to what the completed terminal left. Nothing is
    /// published after this, so the published screen is its screen even when
    /// it completed hidden and no history could be kept to replace it.
    fn freeze(
        &self,
        history: Result<archive::History, String>,
        events: Arc<Sender<TerminalEvent>>,
    ) {
        self.hidden_through.store(0, Ordering::Release);
        *self.archive.lock().unwrap_or_else(|p| p.into_inner()) =
            ArchivePhase::Frozen { history, events };
        self.archive_ready.notify_all();
    }

    fn frozen(&self) -> bool {
        matches!(
            *self.archive.lock().unwrap_or_else(|p| p.into_inner()),
            ArchivePhase::Frozen { .. }
        )
    }

    /// Whether `snapshot`, read from this state, is the screen the worker is
    /// showing now rather than one from before it was last hidden.
    fn presents(&self, snapshot: &RenderSnapshot) -> bool {
        self.visible.load(Ordering::Acquire)
            && snapshot.generation > self.hidden_through.load(Ordering::Acquire)
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
    worker: Arc<joins::JoinOwner>,
}

impl Drop for HandleInner {
    fn drop(&mut self) {
        // Dropping an error/observer is not authorization to destroy an
        // adopted replay prefix. HOST owns its separate slot before commit.
        if self.published.startup_unresolved.load(Ordering::Acquire) {
            return;
        }
        self.published.request_shutdown();
        // The body can be this very thread or blocked in a callback/TLS.
        // Pending custody outlives this handle; destruction never joins here.
        joins::request(&self.worker, true, true);
    }
}

#[derive(Clone)]
pub struct TerminalHandle {
    inner: Arc<HandleInner>,
}

/// Retirement observes actual join and the exact original holder's exit.
/// Completion or transfer to the child reaper alone is not a release receipt.
pub(crate) enum TerminalRetirement {
    Pending,
    WaitingForHolder(Arc<File>),
    Released,
    Retained(String),
}

/// Callback identity without retaining the worker through its own wake closure.
pub(crate) struct WeakTerminalHandle {
    inner: Weak<HandleInner>,
}

/// Real publication and callback machinery without a PTY, child or worker thread.
/// This fixture tests callback identity, not process retirement or native output.
#[cfg(test)]
pub(crate) struct RenderStreamFixture {
    pub(crate) handle: TerminalHandle,
    events: Arc<Sender<TerminalEvent>>,
    commands: Receiver<Command>,
}

#[cfg(test)]
impl RenderStreamFixture {
    pub(crate) fn new(wake: impl Fn(TerminalWake) + Send + Sync + 'static) -> Self {
        let (commands, pending) = command_channel(64);
        let (events, received) = crossbeam_channel::unbounded();
        let events = Arc::new(events);
        let published = Arc::new(PublishedState::new(true, Arc::new(wake)));
        let _ = published.semantic_events.set(Arc::downgrade(&events));
        Self {
            handle: TerminalHandle {
                inner: Arc::new(HandleInner {
                    commands,
                    events: received,
                    published: published.clone(),
                    worker: joins::JoinOwner::empty(published),
                }),
            },
            events,
            commands: pending,
        }
    }

    pub(crate) fn publish(&self, generation: u64) {
        let mut snapshot = RenderSnapshot::empty();
        snapshot.generation = generation;
        self.handle.inner.published.publish(Arc::new(snapshot));
    }

    /// Publishes a caller-built grid, so row identity can be controlled.
    pub(crate) fn publish_snapshot(&self, snapshot: RenderSnapshot) {
        self.handle.inner.published.publish(Arc::new(snapshot));
    }

    pub(crate) fn emit(&self, event: TerminalEvent) {
        self.handle.inner.published.emit_event(&self.events, event);
    }

    pub(crate) fn queued_commands(&self) -> usize {
        self.commands.len()
    }

    /// Takes what a worker would apply next: the host's pending visibility
    /// request, or else the last queued command that sets visibility. Returns
    /// whether it leaves the worker visible. Queued commands are drained.
    pub(crate) fn drain_visibility(&self) -> Option<bool> {
        fn visibility(command: &Command) -> Option<bool> {
            match command {
                Command::Admitted { command, .. } => visibility(command),
                Command::Hide | Command::Hibernate => Some(false),
                Command::Reveal => Some(true),
                Command::Restore { visible } => Some(*visible),
                _ => None,
            }
        }
        let queued = self
            .commands
            .try_iter()
            .fold(None, |visible, command| visibility(&command).or(visible));
        self.handle
            .inner
            .published
            .host_visibility
            .take()
            .map(|pending| pending == Visibility::Visible)
            .or(queued)
    }

    /// Takes the host's visibility request the worker has not applied yet.
    pub(crate) fn take_host_visibility(&self) -> Option<super::ipc::TerminalVisibility> {
        use super::ipc::TerminalVisibility;
        self.handle
            .inner
            .published
            .host_visibility
            .take()
            .map(|pending| match pending {
                Visibility::Visible => TerminalVisibility::Visible,
                Visibility::Hidden => TerminalVisibility::Hidden,
                Visibility::Hibernated => TerminalVisibility::Hibernated,
            })
    }
}

impl WeakTerminalHandle {
    pub(crate) fn upgrade(&self) -> Option<TerminalHandle> {
        self.inner.upgrade().map(|inner| TerminalHandle { inner })
    }
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
    pub(crate) const SHUTDOWN_TIMEOUT: Duration = WORKER_JOIN_TIMEOUT;
    /// Captured from the authenticated original holder before worker startup;
    /// never rediscovered by a reusable terminal ID.
    pub(crate) fn holder_identity(&self) -> Option<&super::holder::Record> {
        self.inner.published.holder_identity.get()
    }
    /// Run this terminal's ordinary shutdown within `budget` instead of the
    /// product's. Set it before shutdown starts.
    #[cfg(test)]
    pub(super) fn set_shutdown_budget_for_test(&self, budget: ShutdownBudget) {
        *self
            .inner
            .published
            .shutdown_budget
            .lock()
            .unwrap_or_else(|p| p.into_inner()) = budget;
    }
    #[cfg(test)]
    pub(crate) fn original_root_pidfd_for_test(&self) -> std::io::Result<File> {
        self.inner
            .published
            .test_input_root_pidfd
            .get()
            .ok_or_else(|| std::io::Error::other("original root pidfd was not captured"))?
            .try_clone()
    }
    /// Physical provenance of the original PTY launch, not caller authority.
    /// A caller must still revalidate its current Host record/source ownership;
    /// these atomic worker checks do not make a source mutation atomic.
    #[cfg(test)]
    pub(crate) fn pinned_launch_directory(
        &self,
    ) -> Result<(crate::agent_execution::DirectoryIdentity, Arc<File>), String> {
        if self.is_completed()
            || self
                .inner
                .published
                .shutdown_requested
                .load(Ordering::Acquire)
        {
            return Err("terminal worker is no longer active".into());
        }
        let ((identity, file), root) = self
            .inner
            .published
            .launch_directory
            .get()
            .ok_or("this terminal holder cannot attest its original launch directory")?;
        let mut original = libc::pollfd {
            fd: root.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: poll observes the already-held original root, never a PID
        // lookup. A kernel exit must refuse even before Completed is published.
        let ready = unsafe { libc::poll(&mut original, 1, 0) };
        if ready != 0 {
            return Err("terminal original root is no longer live or cannot be observed".into());
        }
        holder::checked_launch_directory(*identity, Arc::clone(file))
    }
    pub(crate) fn transcript_location(&self) -> Option<crate::model::TranscriptLocationReceipt> {
        self.inner
            .published
            .transcript_location
            .get()
            .map(|location| location.receipt())
    }
    pub fn spawn(
        session: TerminalSession,
        config: TerminalConfig,
        wake: impl Fn(TerminalWake) + Send + Sync + 'static,
    ) -> Result<Self, TerminalStartError> {
        Self::spawn_with_execution(session, config, None, wake)
    }

    /// Only validated checkpoint/holder callers can separate the issued
    /// descriptor from the exact recipe the worker must execute and track.
    pub(crate) fn spawn_with_execution(
        session: TerminalSession,
        config: TerminalConfig,
        execution_recipe: Option<LaunchRecipe>,
        wake: impl Fn(TerminalWake) + Send + Sync + 'static,
    ) -> Result<Self, TerminalStartError> {
        Self::spawn_with_execution_installed(session, config, execution_recipe, wake, |_| Ok(()))
    }

    /// HOST installs an independent live custody slot before this method permits
    /// its original thread to prepare or commit adoption. Refusal before that
    /// installation has no adopted owner to unwind.
    pub(crate) fn spawn_with_execution_installed(
        session: TerminalSession,
        config: TerminalConfig,
        execution_recipe: Option<LaunchRecipe>,
        wake: impl Fn(TerminalWake) + Send + Sync + 'static,
        install: impl FnOnce(&TerminalHandle) -> Result<(), String>,
    ) -> Result<Self, TerminalStartError> {
        // Acquire the shared monitor before starting any holder/provider.
        event_notifier().map_err(TerminalStartError::Worker)?;
        super::reactor::runtime().map_err(TerminalStartError::Worker)?;
        let (command_tx, command_rx) = command_channel(config.command_capacity);
        // Semantic events are lossless and independent from visual wakes. They
        // are low-volume metadata; an unbounded queue avoids either dropping a
        // terminal exit/error or deadlocking teardown behind the UI receiver.
        let (event_tx, event_rx) = crossbeam_channel::unbounded();
        let published = Arc::new(PublishedState::new(
            config.initially_visible,
            Arc::new(wake),
        ));
        // Cleanup capacity/service failure refuses before any actual worker,
        // holder or provider exists. A retired screen does not retain admission.
        let join_owner = joins::JoinOwner::new(
            published.clone(),
            joins::reserve().map_err(TerminalStartError::Worker)?,
        );
        let worker_owner = join_owner.clone();
        let worker_published = Arc::clone(&published);
        let (startup_tx, startup_rx) = std::sync::mpsc::sync_channel(1);
        let (begin, admitted) = crossbeam_channel::bounded(1);
        published
            .startup_unresolved
            .store(config.adoption.is_some(), Ordering::Release);
        let handle = Self {
            inner: Arc::new(HandleInner {
                commands: command_tx,
                events: event_rx,
                published: published.clone(),
                worker: join_owner.clone(),
            }),
        };
        if let Err(error) = install(&handle) {
            published.startup_unresolved.store(false, Ordering::Release);
            return Err(TerminalStartError::Worker(error));
        }

        let worker = thread::Builder::new()
            .name(format!("terminal-{:?}", session.id))
            .spawn(move || {
                let _body = joins::BodyCompletion(worker_owner);
                let completion = Arc::clone(&worker_published);
                if admitted.recv().is_err() {
                    completion
                        .startup_unresolved
                        .store(false, Ordering::Release);
                    completion.completed.store(true, Ordering::Release);
                    return;
                }
                let adopting = config.adoption.is_some();
                // Preparation has not sent CommitAdoption. A panic here can
                // only release a provisional control whose holder still owns
                // its replay backup; it never unwinds a committed local owner.
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    Worker::prepare(session, config, execution_recipe, command_rx, event_tx, worker_published)
                })).unwrap_or_else(|_| Err("Terminal preparation panicked before adoption commit".into()));
                match result {
                    Ok(mut worker) => {
                        // Crucially the COMPLETE Worker is outside the unwind
                        // closure. Even a commit/activation panic cannot drop
                        // the original raw queue, reader, master or control.
                        let retained = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            worker.commit_start()
                        })).unwrap_or_else(|_| Some("Adoption commit/activation panicked; complete original Worker retained".into()));
                        if !adopting && let Some(message) = &retained {
                            completion.startup_unresolved.store(false, Ordering::Release);
                            completion.completed.store(true, Ordering::Release);
                            let _ = startup_tx.send(Err(message.clone()));
                            return;
                        }

                        if let Some(message) = retained {
                            *completion
                                .startup_diagnostic
                                .lock()
                                .unwrap_or_else(|p| p.into_inner()) = Some(message.clone());
                            // A notification callback must not unwind the
                            // complete owner after the holder discarded replay.
                            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                worker.error(
                                    "adoption commit unconfirmed; original output retained",
                                    message.clone(),
                                );
                            }));
                            let _ = startup_tx.send(Ok(Some(message)));
                            worker.run_retained_startup();
                        } else {
                            completion
                                .startup_unresolved
                                .store(false, Ordering::Release);
                            let _ = startup_tx.send(Ok(None));
                            worker.run();
                        }
                    }
                    Err(error) => {
                        completion
                            .startup_unresolved
                            .store(false, Ordering::Release);
                        let _ = startup_tx.send(Err(error));
                        completion.completed.store(true, Ordering::Release);
                    }
                }
            })
            .map_err(|error| {
                published.startup_unresolved.store(false, Ordering::Release);
                TerminalStartError::Worker(error.to_string())
            })?;
        join_owner.install(worker);
        let _ = begin.send(());

        match startup_rx.recv() {
            Ok(Ok(None)) => Ok(handle),
            Ok(Ok(Some(message))) => Err(TerminalStartError::CustodyRetained { handle, message }),
            Ok(Err(error)) => {
                joins::request(&join_owner, true, true);
                let _ = joins::wait_until(
                    &join_owner,
                    Some(std::time::Instant::now() + WORKER_JOIN_TIMEOUT),
                    true,
                );
                Err(TerminalStartError::Worker(error))
            }
            Err(_) if published.startup_unresolved.load(Ordering::Acquire) => {
                Err(TerminalStartError::CustodyRetained {
                    handle,
                    message: "The original startup thread did not report its adoption outcome"
                        .into(),
                })
            }
            Err(_) => {
                joins::request(&join_owner, true, true);
                let _ = joins::wait_until(
                    &join_owner,
                    Some(std::time::Instant::now() + WORKER_JOIN_TIMEOUT),
                    true,
                );
                Err(TerminalStartError::WorkerExited)
            }
        }
    }

    /// A viewport read needing no admission.
    #[cfg(test)]
    pub(crate) fn viewport_snapshot(
        &self,
        offset: Option<u64>,
        rows: Option<u16>,
        shown: bool,
    ) -> Result<Arc<RenderSnapshot>, String> {
        self.viewport_snapshot_admitted(offset, rows, shown, || ())
    }

    /// `shown` says the host has asked this worker to show the terminal.
    /// Until the worker has published a screen since, what it published is a
    /// hibernation marker, nothing, or the screen from before it was hidden.
    /// Such a read is answered on the worker, after the reveal queued ahead
    /// of it. What `admit` returns is held only while the read is queued to
    /// the worker, not while it waits for the frame.
    pub(crate) fn viewport_snapshot_admitted<A>(
        &self,
        offset: Option<u64>,
        rows: Option<u16>,
        shown: bool,
        admit: impl FnOnce() -> A,
    ) -> Result<Arc<RenderSnapshot>, String> {
        let published = self.snapshot();
        let drawn = !published.detached && published.rows > 0;
        if !shown && !drawn {
            return Ok(published);
        }
        // A reveal turns `visible` on before its capture is published, and a
        // synchronized update can hold that capture back; until then the
        // published screen is the one from before the terminal was hidden.
        let current = !shown || self.inner.published.presents(&published);
        let default_view = drawn
            && offset.is_none()
            && rows.is_none_or(|rows| rows == published.rows)
            && published.scroll.offset == published.scroll.max_offset();
        if current && default_view {
            return Ok(published);
        }
        let (reply, receive) = crossbeam_channel::bounded(1);
        let admission = admit();
        let queued = self.try_command(Command::ReadViewport {
            offset,
            rows,
            reply,
        });
        drop(admission);
        match queued {
            Ok(()) => {}
            // A closed worker shows nothing new: what it left is its screen.
            Err(TerminalSendError::Closed) if !drawn || default_view => {
                return Ok(self.snapshot());
            }
            Err(error) => return Err(error.to_string()),
        }
        match receive.recv_timeout(Duration::from_secs(3)) {
            Ok(result) => result,
            // A completed terminal whose history could not be kept reports
            // that as an event and keeps what it published. Only its archive
            // drops a read unanswered; anything else that does is a failure.
            Err(RecvTimeoutError::Disconnected)
                if !drawn || (default_view && self.inner.published.frozen()) =>
            {
                Ok(self.snapshot())
            }
            Err(error) => Err(error.to_string()),
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
                process: Box::new(process),
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

    #[cfg(test)]
    pub(crate) fn spawn_with_wedged_worker_for_test() -> (Self, Sender<()>) {
        Self::spawn_with_blocked_worker_for_test(false)
    }

    #[cfg(test)]
    pub(crate) fn spawn_with_panicking_worker_for_test() -> (Self, Sender<()>) {
        Self::spawn_with_blocked_worker_for_test(true)
    }

    #[cfg(test)]
    pub(crate) fn spawn_with_blocked_tls_for_test() -> (Self, Sender<()>, Receiver<()>) {
        struct TlsWait {
            blocked: Receiver<()>,
            entered: Sender<()>,
        }
        impl Drop for TlsWait {
            fn drop(&mut self) {
                let _ = self.entered.send(());
                let _ = self.blocked.recv();
            }
        }
        std::thread_local! {
            static BLOCK: RefCell<Option<TlsWait>> = const { RefCell::new(None) };
        }
        let published = Arc::new(PublishedState::new(true, Arc::new(|_| {})));
        let owner = joins::JoinOwner::new(
            published.clone(),
            joins::reserve().expect("test TLS cleanup admission"),
        );
        let worker_owner = owner.clone();
        let worker_published = published.clone();
        let (commands, inbox) = command_channel(1);
        let (events, event_receiver) = crossbeam_channel::unbounded();
        let (release, blocked) = crossbeam_channel::bounded(1);
        let (entered, observed) = crossbeam_channel::bounded(1);
        let worker = thread::spawn(move || {
            let _body = joins::BodyCompletion(worker_owner);
            let _inbox = inbox;
            let _events = events;
            BLOCK.with(|slot| *slot.borrow_mut() = Some(TlsWait { blocked, entered }));
            // This control models a completed process/body publication, not a
            // real PTY. The OS thread is still alive in its actual TLS destructor.
            worker_published.completed.store(true, Ordering::Release);
        });
        owner.install(worker);
        (
            Self {
                inner: Arc::new(HandleInner {
                    commands,
                    events: event_receiver,
                    published,
                    worker: owner,
                }),
            },
            release,
            observed,
        )
    }

    #[cfg(test)]
    fn spawn_with_blocked_worker_for_test(panic_after_release: bool) -> (Self, Sender<()>) {
        let published = Arc::new(PublishedState::new(true, Arc::new(|_| {})));
        let owner = joins::JoinOwner::new(
            published.clone(),
            joins::reserve().expect("test worker cleanup admission"),
        );
        let worker_owner = owner.clone();
        let (commands, inbox) = command_channel(1);
        let (events, event_receiver) = crossbeam_channel::unbounded();
        let (release, blocked) = crossbeam_channel::bounded(1);
        let worker = thread::spawn(move || {
            let _body = joins::BodyCompletion(worker_owner);
            let _inbox = inbox;
            let _events = events;
            let _ = blocked.recv();
            assert!(!panic_after_release, "real terminal-worker panic fixture");
        });
        owner.install(worker);
        (
            Self {
                inner: Arc::new(HandleInner {
                    commands,
                    events: event_receiver,
                    published,
                    worker: owner,
                }),
            },
            release,
        )
    }

    #[cfg(test)]
    pub(crate) fn worker_joined_for_test(&self) -> bool {
        self.inner.worker.joined()
    }

    #[cfg(test)]
    pub(crate) fn reserved_worker_joins_for_test() -> usize {
        joins::reserved()
    }

    #[cfg(test)]
    pub(crate) fn worker_join_capacity_for_test() -> usize {
        joins::CAPACITY
    }

    #[cfg(test)]
    pub(crate) fn spawn_with_missing_exit_report_for_test(session: TerminalSession) -> Self {
        let (commands, receiver) = command_channel(1);
        let (events, event_receiver) = crossbeam_channel::unbounded();
        let published = Arc::new(PublishedState::new(true, Arc::new(|_| {})));
        let join_owner = joins::JoinOwner::new(
            published.clone(),
            joins::reserve().expect("test worker cleanup admission"),
        );
        let worker_owner = join_owner.clone();
        let worker_published = Arc::clone(&published);
        let (ready, initialized) = crossbeam_channel::bounded(1);
        let worker = thread::spawn(move || {
            let _body = joins::BodyCompletion(worker_owner);
            let mut worker = Worker::start(
                session,
                TerminalConfig::default(),
                None,
                receiver,
                events,
                worker_published,
            )
            .unwrap()
            .0;
            assert!(
                !worker.holder.registered(),
                "failure fixture must not write holder recovery into user state"
            );
            worker.disconnected_exit_report();
            // A timed-out caller sets shutdown; still run cleanup if its
            // readiness receiver disappeared instead of panicking first.
            let _ = ready.send(());
            worker.run();
        });
        join_owner.install(worker);
        if let Err(error) = initialized.recv_timeout(Duration::from_secs(5)) {
            published.request_shutdown();
            joins::request(&join_owner, true, true);
            panic!("test terminal initialization did not finish: {error}");
        }
        Self {
            inner: Arc::new(HandleInner {
                commands,
                events: event_receiver,
                published,
                worker: join_owner,
            }),
        }
    }

    /// True after teardown and confirmed process cleanup or committed carry.
    /// Published before the final `Completed` event when an exit is confirmed.
    /// A stopped parser or foreground-process exit alone releases no ownership.
    pub fn is_completed(&self) -> bool {
        self.inner.published.completed.load(Ordering::Acquire)
    }
    pub(crate) fn same_worker(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }

    pub(crate) fn execution_instance(&self) -> uuid::Uuid {
        self.inner.published.execution_instance
    }

    pub(crate) fn begin_completed_retirement(&self) -> Result<(), &'static str> {
        if !self.is_completed() {
            return Err("The original terminal has not completed");
        }
        self.request_shutdown();
        // The already-joined fast path need not enqueue another lane task.
        crate::daemon::wake();
        Ok(())
    }

    pub(crate) fn completed_retirement(
        &self,
        source: &TerminalSession,
        proof: &crate::model::HostSessionProof,
    ) -> TerminalRetirement {
        match self.inner.worker.retirement_ready() {
            Err(error) => return TerminalRetirement::Retained(error.into()),
            Ok(false) => return TerminalRetirement::Pending,
            Ok(true) => {}
        }
        if !self.is_completed() {
            return TerminalRetirement::Retained(
                "Original terminal cleanup remains unconfirmed".into(),
            );
        }
        let Some(original) = self.inner.published.original_holder_process.get() else {
            return TerminalRetirement::Retained(
                "Original holder process evidence is unavailable".into(),
            );
        };
        let mut event = libc::pollfd {
            fd: original.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // The held pidfd cannot observe a reused numeric PID. Keep it alive
        // through the daemon's wait when an adopted or fsyncing holder remains.
        match unsafe { libc::poll(&mut event, 1, 0) } {
            0 => TerminalRetirement::WaitingForHolder(Arc::clone(original)),
            n if n < 0
                && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted =>
            {
                TerminalRetirement::WaitingForHolder(Arc::clone(original))
            }
            n if n > 0
                && event.revents & libc::POLLIN != 0
                && event.revents & (libc::POLLNVAL | libc::POLLERR) == 0 =>
            {
                if let Some(record) = self.holder_identity() {
                    match super::holder::cleanup::confirmed_with_role(
                        source,
                        record,
                        proof,
                        self.inner.published.original_holder_role.get(),
                    ) {
                        Ok(true) => TerminalRetirement::Released,
                        Ok(false) => TerminalRetirement::Retained(
                            "The exact original holder cleanup receipt is unavailable".into(),
                        ),
                        Err(error) => TerminalRetirement::Retained(error),
                    }
                } else {
                    TerminalRetirement::Released
                }
            }
            _ => TerminalRetirement::Retained("Original holder exit could not be observed".into()),
        }
    }

    pub(crate) fn downgrade(&self) -> WeakTerminalHandle {
        WeakTerminalHandle {
            inner: Arc::downgrade(&self.inner),
        }
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

    /// Transfers this worker's semantic stream to a shared off-thread observer.
    /// Install once the Host record is published. Startup events are replayed
    /// before subsequent events, including an already-completed worker's tail.
    pub(crate) fn set_event_observer(
        &self,
        observer: impl Fn(&TerminalHandle, TerminalEvent) + Send + Sync + 'static,
    ) {
        let handle = Arc::downgrade(&self.inner);
        let observer: Arc<ArchiveObserver> = Arc::new(move |event| {
            if let Some(inner) = handle.upgrade() {
                observer(&TerminalHandle { inner }, event);
            }
        });
        let mut current = self
            .inner
            .published
            .event_observer
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        for event in self.inner.events.try_iter() {
            let observer = Arc::clone(&observer);
            notify_archived(move || observer(event));
        }
        *current = Some(observer);
    }

    pub fn input(&self, bytes: impl Into<Vec<u8>>) -> Result<(), TerminalSendError> {
        let bytes = bytes.into();
        if bytes == [3] {
            return self.try_command(Command::Interrupt(None));
        }
        self.try_command(Command::Input(bytes))
    }

    pub fn paste(
        &self,
        text: impl Into<String>,
        origin: PasteOrigin,
    ) -> Result<(), TerminalSendError> {
        self.try_command(Command::Paste(text.into(), origin))
    }

    pub fn key(&self, mut key: KeyInput) -> Result<(), TerminalSendError> {
        // Associated text belongs to a down/repeat, never to physical cleanup.
        // Normalize before reserving or publishing the release admission.
        if key.action == KeyAction::Release {
            key.text = None;
        }
        if key.action != KeyAction::Release && is_interrupt_key(&key) {
            return self.try_command(Command::Interrupt(Some(key)));
        }
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
    pub fn clear_scrollback(&self) -> Result<(), TerminalSendError> {
        self.try_command(Command::ClearScrollback)
    }
    pub fn copy_selection(
        &self,
        request: uuid::Uuid,
        selection: super::SearchMatch,
    ) -> Result<(), TerminalSendError> {
        self.try_command(Command::CopySelection { request, selection })
    }

    pub fn search(
        &self,
        request: u64,
        query: String,
        case_sensitive: bool,
    ) -> Result<(), TerminalSendError> {
        self.search_for(None, request, query, case_sensitive)
    }

    pub(crate) fn search_for(
        &self,
        owner: Option<u64>,
        request: u64,
        query: String,
        case_sensitive: bool,
    ) -> Result<(), TerminalSendError> {
        let latest = self.inner.published.prepare_search(owner, request);
        self.try_command(Command::Search {
            owner,
            request,
            query,
            case_sensitive,
            latest,
        })
    }

    #[cfg(test)]
    pub(crate) fn set_archived_event_observer(
        &self,
        observer: impl Fn(&TerminalHandle, TerminalEvent) + Send + Sync + 'static,
    ) {
        let handle = Arc::downgrade(&self.inner);
        let _ = self
            .inner
            .published
            .archive_observer
            .set(Arc::new(move |event| {
                if let Some(inner) = handle.upgrade() {
                    observer(&TerminalHandle { inner }, event);
                }
            }));
    }

    /// Asks the worker to show, hide or hibernate its terminal for the host.
    ///
    /// This cannot be refused. Only the newest request matters, so it
    /// replaces any the worker has not applied yet, and the worker applies it
    /// before the next event it handles, including a command queued after
    /// this call. A worker that has stopped has nothing left to present.
    pub(crate) fn set_host_visibility(&self, visibility: super::ipc::TerminalVisibility) {
        let visibility = match visibility {
            super::ipc::TerminalVisibility::Visible => Visibility::Visible,
            super::ipc::TerminalVisibility::Hidden => Visibility::Hidden,
            super::ipc::TerminalVisibility::Hibernated => Visibility::Hibernated,
        };
        let published = &self.inner.published;
        // Subscription is observational; it must not activate or parse the
        // parked reader of an unconfirmed original merely to list, inspect or
        // close its owner.
        if self.startup_unresolved() {
            published
                .visible
                .store(visibility == Visibility::Visible, Ordering::Release);
            return;
        }
        if published.shutdown_requested.load(Ordering::Acquire) {
            return;
        }
        // The worker enters archive preparation under this lock and takes
        // any unapplied request with it, so a request is either taken there
        // or applied to the archive here, never lost in between.
        let phase = published.archive.lock().unwrap_or_else(|p| p.into_inner());
        match &*phase {
            ArchivePhase::Live => {
                published.host_visibility.set(visibility);
                // The capture wake also ends the worker's wait; a full slot
                // means a wake is already pending.
                let _ = published.capture_tx.try_send(());
            }
            // Visibility belongs to the retained surface, which needs neither
            // the parser nor history.
            ArchivePhase::Preparing | ArchivePhase::Frozen { .. } => published
                .visible
                .store(visibility == Visibility::Visible, Ordering::Release),
            ArchivePhase::Released => {}
        }
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

    /// A reversible cut for an already admitted daemon upgrade. The caller
    /// owns this unique token until carry or emergency preservation settles.
    pub(crate) fn prepare_input_handoff(
        &self,
        token: Arc<AtomicBool>,
        resumed: Sender<Result<(), String>>,
    ) -> Result<(), String> {
        if self.is_completed() {
            let _ = resumed.send(Ok(()));
            return Ok(());
        }
        {
            let mut order = self
                .inner
                .published
                .input_order
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            if order.handoff {
                let _ = resumed.send(Ok(()));
                return Err("Terminal ownership is already changing".into());
            }
            order.handoff = true;
            order.cleanup_sealed = true;
            order.prepared_input = Some(token.clone());
        }
        let (reply, result) = crossbeam_channel::bounded(1);
        if let Err(error) = self.send_command(Command::PrepareInputHandoff {
            token: token.clone(),
            resumed: resumed.clone(),
            reply,
        }) {
            // This command was not accepted; no worker owns the cut.
            let mut order = self
                .inner
                .published
                .input_order
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            if order
                .prepared_input
                .as_ref()
                .is_some_and(|current| Arc::ptr_eq(current, &token))
            {
                order.prepared_input = None;
                order.handoff = false;
                order.cleanup_sealed = false;
            }
            token.store(false, Ordering::Release);
            let _ = resumed.send(Ok(()));
            return Err(error.to_string());
        }
        let answer = result
            .recv_timeout(Duration::from_secs(3))
            .map_err(|error| format!("Terminal input handoff remains unconfirmed: {error}"));
        if !matches!(answer, Ok(Ok(()))) {
            self.cancel_input_handoff(&token);
        }
        answer?
    }

    pub(crate) fn cancel_input_handoff(&self, token: &Arc<AtomicBool>) {
        token.store(false, Ordering::Release);
        // Out-of-band cancellation cannot be starved by a full command queue.
        // The matching worker alone confirms restart before reopening input.
        let _ = self.inner.published.capture_tx.try_send(());
    }

    pub(crate) fn prepare_upgrade(&self) -> Result<UpgradeSnapshot, String> {
        let (sender, receiver) = crossbeam_channel::bounded(1);
        let prepared = self
            .inner
            .published
            .input_order
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .prepared_input
            .as_ref()
            .is_some_and(|token| token.load(Ordering::Acquire));
        // The committed non-continuity fallback may inspect/relaunch an
        // already-cut terminal. Keep its reader/input fenced; ordinary new
        // controls still cannot cross that ownership boundary.
        if prepared {
            self.send_command(Command::PrepareUpgrade(sender))
        } else {
            self.try_command(Command::PrepareUpgrade(sender))
        }
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
    /// This waits for the worker's answer however long it takes: a caller that stopped
    /// waiting could not tell a terminal left here from one already carried.
    pub(crate) fn carry(&self, socket: PathBuf) -> Result<Carried, String> {
        {
            let mut order = self
                .inner
                .published
                .input_order
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            if order.handoff
                && !order
                    .prepared_input
                    .as_ref()
                    .is_some_and(|token| token.load(Ordering::Acquire))
            {
                return Err("Terminal ownership is already changing".into());
            }
            order.handoff = true;
            order.cleanup_sealed = true;
        }
        let reopen = || {
            let mut order = self
                .inner
                .published
                .input_order
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            if order.prepared_input.is_none() {
                order.handoff = false;
                order.cleanup_sealed = false;
            }
        };
        let (reply, result) = crossbeam_channel::bounded(1);
        if let Err(error) = self.send_command(Command::Carry { socket, reply }) {
            reopen();
            return Err(error.to_string());
        }
        let carried = match result.recv() {
            Ok(Ok(carried)) => carried,
            Ok(Err(error)) => return Err(error),
            Err(_) => {
                return Err("the terminal ended before its carry outcome was confirmed".into());
            }
        };
        joins::request(&self.inner.worker, false, false);
        if !joins::wait_until(&self.inner.worker, None, false) {
            return Err("Terminal worker failed while completing its carry".into());
        }
        Ok(carried)
    }

    /// Inspection and the close decision are one serialized PTY-owner command.
    pub(crate) fn close_checked(&self, allow_active: bool) -> super::ipc::TerminalCloseResult {
        use super::ipc::TerminalCloseResult;
        if self.is_completed() {
            return match self.shutdown() {
                Ok(()) => TerminalCloseResult::Closed,
                Err(error) => TerminalCloseResult::Unknown(error.to_string()),
            };
        }
        let (reply, result) = crossbeam_channel::bounded(1);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        if let Err(error) = self.try_command(Command::CloseChecked {
            allow_active,
            deadline,
            reply,
        }) {
            if error == TerminalSendError::Closed
                && matches!(
                    *self
                        .inner
                        .published
                        .archive
                        .lock()
                        .unwrap_or_else(|p| p.into_inner()),
                    ArchivePhase::Frozen { .. }
                )
            {
                return match self.shutdown() {
                    Ok(()) => TerminalCloseResult::Closed,
                    Err(error) => TerminalCloseResult::Unknown(error.to_string()),
                };
            }
            return TerminalCloseResult::Unknown(error.to_string());
        }
        let outcome = result
            .recv_timeout(std::time::Duration::from_secs(3))
            .unwrap_or_else(|error| TerminalCloseResult::Unknown(error.to_string()));
        if matches!(outcome, TerminalCloseResult::Unknown(_)) {
            // An adopted root can already be reaped when preparation tries to
            // capture its live process identity. A checked close may then run
            // before the queued exit report and PTY EOF finish publication.
            // Give that ordinary completion the remainder of the close budget;
            // neither the failed inspection nor elapsed time proves release.
            let published = &self.inner.published;
            let mut archive = published.archive.lock().unwrap_or_else(|p| p.into_inner());
            while matches!(*archive, ArchivePhase::Live | ArchivePhase::Preparing) {
                let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                if remaining.is_zero() {
                    break;
                }
                let (next, _) = published
                    .archive_ready
                    .wait_timeout(archive, remaining)
                    .unwrap_or_else(|p| p.into_inner());
                archive = next;
            }
            let frozen = matches!(*archive, ArchivePhase::Frozen { .. });
            drop(archive);
            if frozen {
                return match self.shutdown() {
                    Ok(()) => TerminalCloseResult::Closed,
                    Err(error) => TerminalCloseResult::Unknown(error.to_string()),
                };
            }
        }
        if outcome == TerminalCloseResult::Closed {
            // Completion is not reported until the owner has released its PTY.
            self.request_shutdown();
            if !joins::wait_until(
                &self.inner.worker,
                Some(std::time::Instant::now() + self.inner.published.shutdown_budget().join),
                true,
            ) {
                return TerminalCloseResult::Unknown(
                    "Terminal worker join remains unconfirmed; its original owner is retained"
                        .into(),
                );
            }
            if !self.is_completed() {
                return TerminalCloseResult::Unknown(
                    "Terminal process cleanup remains unconfirmed".into(),
                );
            }
        }
        outcome
    }

    /// Starts shutdown before joining any workers, so a daemon with many
    /// terminals lets all roots handle their hangups concurrently.
    pub(crate) fn request_shutdown(&self) {
        if self.startup_unresolved() {
            return;
        }
        self.inner.published.request_shutdown();
        joins::request(&self.inner.worker, true, false);
    }

    pub fn shutdown(&self) -> Result<(), TerminalSendError> {
        self.shutdown_until(std::time::Instant::now() + self.inner.published.shutdown_budget().join)
    }

    pub(crate) fn shutdown_until(
        &self,
        deadline: std::time::Instant,
    ) -> Result<(), TerminalSendError> {
        if self.startup_unresolved() {
            return Err(TerminalSendError::Closed);
        }
        self.request_shutdown();
        if !joins::wait_until(&self.inner.worker, Some(deadline), true) {
            return Err(TerminalSendError::Closed);
        }
        if self.is_completed() {
            Ok(())
        } else {
            Err(TerminalSendError::Closed)
        }
    }

    /// The caller first fences the exact source against this retained worker.
    /// A holder receipt, not an absent worker, proves the paired cleanup after
    /// a daemon restart or lost reply.
    pub(crate) fn close_with_receipt(
        &self,
        source: &TerminalSession,
        proof: &crate::model::HostSessionProof,
    ) -> Result<(), String> {
        super::holder::cleanup::authorize(proof)?;
        let expected = self
            .holder_identity()
            .ok_or("The paired terminal has no authenticated holder identity")?;
        match self.close_checked(true) {
            super::ipc::TerminalCloseResult::Closed => {}
            other => return Err(format!("Paired terminal close is unconfirmed: {other:?}")),
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            if super::holder::cleanup::confirmed_with_role(
                source,
                expected,
                proof,
                self.inner.published.original_holder_role.get(),
            )? {
                return Ok(());
            }
            if std::time::Instant::now() >= deadline {
                return Err("The exact terminal holder has not proved its release".into());
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
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
        joins::settled(&self.inner.worker);
    }

    /// Failed checkpoint restoration must never release an adopted registered
    /// holder. Serialize with completion, then transfer any final retained
    /// screen; a live parser that cannot serialize is honestly screen-lost.
    pub(crate) fn preserve_for_restart(&self) -> Result<(), String> {
        if !self.is_completed() {
            {
                let mut order = self
                    .inner
                    .published
                    .input_order
                    .lock()
                    .unwrap_or_else(|p| p.into_inner());
                if order.handoff
                    && !order
                        .prepared_input
                        .as_ref()
                        .is_some_and(|token| token.load(Ordering::Acquire))
                {
                    return Err("Terminal ownership is already changing".into());
                }
                order.handoff = true;
                order.cleanup_sealed = true;
            }
            let reopen = || {
                let mut order = self
                    .inner
                    .published
                    .input_order
                    .lock()
                    .unwrap_or_else(|p| p.into_inner());
                if order.prepared_input.is_none() {
                    order.handoff = false;
                    order.cleanup_sealed = false;
                }
            };
            let (reply, result) = crossbeam_channel::bounded(1);
            if let Err(error) = self.send_command(Command::PreserveForRestart(reply)) {
                reopen();
                return Err(error.to_string());
            }
            match result.recv() {
                Ok(Ok(())) => {}
                Ok(Err(error)) => return Err(error),
                Err(_) if self.is_completed() => {}
                Err(_) => return Err("terminal preservation outcome remains unconfirmed".into()),
            }
        }
        joins::request(&self.inner.worker, false, false);
        if !joins::wait_until(&self.inner.worker, None, false) {
            return Err("terminal worker failed while preserving its holder".into());
        }
        self.preserve_completed_for_restart();
        Ok(())
    }

    pub(crate) fn startup_unresolved(&self) -> bool {
        self.inner
            .published
            .startup_unresolved
            .load(Ordering::Acquire)
    }

    #[cfg(test)]
    pub(crate) fn fail_startup_commit_after_ack_for_test(&self) {
        self.inner
            .published
            .test_fail_startup_commit
            .store(true, Ordering::Release);
    }

    #[cfg(test)]
    pub(crate) fn retained_startup_bytes_for_test(&self) -> Vec<u8> {
        self.retained_startup_state_for_test().0
    }

    #[cfg(test)]
    pub(crate) fn retained_startup_state_for_test(&self) -> (Vec<u8>, bool, bool) {
        assert!(self.startup_unresolved());
        let (reply, result) = crossbeam_channel::bounded(1);
        *self.inner.published.test_startup_probe.lock().unwrap() = Some(reply);
        let _ = self.inner.published.capture_tx.try_send(());
        result
            .recv_timeout(Duration::from_secs(5))
            .expect("original retained worker probe")
    }

    #[cfg(test)]
    pub(crate) fn startup_diagnostic(&self) -> Option<String> {
        self.inner
            .published
            .startup_diagnostic
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    fn try_command(&self, command: Command) -> Result<(), TerminalSendError> {
        if self.startup_unresolved() && !matches!(command, Command::CloseChecked { .. }) {
            return Err(TerminalSendError::Closed);
        }
        // Stop closes admission even when an existing cached reader holds the
        // archive lock. Natural completion alone leaves read-only commands live.
        if self
            .inner
            .published
            .shutdown_requested
            .load(Ordering::Acquire)
        {
            return Err(TerminalSendError::Closed);
        }

        // Enqueue and archive preparation share this lock. No accepted query
        // can land between the final drain and worker teardown.
        let published = &self.inner.published;
        let mut phase = published.archive.lock().unwrap_or_else(|p| p.into_inner());
        // Fast terminal controls share a bounded dispatcher. Waiting for a
        // different terminal's final screen would occupy its input capacity;
        // refuse admission so the caller can retry after archival completes.
        if matches!(*phase, ArchivePhase::Preparing)
            && matches!(
                command,
                Command::Interrupt(_)
                    | Command::Input(_)
                    | Command::Paste(..)
                    | Command::Key(_)
                    | Command::Mouse(_)
                    | Command::Focus(_)
                    | Command::Resize(_)
                    | Command::Scroll(_)
                    | Command::SetTheme(_)
                    | Command::Hide
                    | Command::Reveal
                    | Command::Hibernate
                    | Command::Restore { .. }
            )
        {
            return Err(TerminalSendError::Full);
        }
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while matches!(*phase, ArchivePhase::Preparing) {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                return Err(TerminalSendError::Closed);
            }
            let (next, timeout) = published
                .archive_ready
                .wait_timeout(phase, remaining)
                .unwrap_or_else(|p| p.into_inner());
            phase = next;
            if timeout.timed_out() && matches!(*phase, ArchivePhase::Preparing) {
                return Err(TerminalSendError::Closed);
            }
        }
        if matches!(*phase, ArchivePhase::Frozen { .. }) {
            drop(phase);
            return published.archived_command(command);
        }
        if matches!(*phase, ArchivePhase::Released) {
            return Err(TerminalSendError::Closed);
        }
        if matches!(
            command,
            Command::Interrupt(_) | Command::CloseChecked { .. }
        ) && published.completed.load(Ordering::Acquire)
        {
            return Err(TerminalSendError::Closed);
        }
        if let Command::Interrupt(key) = command {
            return if published.request_interrupt(key) {
                Ok(())
            } else {
                Err(TerminalSendError::Closed)
            };
        }
        // Serialize input admission with the priority cancellation boundary.
        // Commands submitted after Ctrl+C must survive its queue drain.
        let mut order = self
            .inner
            .published
            .input_order
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        if published.shutdown_requested.load(Ordering::Acquire) {
            return Err(TerminalSendError::Closed);
        }
        if order.prepared_input.is_some() {
            // A stable upgrade cut has no cleanup/retry escape hatch: it was
            // admitted only after all held input settled. The exact-token
            // rollback must restore the reader before any new command enters.
            return Err(TerminalSendError::Closed);
        }
        if let Some((identity, KeyAction::Release)) = command_pair(&command) {
            if order.cleanup_sealed {
                return Err(TerminalSendError::Closed);
            }
            if let Some(claim) = published.cleanup_pool.find(identity) {
                if claim.enqueue_once() {
                    let mut releases = published
                        .cleanup_commands
                        .lock()
                        .unwrap_or_else(|p| p.into_inner());
                    assert!(
                        releases.len() < admission::CLEANUP_LIMIT,
                        "each admitted press reserves its one cleanup slot"
                    );
                    releases.push_back((claim, order.next_sequence(), command));
                    let _ = published.capture_tx.try_send(());
                }
                return Ok(());
            }
        }
        if order.handoff && !order.cleanup_sealed {
            return Err(TerminalSendError::Closed);
        }
        if matches!(command, Command::CloseChecked { .. }) {
            let mut priority = published
                .priority_close
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            if priority.is_some() {
                return Err(TerminalSendError::Full);
            }
            *priority = Some(command);
            let _ = published.capture_tx.try_send(());
            return Ok(());
        }
        if order.handoff {
            return Err(TerminalSendError::Closed);
        }
        let _input = matches!(
            command,
            Command::Input(_) | Command::Paste(..) | Command::Key(_) | Command::Mouse(_)
        );
        let pair = command_pair(&command);
        let input_claim = match pair {
            Some((identity, KeyAction::Press)) => Some(
                published
                    .cleanup_pool
                    .press(identity)
                    .ok_or(TerminalSendError::Full)?,
            ),
            Some((identity, KeyAction::Repeat)) => {
                if let Some(claim) = published.cleanup_pool.find(identity) {
                    // An already accepted up still owns the end of this cycle.
                    let ticket = claim.queue_press().ok_or(TerminalSendError::Full)?;
                    Some((claim, Some(ticket)))
                } else {
                    // Cancellation may have discarded the original wholly
                    // unsent down. A later physical repeat can establish a
                    // fresh pair, with the same pre-ACK cleanup reservation.
                    Some(
                        published
                            .cleanup_pool
                            .press(identity)
                            .ok_or(TerminalSendError::Full)?,
                    )
                }
            }
            _ => None,
        };
        let command = if matches!(
            command,
            Command::Input(_) | Command::Paste(..) | Command::Key(_) | Command::Mouse(_)
        ) {
            Command::OrderedInput {
                generation: order.generation,
                admission: Some(order.track_input()),
                input: Box::new(command),
            }
        } else {
            command
        };
        let command = if let Some((claim, ticket)) = input_claim {
            Command::InputClaim {
                claim,
                ticket,
                command: Box::new(command),
            }
        } else {
            command
        };
        let storage = command_storage_bound(&command)
            .map(|bound| {
                let bytes = bound.ok_or(TerminalSendError::Full)?;
                published
                    .input_storage
                    .reserve(bytes, 1)
                    .ok_or(TerminalSendError::Full)
            })
            .transpose()?;
        let mut permit = self
            .command_budget()
            .try_acquire()
            .ok_or(TerminalSendError::Full)?;
        if let Some(storage) = storage {
            admission::Permit::attach_storage(&mut permit, storage);
        }
        self.inner
            .commands
            .try_send(Command::Admitted {
                permit,
                command: Box::new(command),
            })
            .map_err(|error| match error {
                TrySendError::Full(_) => TerminalSendError::Full,
                TrySendError::Disconnected(_) => TerminalSendError::Closed,
            })?;
        Ok(())
    }

    fn command_budget(&self) -> &Arc<admission::Budget> {
        self.inner.published.command_budget.get_or_init(|| {
            admission::Budget::new(
                self.inner
                    .commands
                    .capacity()
                    .expect("terminal command channel is bounded"),
            )
        })
    }

    fn send_command(&self, command: Command) -> Result<(), TerminalSendError> {
        let deadline = std::time::Instant::now() + WORKER_JOIN_TIMEOUT;
        let permit = self
            .command_budget()
            .acquire_until(deadline)
            .ok_or(TerminalSendError::Full)?;
        self.inner
            .commands
            .send_timeout(
                Command::Admitted {
                    permit,
                    command: Box::new(command),
                },
                deadline.saturating_duration_since(std::time::Instant::now()),
            )
            .map_err(|error| match error {
                crossbeam_channel::SendTimeoutError::Timeout(_) => TerminalSendError::Full,
                crossbeam_channel::SendTimeoutError::Disconnected(_) => TerminalSendError::Closed,
            })
    }
}

fn is_interrupt_key(key: &KeyInput) -> bool {
    key.modifiers.control
        && !key.modifiers.alt
        && !key.modifiers.super_key
        && matches!(key.key, TerminalKey::Character('c' | 'C'))
        // Compound text is ordinary input, not the fixed-size priority key.
        && key.text.as_ref().is_none_or(|text| text.chars().nth(1).is_none())
}

fn command_storage_bound(command: &Command) -> Option<Option<usize>> {
    match command {
        Command::Admitted { command, .. } | Command::InputClaim { command, .. } => {
            command_storage_bound(command)
        }
        Command::OrderedInput { input, .. } => command_storage_bound(input),
        Command::Input(bytes) => Some(Some(bytes.len())),
        Command::Paste(text, _) => Some(text.len().checked_add(BRACKETED_PASTE_FRAME_BYTES)),
        Command::Key(input) => Some(key_storage_bound(input)),
        Command::Mouse(_) | Command::Focus(_) | Command::SetTheme(_) => Some(Some(128)),
        _ => None,
    }
}

fn key_storage_bound(input: &KeyInput) -> Option<usize> {
    input.text.as_ref().map_or(Some(128), |text| {
        text.len()
            .checked_mul(8)
            .and_then(|length| length.checked_add(128))
    })
}

fn command_pair(command: &Command) -> Option<(admission::Identity, KeyAction)> {
    match command {
        Command::Admitted { command, .. } | Command::InputClaim { command, .. } => {
            command_pair(command)
        }
        Command::OrderedInput { input, .. } => command_pair(input),
        Command::Key(input) => Some((admission::Identity::key(input), input.action)),
        Command::Mouse(input) if input.action != MouseAction::Motion => {
            let button = input.button?;
            if matches!(button, MouseButton::WheelUp | MouseButton::WheelDown) {
                return None;
            }
            Some((
                admission::Identity::Mouse(button),
                if input.action == MouseAction::Release {
                    KeyAction::Release
                } else {
                    KeyAction::Press
                },
            ))
        }
        _ => None,
    }
}

fn is_cancelable_input_before(command: &Command, boundary: u64) -> bool {
    match command {
        Command::Admitted { command, .. } | Command::InputClaim { command, .. } => {
            is_cancelable_input_before(command, boundary)
        }
        Command::OrderedInput {
            generation, input, ..
        } => *generation < boundary && is_cancelable_input_before(input, boundary),
        // A press may already be consumed. Preserve key and button releases
        // while canceling bulk input, so raw/extended apps never keep a key or
        // button pressed merely because Ctrl+C overtook its queued release.
        Command::Key(key) if key.action == KeyAction::Release => false,
        Command::Mouse(mouse) if mouse.action == MouseAction::Release => false,
        Command::Input(_) | Command::Paste(..) | Command::Key(_) | Command::Mouse(_) => true,
        _ => false,
    }
}

fn command_channel(capacity: usize) -> (Sender<Command>, Receiver<Command>) {
    crossbeam_channel::bounded(capacity.max(1))
}

struct WorkerCompletion {
    event: Option<TerminalEvent>,
    /// Natural exit/drain, checked teardown or committed carry released ownership.
    ownership_released: bool,
    natural_close: Option<(NaturalClose, bool)>,
}

struct NaturalClose {
    deadline: Instant,
    reply: Sender<super::ipc::TerminalCloseResult>,
}

enum CheckedClose {
    Finished(super::ipc::TerminalCloseResult),
    AwaitNaturalExit,
}

struct InputHandoff {
    token: Arc<AtomicBool>,
    resumed: Option<Sender<Result<(), String>>>,
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
    /// A shell's title as it last reported it. A change means a command
    /// started or ended even while what shows stays the same.
    reported_title: String,
    agent_exited: bool,
    root_exit: Option<TerminalEvent>,
    pty_eof: bool,
    natural_close: Option<NaturalClose>,
    terminal: Terminal<'static, 'static>,
    theme: Rc<Cell<TerminalTheme>>,
    key_encoder: KeyEncoder<'static>,
    mouse_encoder: MouseEncoder<'static>,
    effects: Rc<RefCell<Effects>>,
    bell: Bell,
    visual: Option<VisualResources>,
    visibility: Visibility,
    generation: u64,
    capture_dirty: bool,
    viewport_reads: Vec<(
        Option<u64>,
        Option<u16>,
        Sender<Result<Arc<RenderSnapshot>, String>>,
        Option<Arc<admission::Permit>>,
    )>,
    force_capture: bool,
    /// When a capture was first withheld for the synchronized update still in
    /// progress. `None` while no update is holding the grid back.
    synchronized_since: Option<std::time::Instant>,
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
    pending_input: input_queue::PendingInput,
    writer_failed: bool,
    #[cfg(test)]
    test_write_budget: Option<usize>,
    kernel_preservation_warned: bool,
    retained_commands: VecDeque<Command>,
    current_admission: Option<Arc<admission::Permit>>,
    current_input_claim: Option<Arc<admission::Cleanup>>,
    child: holder::HeldChild,
    child_exit_rx: Receiver<ChildExit>,
    child_identity: Option<RootIdentity>,
    checked_shutdown: bool,
    checked_close_started: bool,
    /// The terminal was left to its holder for a successor daemon, so ending
    /// this worker must not end its processes.
    carried: bool,
    /// The process that owns this terminal's PTY and reaps its root.
    /// Released last, once nothing here reads or writes the terminal.
    holder: holder::Holder,
    input_handoff: Option<InputHandoff>,
    reader_restart_pending: bool,
    reply_backpressured: bool,
    reader: Option<JoinHandle<()>>,
    /// Retained startup never lets this reader consume the PTY. The queued
    /// original replay prefix stays in pty_rx until explicit recovery.
    reader_start: Option<Sender<()>>,
    #[cfg(test)]
    test_startup_queue: Option<Sender<PtyMessage>>,
    /// Write end of the reader's wake pipe. Closing it releases a read that a
    /// disowned grandchild would otherwise hold open indefinitely.
    reader_wake: Option<OwnedFd>,
    /// Disconnects when the reader thread returns, so shutdown can wait for
    /// it with a deadline instead of joining unconditionally.
    reader_done: Receiver<()>,
    /// Destructive teardown can abandon an undelivered read. Carry leaves
    /// this false and drains every byte already consumed from the PTY.
    reader_discard_pending: Arc<AtomicBool>,
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
    #[cfg(test)]
    test_queue: Sender<PtyMessage>,
    discard_pending: Arc<AtomicBool>,
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
        #[cfg(test)]
        let test_queue = pty_tx.clone();
        let (done_tx, done) = crossbeam_channel::bounded::<()>(0);
        let (start, started) = crossbeam_channel::bounded(1);
        let discard_pending = Arc::new(AtomicBool::new(false));
        let reader_discard_pending = Arc::clone(&discard_pending);
        let thread = thread::Builder::new()
            .name(format!("terminal-pty-reader-{terminal_id:?}"))
            .spawn(move || {
                let _done = done_tx;
                if started.recv().is_ok() {
                    read_pty(
                        &mut reader,
                        &pty_tx,
                        readiness,
                        wake_rx,
                        &reader_discard_pending,
                    );
                }
            })
            .map_err(|error| format!("start PTY reader: {error}"))?;
        Ok(Self {
            thread,
            start: Some(start),
            wake,
            done,
            output,
            #[cfg(test)]
            test_queue,
            discard_pending,
        })
    }

    fn activate(&mut self) {
        if let Some(start) = self.start.take() {
            let _ = start.send(());
        }
    }
}

impl Worker {
    fn prepare(
        session: TerminalSession,
        config: TerminalConfig,
        execution_recipe: Option<LaunchRecipe>,
        commands: Receiver<Command>,
        events: Sender<TerminalEvent>,
        published: Arc<PublishedState>,
    ) -> Result<Self, String> {
        if config
            .holder_recovery
            .as_ref()
            .is_some_and(|recovery| recovery.execution_recipe.as_ref() != execution_recipe.as_ref())
        {
            return Err("Terminal execution does not match its retained holder context".into());
        }
        let current_size = TerminalSize {
            columns: config.columns.max(1),
            rows: config.rows.max(1),
            cell_width_px: config.cell_width_px.max(1),
            cell_height_px: config.cell_height_px.max(1),
        };
        let mut terminal = Terminal::new(TerminalOptions {
            cols: current_size.columns,
            rows: current_size.rows,
            max_scrollback: config.max_scrollback_bytes,
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

        let effects = Rc::new(RefCell::new(Effects::default()));
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

        terminal
            .on_pwd_changed({
                let effects = Rc::clone(&effects);
                move |_| effects.borrow_mut().push(Effect::WorkingDirectoryChanged)
            })
            .map_err(|error| format!("register working directory effect: {error}"))?;

        let execution = execution_recipe.unwrap_or_else(|| session.launch.clone());
        let mut command = match &config.execution_environment {
            Some(environment) => command_from_execution(&execution, environment),
            None => command_from_recipe(&execution),
        };
        for name in crate::account::launch_credential_overrides(&execution) {
            command.env_remove(name);
        }
        command.env("CAGENTS_TERMINAL_ID", session.id.to_string());
        match crate::instance::development_instance() {
            Some(instance) => command.env(crate::instance::DEVELOPMENT_INSTANCE_ENV, instance),
            None => command.env_remove(crate::instance::DEVELOPMENT_INSTANCE_ENV),
        };
        // A cagents process started in this terminal must not take the
        // account selected for it for the system account.
        let home_of = |name: &str| command.get_env(name)?.to_str().map(str::to_owned);
        let selection = crate::account::registry::selection_marker(home_of);
        // It must keep the Claude home the reader started cagents with,
        // though, rather than take it for the default earlier cagents
        // exported to every launch.
        let system = crate::account::system_home_marker(home_of, selection.as_deref(), |name| {
            std::env::var(name).ok()
        });
        for (marker, value) in [
            (crate::account::SELECTION_MARKER, selection),
            (crate::account::SYSTEM_HOME_MARKER, system),
        ] {
            match value {
                Some(value) => command.env(marker, value),
                None => command.env_remove(marker),
            }
        }
        command.env_remove(crate::agent_execution::AUTHORITY_ENV);
        command.env_remove(crate::agent_execution::CONSOLE_STARTUP_OWNER_ENV);
        if let Some(proof) = &config.console_startup_owner {
            command.env(
                crate::agent_execution::CONSOLE_STARTUP_OWNER_ENV,
                serde_json::to_string(proof).map_err(|error| error.to_string())?,
            );
        }
        if let Some(token) = config.agent_authority.token() {
            command.env(crate::agent_execution::AUTHORITY_ENV, token);
        }
        command.env(
            "CAGENTS_RECOVERY_ORIGIN",
            session.recovery_origin.unwrap_or(session.id).to_string(),
        );
        let executable = interactive_shell_launch(&execution)
            .then(|| RootIdentity::launch_executable(&command))
            .flatten();
        let (child_exit_tx, child_exit_rx) = crossbeam_channel::bounded(1);
        let (held, output) = match &config.adoption {
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
                effects.borrow_mut().retain_title();
                if adopted.screen_lost {
                    // A crash has no emulator snapshot. Nudge full-screen
                    // clients to redraw on their surviving controlling PTY.
                    adopted
                        .held
                        .master
                        .request_redraw()
                        .map_err(|error| format!("request surviving terminal redraw: {error}"))?;
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
                    crate::agent::Provider::from_program(&execution.program),
                    &skill_environment,
                    &execution.working_directory,
                );
                if let Some(path) = crate::agent_execution::bundled::matched_path(
                    command.get_env("PATH").and_then(|value| value.to_str()),
                ) {
                    command.env("PATH", path);
                }
                // Before the holder forks, so nothing this launch starts
                // predates its record.
                crate::lineage_birth::record_instance_launch(&session)?;
                let isolate_codex_tmp = matches!(
                    crate::agent::Provider::launched(&execution),
                    Some((crate::agent::Provider::Codex, _))
                );
                #[cfg(test)]
                let reservation = published.test_holder_reservation.lock().unwrap().take();
                #[cfg(test)]
                let held = if let Some(reservation) = reservation {
                    holder::spawn_reserved(
                        command,
                        pty_size(current_size),
                        child_exit_tx,
                        config.holder_recovery.clone(),
                        isolate_codex_tmp,
                        reservation,
                    )?
                } else {
                    holder::spawn_recoverable(
                        command,
                        pty_size(current_size),
                        child_exit_tx,
                        config.holder_recovery.clone(),
                        isolate_codex_tmp,
                    )?
                };
                #[cfg(not(test))]
                let held = holder::spawn_recoverable(
                    command,
                    pty_size(current_size),
                    child_exit_tx,
                    config.holder_recovery.clone(),
                    isolate_codex_tmp,
                )?;
                (held, Vec::new())
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
        #[cfg(test)]
        {
            // The existing holder pidfd must still reach its original root
            // after opening the fixture capability. A reaped/reused PID never
            // reaches the failure guard. Quick natural exits need no fallback.
            let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, held.child.pid(), 0) };
            if raw >= 0 {
                let descriptor = unsafe { File::from_raw_fd(raw as i32) };
                if held.child.unreaped() {
                    let _ = published.test_input_root_pidfd.set(descriptor);
                }
            }
        }
        let writer = File::from(duplicate("writing")?);
        // Duplicated master descriptors share status flags. Existing reader
        // and detached-holder paths already rearm on WouldBlock.
        let flags = unsafe { libc::fcntl(writer.as_raw_fd(), libc::F_GETFL) };
        if flags < 0
            || unsafe { libc::fcntl(writer.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) }
                < 0
        {
            return Err(format!(
                "make PTY nonblocking: {}",
                std::io::Error::last_os_error()
            ));
        }
        // Output an adopted terminal produced between daemons comes first, as
        // any other output would.
        let mut reader = Reader::prepare(session.id, &held.master, config.pty_capacity, output)?;

        let mut transcript = session
            .transcript_path
            .map(|path| {
                TranscriptWriter::start(
                    path,
                    session.id,
                    config.transcript_capacity.max(1),
                    config.transcript_max_bytes.max(1),
                )
            })
            .transpose()?;
        if let Some(location) = transcript
            .as_mut()
            .and_then(|writer| writer.location.take())
        {
            let _ = published.transcript_location.set(location);
        }
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
        let original_holder_process = held
            .holder
            .original_process_descriptor()
            .map_err(|error| format!("retain original holder observation: {error}"))?;
        // Install only an already-held exact root identity, and allocate its
        // CLOEXEC clone before adoption commit can transfer replay custody.
        let launch_directory = held
            .launch_directory
            .as_ref()
            .map(|(identity, file)| {
                held.child
                    .duplicate_pidfd()
                    .map(|root| ((*identity, Arc::clone(file)), root))
                    .map_err(|error| format!("pin original terminal root: {error}"))
            })
            .transpose()?;
        // Replay transfer and reader activation remain in commit_start, after
        // the complete original Worker has moved outside the panic guard.
        if let Some(original) = original_holder_process {
            let _ = published.original_holder_process.set(Arc::new(original));
        }
        if let Some(role) = held.holder.original_role() {
            let _ = published.original_holder_role.set(role.clone());
        }
        if let Some(record) = held.holder.record() {
            let _ = published.holder_identity.set(record.clone());
        }
        if let Some(pin) = launch_directory {
            let _ = published.launch_directory.set(pin);
        }

        let events = Arc::new(events);
        let _ = published.semantic_events.set(Arc::downgrade(&events));

        let worker = Self {
            terminal_id: session.id,
            resume_hint: crate::model::agent_recovery::observed_hint_from_launch(&execution),
            resume_foreground: None,
            foreground_poll: FOREGROUND_POLL_MIN,
            last_foreground_observation: std::time::Instant::now(),
            agent_provider: None,
            agent_process: None,
            display_title: String::new(),
            reported_title: String::new(),
            agent_exited: false,
            root_exit: None,
            pty_eof: false,
            natural_close: None,
            launch: execution,
            terminal,
            theme,
            key_encoder,
            mouse_encoder,
            effects,
            bell: Bell::default(),
            visual,
            visibility,
            generation: 0,
            capture_dirty: false,
            viewport_reads: Vec::new(),
            force_capture: false,
            synchronized_since: None,
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
            pending_input: input_queue::PendingInput::default(),
            writer_failed: false,
            #[cfg(test)]
            test_write_budget: None,
            kernel_preservation_warned: false,
            retained_commands: VecDeque::new(),
            current_admission: None,
            current_input_claim: None,
            child: held.child,
            child_exit_rx,
            child_identity,
            checked_shutdown: false,
            checked_close_started: false,
            carried: false,
            holder: held.holder,
            pty_capacity: config.pty_capacity,
            input_handoff: None,
            reader_restart_pending: false,
            reply_backpressured: false,
            reader: Some(reader.thread),
            reader_start: reader.start.take(),
            #[cfg(test)]
            test_startup_queue: Some(reader.test_queue),
            reader_wake: Some(reader.wake),
            reader_done: reader.done,
            reader_discard_pending: reader.discard_pending,
        };
        Ok(worker)
    }

    /// Called only with the entire original Worker owned OUTSIDE the caller's
    /// panic guard. No error/panic can unwind its replay prefix out of custody.
    fn commit_start(&mut self) -> Option<String> {
        #[cfg(test)]
        let retained = if self
            .published
            .test_fail_startup_commit
            .load(Ordering::Acquire)
        {
            self.holder.fail_after_actual_commit_for_test().err()
        } else {
            self.holder.commit_adoption().err()
        };
        #[cfg(not(test))]
        let retained = self.holder.commit_adoption().err();
        let mut retained = retained;
        if retained.is_none()
            && let Some(start) = self.reader_start.take()
            && start.send(()).is_err()
        {
            retained = Some(
                "The prepared reader did not accept activation; original replay is retained".into(),
            );
        }
        #[cfg(test)]
        if retained.is_none() {
            drop(self.test_startup_queue.take());
        }
        retained
    }

    #[cfg(test)]
    fn start(
        session: TerminalSession,
        config: TerminalConfig,
        execution_recipe: Option<LaunchRecipe>,
        commands: Receiver<Command>,
        events: Sender<TerminalEvent>,
        published: Arc<PublishedState>,
    ) -> Result<(Self, Option<String>), String> {
        let mut worker = Self::prepare(
            session,
            config,
            execution_recipe,
            commands,
            events,
            published,
        )?;
        let retained =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| worker.commit_start()))
                .unwrap_or_else(|_| {
                    Some("Commit panicked with its complete original owner retained".into())
                });
        Ok((worker, retained))
    }

    fn run_retained_startup(mut self) {
        loop {
            #[cfg(test)]
            {
                let probe = self.published.test_startup_probe.lock().unwrap().take();
                if let Some(probe) = probe {
                    // The reader is unactivated: this observes and reinstalls
                    // its actual sole replay message, not a second model/DTO.
                    let queued = self.pty_rx.try_iter().collect::<Vec<_>>();
                    let mut raw = Vec::new();
                    for message in queued {
                        if let PtyMessage::Bytes(bytes) = &message {
                            raw.extend_from_slice(bytes);
                        }
                        self.test_startup_queue
                            .as_ref()
                            .unwrap()
                            .try_send(message)
                            .unwrap();
                    }
                    let (pending, cleanup) = self.holder.startup_cleanup_state_for_test();
                    let _ = probe.send((raw, pending, cleanup));
                }
            }
            let close = self
                .published
                .priority_close
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .take();
            if let Some(command) = close
                && let Command::CloseChecked {
                    allow_active,
                    deadline,
                    reply,
                } = command
            {
                // A panicking observer/inspection cannot unwind this
                // complete original owner out of its parked frame.
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    self.checked_close(allow_active, deadline)
                }))
                .unwrap_or_else(|_| {
                    super::ipc::TerminalCloseResult::Unknown(
                        "Checked close panicked; original startup custody remains retained".into(),
                    )
                });
                let closed = outcome == super::ipc::TerminalCloseResult::Closed;
                if closed {
                    // Only an explicit checked destructive close releases
                    // unresolved replay. Ordinary shutdown/Drop does not.
                    self.holder.prove_startup_cleanup();
                    drop(self.reader_start.take());
                    self.published
                        .startup_unresolved
                        .store(false, Ordering::Release);
                    self.published.request_shutdown();
                    let _ = reply.send(outcome);
                    self.run();
                    return;
                }
                let _ = reply.send(outcome);
            }
            // The original reader remains unactivated. In particular, no raw
            // query can be parsed or answered before ownership is confirmed.
            // A dropped external observer is not an owner-release instruction.
            let _ = self.published.capture_rx.recv();
        }
    }

    #[cfg(test)]
    fn disconnected_exit_report(&mut self) {
        let (sender, receiver) = crossbeam_channel::bounded(1);
        drop(sender);
        self.child_exit_rx = receiver;
    }

    fn run(self) {
        let published = Arc::clone(&self.published);
        let events = self.events.clone();
        // Returning from the consuming frame drops the emulator, encoders and
        // render state after final capture and PTY cleanup. Only then may a
        // completion observer rely on ended continuity or retire the result.
        // A panic during teardown skips both publications and retains live
        // inventory rather than certifying unobserved cleanup.
        let completion = self.run_to_completion();
        published
            .completed
            .store(completion.ownership_released, Ordering::Release);
        if let Some(completion) = completion.event {
            published.emit_event(&events, completion);
        }
        if let Some((close, drained)) = completion.natural_close {
            use super::ipc::TerminalCloseResult;
            let result = if drained && Instant::now() < close.deadline {
                TerminalCloseResult::Closed
            } else {
                published
                    .input_order
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .handoff = false;
                TerminalCloseResult::Unknown("Terminal process cleanup remains unconfirmed".into())
            };
            let _ = close.reply.send(result);
        }
    }

    fn run_to_completion(mut self) -> WorkerCompletion {
        // An adopted terminal's title, set by drawing its screen again.
        self.drain_effects();
        self.publish_fallback_title();
        if self.visibility == Visibility::Visible {
            self.publish_snapshot(true);
        }

        let mut pty_open = true;
        let mut exited = false;
        let mut shutting_down = false;
        let mut carried_command = None;
        let deferred_commands = crossbeam_channel::never();
        while !shutting_down && (pty_open || !exited) {
            if self
                .natural_close
                .as_ref()
                .is_some_and(|close| Instant::now() >= close.deadline)
            {
                self.refuse_natural_close();
            }
            if self.resume_cancelled_input_handoff() || self.resume_reader_after_refusal() {
                pty_open = true;
            }
            if self.published.shutdown_requested.load(Ordering::Acquire) {
                shutting_down = true;
                break;
            }
            if self.published.interrupt_requested.load(Ordering::Acquire) {
                let boundary = self.interrupt_input_with_carried(carried_command.as_ref());
                if carried_command
                    .as_ref()
                    .is_some_and(|command| is_cancelable_input_before(command, boundary))
                {
                    carried_command = None;
                }
            }
            let close = self
                .published
                .priority_close
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .take();
            if let Some(close) = close {
                if let Some(carried) = carried_command.take() {
                    self.retained_commands.push_back(carried);
                }
                if self.handle_command(close) {
                    shutting_down = true;
                    break;
                }
            }
            let cleanup_budget = if self.natural_close.is_some() {
                0
            } else {
                admission::CLEANUP_LIMIT
            };
            for _ in 0..cleanup_budget {
                let Some(command) = self.published.take_cleanup_command(true) else {
                    break;
                };
                let Some(command) = self.command_before_interrupt(command) else {
                    break;
                };
                if self.handle_command(command) {
                    shutting_down = true;
                    break;
                }
            }
            if shutting_down {
                break;
            }
            if self.natural_close.is_none() {
                self.flush_pty_input();
            }
            self.drain_effects();
            self.publish_bell();
            // Output or a busy input queue must not keep resetting discovery's
            // timeout: an agent can start in an existing shell's process group.
            if self.last_foreground_observation.elapsed() >= self.foreground_poll {
                self.observe_foreground();
            }
            let deferred = self.natural_close.is_some();
            let event = (!deferred)
                .then(|| {
                    self.retained_commands
                        .pop_front()
                        .or_else(|| carried_command.take())
                })
                .flatten()
                .map(|command| WorkerEvent::Command(Some(command)))
                .unwrap_or_else(|| {
                    wait_for_worker_event(
                        if deferred {
                            &deferred_commands
                        } else {
                            &self.commands
                        },
                        &self.pty_rx,
                        &self.child_exit_rx,
                        &self.published.capture_rx,
                        pty_open && self.reader.is_some() && !self.reply_pending(),
                        !exited,
                        self.foreground_poll
                            .saturating_sub(self.last_foreground_observation.elapsed()),
                        self.synchronized_release_after()
                            .into_iter()
                            .chain(self.bell.release_after())
                            .chain(
                                (!deferred
                                    && (!self.pending_input.is_empty() || self.reply_pending()))
                                .then_some(PTY_WRITE_RETRY),
                            )
                            .chain(self.natural_close.as_ref().map(|close| {
                                close.deadline.saturating_duration_since(Instant::now())
                            }))
                            .min(),
                    )
                });
            // Before the event, so a command queued after a visibility
            // request sees it applied, as it would had the request queued.
            self.apply_host_visibility();
            match event {
                WorkerEvent::Command(Some(command)) => {
                    // A command selected together with the priority wake must
                    // cross the cancellation boundary before it can write.
                    let Some(command) = self.command_before_interrupt(command) else {
                        continue;
                    };
                    self.retain_carried_for_handoff(&command, &mut carried_command);
                    // Wheel bursts otherwise queue one repaint per event. Fold
                    // adjacent viewport deltas before publishing a snapshot.
                    let command =
                        coalesce_scroll_command(command, &self.commands, &mut carried_command);
                    shutting_down = self.handle_command(command);
                }
                WorkerEvent::Command(None) => shutting_down = true,
                WorkerEvent::Capture => self.capture_if_ready(),
                // Foreground observation and pending bells run at the top of
                // the loop. An expired synchronized update is released here.
                WorkerEvent::Timer => self.capture_if_ready(),
                WorkerEvent::Pty(message) => {
                    pty_open = self.handle_pty_message(message);
                }
                WorkerEvent::ChildExit(message) => {
                    self.handle_child_exit(
                        &mut exited,
                        message.unwrap_or_else(|| Err("child reaper closed".into())),
                    );
                    if !matches!(self.root_exit, Some(TerminalEvent::Exited { .. })) {
                        self.refuse_natural_close();
                    }
                }
            }
        }

        // A readable root pidfd only selected this reconciliation route. The
        // actual holder report and PTY EOF together prove natural completion.
        let natural_close = self.natural_close.take().map(|close| {
            let drained = !shutting_down
                && !self.checked_close_started
                && self.pty_eof
                && matches!(self.root_exit, Some(TerminalEvent::Exited { .. }));
            (close, drained)
        });

        // A last bell may still be waiting for its cadence deadline when the
        // PTY drains. Deliver its attention before completion, without holding
        // teardown open for a timer.
        if std::mem::take(&mut self.bell.pending) {
            self.emit(TerminalEvent::Bell);
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
            // Nothing will end a synchronized update the child left open.
            self.capture_when_ready(false);
        }
        let shutdown_cleanup_confirmed = if shutting_down && !self.carried {
            let budget = self.published.shutdown_budget();
            if exited {
                let cleanup = if self.checked_shutdown {
                    Ok(())
                } else {
                    confirm_exited_family(
                        &self.child,
                        &self.master,
                        Instant::now() + budget.deadline,
                        budget.census_reserve,
                    )
                };
                match cleanup {
                    Ok(()) => true,
                    Err(error) => {
                        self.error("wait for child", error);
                        false
                    }
                }
            } else {
                #[cfg(test)]
                if self
                    .published
                    .shutdown_census_failure
                    .load(Ordering::Acquire)
                {
                    close_processes::fail_next_census_for_test();
                }
                #[cfg(test)]
                if let Some(finished) = self.published.shutdown_census_delay.lock().unwrap().take()
                {
                    close_processes::delay_next_census_for_test(finished);
                }
                let result = shutdown_child_exit(
                    &self.child,
                    &self.master,
                    self.child_identity.as_ref(),
                    &self.child_exit_rx,
                    self.checked_shutdown,
                    budget,
                );
                let confirmed = result.is_ok();
                self.handle_child_exit(&mut exited, result);
                confirmed
            }
        } else {
            true
        };
        if let Some(transcript) = self.transcript.take() {
            transcript.finish();
        }

        // Release the reader's blocking wait before waiting on it, then bound
        // that wait: a reader still stuck on a descriptor a disowned
        // grandchild holds open is abandoned rather than joined forever,
        // because a hung join here wedges the daemon's dispatch thread.
        self.reader_discard_pending.store(true, Ordering::Release);
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
        // A failed destructive close retains its original family obligation.
        // Later root exit and EOF cannot substitute for successful family kill.
        // The shared input seal alone is insufficient: carry also uses it.
        let family_cleanup_confirmed =
            shutdown_cleanup_confirmed && (!self.checked_close_started || self.checked_shutdown);
        let completion = self.root_exit.take().and_then(|event| match event {
            TerminalEvent::Exited {
                exit_code,
                signal,
                intentional,
            } if family_cleanup_confirmed && (shutting_down || self.pty_eof) => {
                Some(TerminalEvent::Completed {
                    exit_code,
                    signal,
                    intentional,
                })
            }
            _ => None,
        });
        if !shutting_down && completion.is_some() {
            {
                let mut phase = self
                    .published
                    .archive
                    .lock()
                    .unwrap_or_else(|p| p.into_inner());
                *phase = ArchivePhase::Preparing;
                // A request the loop did not reach goes to the retained
                // surface, as every later one does.
                if let Some(visibility) = self.published.host_visibility.take() {
                    self.published
                        .visible
                        .store(visibility == Visibility::Visible, Ordering::Release);
                }
            }
            let history = archive::History::capture(
                &mut self.terminal,
                &self.published.snapshot.load_full(),
                holder::MAX_SCREEN,
            );
            if let Ok(history) = &history {
                let generation = self
                    .published
                    .published_generation
                    .load(Ordering::Acquire)
                    .saturating_add(1);
                self.published.snapshot.store(history.snapshot(generation));
                self.published
                    .published_generation
                    .store(generation, Ordering::Release);
                if history.omitted > 0 {
                    self.error("retain completed terminal history", format!("The completed terminal retains its newest rows within 8 MiB; {} older rows are unavailable.", history.omitted));
                }
            } else if let Err(message) = &history {
                self.error("retain completed terminal history", message.clone());
            }
            self.published.freeze(history, self.events.clone());
            self.published
                .request_wake(self.published.published_generation.load(Ordering::Acquire));
            while let Ok(command) = self.commands.try_recv() {
                let _ = self.published.archived_command(command);
            }
        }
        let ownership_released = completion.is_some() || self.carried;
        if !ownership_released && self.holder.registered() {
            // A missing exit report or failed PTY drain cannot authorize a
            // replacement or erase the holder's recovery record. Retain it for
            // a successor without claiming a successful completed handoff.
            if let Err(error) = carry::screen(&mut self.terminal, holder::MAX_SCREEN)
                .and_then(|screen| self.holder.cache_screen(&screen))
            {
                self.error("retain unconfirmed terminal screen", error);
            }
            if shutting_down {
                self.holder.leave_after_unconfirmed_shutdown();
            } else {
                self.holder.leave_for_successor();
            }
            drop(self.holder);
        } else if !shutting_down && completion.is_some() && self.holder.registered() {
            self.holder.prove_cleanup();
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
        WorkerCompletion {
            event: completion,
            ownership_released,
            natural_close,
        }
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
                            self.pty_eof = true;
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
            Some(PtyMessage::Eof) => {
                self.pty_eof = true;
                false
            }
            None => false,
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
        self.observe_synchronized_output(&bytes);

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
    fn command_before_interrupt(&mut self, command: Command) -> Option<Command> {
        if self.published.interrupt_requested.load(Ordering::Acquire) {
            // A coalesced scroll may already own the carry slot. Return the
            // selected command to the inbox without replacing that control.
            self.retained_commands.push_front(command);
            None
        } else {
            Some(command)
        }
    }

    fn retain_carried_for_handoff(&mut self, command: &Command, carried: &mut Option<Command>) {
        fn changes_owner(command: &Command) -> bool {
            match command {
                Command::Admitted { command, .. } | Command::InputClaim { command, .. } => {
                    changes_owner(command)
                }
                Command::PrepareInputHandoff { .. }
                | Command::PrepareUpgrade(_)
                | Command::Carry { .. }
                | Command::PreserveForRestart(_) => true,
                _ => false,
            }
        }
        if changes_owner(command)
            && let Some(command) = carried.take()
        {
            // A scroll coalescer may have consumed an accepted input before
            // this control was selected. Include its SAME owner/credits in
            // the drain proof before an irreversible handoff can proceed.
            self.retained_commands.push_front(command);
        }
    }

    fn handle_command(&mut self, command: Command) -> bool {
        match command {
            Command::InputClaim {
                claim,
                ticket,
                command,
            } => {
                let previous = self.current_input_claim.replace(claim);
                let close = self.handle_command(*command);
                self.current_input_claim = previous;
                drop(ticket);
                return close;
            }
            Command::Admitted { permit, command } => {
                let previous = self.current_admission.replace(permit);
                let close = self.handle_command(*command);
                self.current_admission = previous;
                return close;
            }
            Command::OrderedInput {
                admission, input, ..
            } => {
                let _admission = admission;
                // A release can arrive after the loop selected this input.
                // Finish eligible earlier releases before encoding it, while
                // retaining this input's ticket so newer releases cannot pass.
                let ordinary = self.current_admission.take();
                for _ in 0..admission::CLEANUP_LIMIT {
                    let Some(cleanup) = self.published.take_cleanup_command(true) else {
                        break;
                    };
                    self.handle_command(cleanup);
                }
                self.current_admission = ordinary;
                return self.handle_command(*input);
            }
            Command::Interrupt(key) => {
                self.published.request_interrupt(key);
            }

            Command::ReadViewport {
                offset,
                rows,
                reply,
            } => {
                if self.visibility != Visibility::Visible {
                    // Nothing is captured until a reveal, so the read cannot
                    // wait for one. Hibernated or never shown, the published
                    // marker is the answer; hidden, the parser's screen is.
                    let base = self.published.snapshot.load_full();
                    let _ = reply.send(if self.visual.is_none() {
                        Ok(base)
                    } else {
                        super::snapshot::capture_viewport(
                            &self.terminal,
                            &base,
                            offset,
                            rows.unwrap_or(base.rows),
                        )
                        .map_err(|error| error.to_string())
                    });
                    return false;
                }
                self.viewport_reads
                    .push((offset, rows, reply, self.current_admission.clone()));
                if self.capture_dirty {
                    // This explicit next-frame request grants one capture
                    // credit. Waiting for its future ACK would deadlock.
                    self.published
                        .acknowledged_generation
                        .fetch_max(self.generation, Ordering::AcqRel);
                    self.capture_when_ready(true);
                }
                if !self.capture_dirty {
                    self.finish_viewport_reads();
                }
            }
            Command::ApplyAgentActivity {
                process,
                activity,
                title,
                reply,
                deadline,
            } => {
                let proposed = Some(activity.unwrap_or(crate::agent::Activity {
                    provider: process.provider,
                    state: crate::agent::ActivityState::Unknown,
                }));
                let current_activity = *self
                    .published
                    .agent_activity
                    .lock()
                    .unwrap_or_else(|p| p.into_inner());
                let unchanged = self.agent_process.as_ref() == Some(&*process)
                    && current_activity == proposed
                    && title.as_ref().is_none_or(|title| {
                        self.published
                            .conversation_title
                            .lock()
                            .unwrap_or_else(|p| p.into_inner())
                            .as_deref()
                            .is_some_and(|current| current.chars().eq(title.chars().take(256)))
                    });
                // An unchanged sample has nothing to apply. Validate liveness
                // without walking launchers or reading environments; periodic
                // discovery still notices exec and refreshes hidden recovery.
                // Never use this shortcut to publish a changed activity/title.
                if unchanged
                    && std::time::Instant::now() < deadline
                    && self.foreground_group().is_some_and(|group| {
                        crate::model::agent_recovery::process_in_foreground(&process, group)
                            && self.foreground_group() == Some(group)
                    })
                {
                    let _ = reply.send(current_activity);
                    return false;
                }
                let current = self.observe_agent_process();
                let valid = std::time::Instant::now() < deadline && current == Some(&*process);
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
            } => match self.begin_checked_close(allow_active, deadline, true) {
                CheckedClose::Finished(outcome) => {
                    let close = outcome == super::ipc::TerminalCloseResult::Closed;
                    let _ = reply.send(outcome);
                    return close;
                }
                CheckedClose::AwaitNaturalExit => {
                    self.natural_close = Some(NaturalClose { deadline, reply });
                }
            },
            Command::PrepareInputHandoff {
                token,
                resumed,
                reply,
            } => {
                self.input_handoff = Some(InputHandoff {
                    token: token.clone(),
                    resumed: Some(resumed),
                });
                let result = if !token.load(Ordering::Acquire) {
                    Err("Terminal input preparation was cancelled".into())
                } else {
                    self.settle_viewport_reads_for_handoff();
                    self.input_drained_for_handoff()
                        .and_then(|()| self.stop_reading())
                        .and_then(|()| self.input_drained_for_handoff())
                        .and_then(|()| {
                            token
                                .load(Ordering::Acquire)
                                .then_some(())
                                .ok_or_else(|| "Terminal input preparation was cancelled".into())
                        })
                };
                if result.is_err() {
                    token.store(false, Ordering::Release);
                    self.resume_cancelled_input_handoff();
                }
                let _ = reply.send(result);
            }
            Command::PrepareUpgrade(reply) => {
                self.settle_viewport_reads_for_handoff();
                let result = self.input_drained_for_handoff().and_then(|()| self
                    .inspect_close(std::time::Instant::now() + std::time::Duration::from_secs(3)))
                    .and_then(|(active, family)| {
                        self.refresh_resume_hint(true);
                        if active && self.resume_hint.as_deref()
                            .and_then(crate::model::agent_recovery::recipe_from_hint).is_some()
                            && !self.agent_process.as_ref().is_some_and(|process| family.only_agent_tree(process.pid))
                        {
                            return Err("Terminal also owns unrelated background jobs that cannot be checkpointed".into());
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
                    if self.input_handoff.is_none() {
                        self.restart_reader_after_refusal();
                    }
                    let _ = reply.send(Err(
                        "legacy terminal has no persistent holder endpoint".into()
                    ));
                    return false;
                }
                self.settle_viewport_reads_for_handoff();
                let result = self
                    .input_drained_for_handoff()
                    .and_then(|()| self.stop_reading())
                    .and_then(|()| self.input_drained_for_handoff());
                if let Err(error) = result {
                    if self.input_handoff.is_none() {
                        self.restart_reader_after_refusal();
                    }
                    let _ = reply.send(Err(error));
                    return false;
                }
                self.carried = true;
                self.complete_input_handoff();
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
            Command::Input(bytes) => {
                self.write_user_input(&bytes, "write terminal input");
            }
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
                    Ok(written) => {
                        self.write_pty(&buffer[..written], "write focus report");
                    }
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
            Command::Search {
                owner,
                request,
                query,
                case_sensitive,
                latest,
            } => {
                match super::search::search(&self.terminal, &query, case_sensitive, || {
                    latest.load(Ordering::Acquire) != request
                }) {
                    Ok(Some((matches, truncated))) => self.emit(TerminalEvent::SearchResults {
                        owner,
                        request,
                        query,
                        matches,
                        truncated,
                    }),
                    Ok(None) => {}
                    Err(error) => self.error("search terminal", error.to_string()),
                }
            }
            Command::Hide => self.set_hidden(),
            Command::ClearScrollback => {
                self.terminal.vt_write(b"\x1b[3J");
                self.publish_snapshot(true);
            }
            Command::CopySelection { request, selection } => {
                match super::search::selected_text(&self.terminal, selection) {
                    Ok(text) => self.emit(TerminalEvent::ClipboardText { request, text }),
                    Err(error) => self.error("copy terminal selection", error),
                }
            }
            Command::Reveal => self.reveal(),
            Command::Hibernate => self.hibernate_visuals(),
            Command::Restore { visible } => self.restore_visuals(visible),
            #[cfg(test)]
            Command::Shutdown => return true,
        }
        false
    }

    fn encode_key(&mut self, mut input: KeyInput) {
        if input.action == KeyAction::Repeat && !self.pending_input.has_key_owner(&input) {
            // Its earlier unsent down may have been canceled at a priority
            // boundary. A later accepted repeat establishes a fresh down.
            input.action = KeyAction::Press;
        }
        let mut release = input.clone();
        release.action = KeyAction::Release;
        release.text = None;
        let paired = match encode_key_input(&self.terminal, &mut self.key_encoder, release) {
            Ok(bytes) => !bytes.is_empty(),
            Err(error) => {
                self.error("encode key release", error.to_string());
                return;
            }
        };
        match encode_key_input(&self.terminal, &mut self.key_encoder, input.clone()) {
            Ok(encoded) => {
                if self.pending_input.key_release_protocol_unknown(
                    &input,
                    &encoded,
                    self.current_input_claim.as_ref(),
                ) {
                    self.error("release terminal key", "The terminal changed its key protocol before an owned release. Delivery is unconfirmed; the original input ownership is retained.".into());
                    return;
                }
                self.flush_pty_input();
                let admitted = self.pending_input.admit_key(
                    &input,
                    &encoded,
                    paired,
                    self.current_admission.clone(),
                    self.current_input_claim.clone(),
                );
                if !admitted {
                    self.input_backpressure("write encoded key");
                }
                if admitted {
                    self.flush_pty_input();
                }
            }
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
                self.flush_pty_input();
                let bracketed = self.terminal.mode(Mode::BRACKETED_PASTE).unwrap_or(false);
                if !self.pending_input.admit_paste(
                    &encoded,
                    bracketed,
                    self.current_admission.clone(),
                ) {
                    self.input_backpressure("write terminal paste");
                    return;
                }
                self.flush_pty_input();
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
            Ok(()) => {
                if self.pending_input.mouse_release_protocol_unknown(
                    &input,
                    &encoded,
                    self.current_input_claim.as_ref(),
                ) {
                    self.error("release terminal mouse button", "The terminal changed its mouse protocol before an owned release. Delivery is unconfirmed; the original input ownership is retained.".into());
                    return;
                }
                self.flush_pty_input();
                if !self.pending_input.admit_mouse(
                    &input,
                    &encoded,
                    self.current_admission.clone(),
                    self.current_input_claim.clone(),
                ) {
                    self.input_backpressure("write encoded mouse event");
                    return;
                }
                self.flush_pty_input();
            }
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
                Ok(()) => {
                    self.applied_parser_size = size;
                    // The parser ends an update when it resizes; the next one
                    // gets its own full hold.
                    self.synchronized_since = None;
                }
                Err(error) => self.error("resize terminal", error.to_string()),
            }
        }
        self.current_size = size;
        if self.visibility == Visibility::Visible {
            self.publish_snapshot(true);
        }
    }

    fn apply_host_visibility(&mut self) {
        match self.published.host_visibility.take() {
            Some(Visibility::Visible) => self.reveal(),
            Some(Visibility::Hidden) => self.set_hidden(),
            Some(Visibility::Hibernated) => self.hibernate_visuals(),
            None => {}
        }
    }

    fn set_hidden(&mut self) {
        self.visibility = Visibility::Hidden;
        self.published.stop_presenting();
        // Nothing is published while away, so there is no update to time.
        self.synchronized_since = None;
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
        // An update seen before the card went away must not count against one
        // in progress now: that one gets its own full hold.
        self.synchronized_since = None;
        // Observe promptly again: whatever ran in the background while this
        // card was away is what the user is now looking at.
        self.foreground_poll = FOREGROUND_POLL_MIN;
        self.published.visible.store(true, Ordering::Release);
        self.publish_snapshot(true);
    }

    fn hibernate_visuals(&mut self) {
        self.visibility = Visibility::Hibernated;
        self.published.stop_presenting();
        self.synchronized_since = None;
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
        self.capture_when_ready(true);
    }

    /// Publishes the grid if it changed and the consumer has returned credit.
    /// With `honor_synchronized_output`, an update in progress keeps the grid
    /// back whatever asked for it: a reveal or theme change mid-update would
    /// otherwise show the frame half drawn. A resize is never held, because
    /// the parser ends the update when it resizes.
    fn capture_when_ready(&mut self, honor_synchronized_output: bool) {
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
        if honor_synchronized_output && self.synchronized_output_holds() {
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
        self.finish_viewport_reads();
    }

    fn finish_viewport_reads(&mut self) {
        if self.capture_dirty {
            return;
        }
        let base = self.published.snapshot.load_full();
        for (offset, rows, reply, _permit) in std::mem::take(&mut self.viewport_reads) {
            let rows = rows.unwrap_or(base.rows);
            let result = super::snapshot::capture_viewport(&self.terminal, &base, offset, rows)
                .map_err(|error| error.to_string());
            let _ = reply.send(result);
        }
    }

    /// Whether the child is inside a synchronized update that may still
    /// withhold its output, starting the update's hold on first sight.
    fn synchronized_output_holds(&mut self) -> bool {
        let active = match self.terminal.mode(Mode::SYNC_OUTPUT) {
            Ok(active) => active,
            Err(error) => {
                self.error("read synchronized output mode", error.to_string());
                false
            }
        };
        if !active {
            self.synchronized_since = None;
            return false;
        }
        let since = *self
            .synchronized_since
            .get_or_insert_with(std::time::Instant::now);
        since.elapsed() < SYNCHRONIZED_OUTPUT_TIMEOUT
    }

    /// Times synchronized updates per read batch, whether or not the batch
    /// can publish. A capture samples the mode only when credit allows, so an
    /// update that ends and restarts inside one batch, or across batches while
    /// the consumer owes credit, would otherwise read as one long update and
    /// lose its hold to the timeout under sustained output.
    fn observe_synchronized_output(&mut self, bytes: &[u8]) {
        const END: &[u8] = b"\x1b[?2026l";
        let ended = bytes.windows(END.len()).any(|window| window == END);
        // A read error is reported by the capture that consults the mode.
        let active = self.terminal.mode(Mode::SYNC_OUTPUT).unwrap_or(false);
        if ended || !active {
            self.synchronized_since = None;
        }
        // Nothing is published while away, so only a visible card times one.
        if active && self.visibility == Visibility::Visible {
            self.synchronized_since
                .get_or_insert_with(std::time::Instant::now);
        }
    }

    /// How long until the update holding back a dirty grid loses its hold.
    /// `None` once the hold has lapsed: from then on output and returned
    /// credit publish as usual, and a timer would only spin while the
    /// consumer still owes credit.
    fn synchronized_release_after(&self) -> Option<Duration> {
        let since = self.synchronized_since.filter(|_| self.capture_dirty)?;
        SYNCHRONIZED_OUTPUT_TIMEOUT.checked_sub(since.elapsed())
    }

    fn drain_effects(&mut self) {
        let effects = std::mem::take(&mut *self.effects.borrow_mut());
        let mut remaining = effects.ordered.into_iter();
        while let Some(effect) = remaining.next() {
            match effect {
                Effect::PtyWrite(bytes) => {
                    if let Err(bytes) = self.admit_pty_reply(bytes) {
                        if !self.reply_backpressured {
                            self.input_backpressure("write terminal response");
                            self.reply_backpressured = true;
                        }
                        let mut pending = self.effects.borrow_mut();
                        pending.push(Effect::PtyWrite(bytes));
                        for effect in remaining {
                            pending.push(effect);
                        }
                        return;
                    }
                }
                Effect::Bell => {
                    self.bell.pending = true;
                    self.publish_bell();
                }
                Effect::WorkingDirectoryChanged => {
                    let raw = self.terminal.pwd().unwrap_or_default();
                    let path = working_directory_path(raw, crate::file_url::host_name);
                    if let Some(path) = path {
                        self.emit(TerminalEvent::WorkingDirectoryChanged(path));
                        self.publish_fallback_title();
                    }
                }
                Effect::Title(title) => {
                    let title = if self.agent_provider == Some(crate::agent::Provider::Codex) {
                        crate::agent::codex_title_without_status(&title)
                    } else {
                        &title
                    };
                    let activity = self.agent_provider.and_then(|provider| {
                        provider
                            .activity_from_title(title)
                            .map(|state| crate::agent::Activity { provider, state })
                    });
                    // A shell's title that only says where it is gives way to
                    // the foreground command's, as having no title does.
                    let provider = self.agent_provider;
                    let mut located = if provider.is_none() {
                        self.foreground_title(Some(title))
                    } else {
                        None
                    };
                    let reported_changed = provider.is_none() && self.reported_title != title;
                    if reported_changed {
                        self.reported_title = title.to_owned();
                    }
                    let display_changed = match agent_title_label(title, activity) {
                        Some("") => self.display_title != "π",
                        Some(label) => self.display_title.strip_prefix("π ") != Some(label),
                        None => self.display_title != located.as_deref().unwrap_or(title),
                    };
                    let previous = *self
                        .published
                        .agent_activity
                        .lock()
                        .unwrap_or_else(|error| error.into_inner());
                    if display_changed || activity != previous || reported_changed {
                        self.refresh_resume_hint(true);
                    }
                    if reported_changed {
                        // A shell titles a command as it starts, which can be
                        // before the command holds the foreground: look again
                        // soon rather than at the settled interval.
                        self.foreground_poll = FOREGROUND_POLL_MIN;
                    }
                    if self.agent_provider != provider {
                        // The observation found an agent arriving or leaving,
                        // which decides whether this title is a location.
                        located = if self.agent_provider.is_none() {
                            self.foreground_title(Some(title))
                        } else {
                            None
                        };
                    }
                    let observed = self.agent_provider.and_then(|provider| {
                        provider
                            .activity_from_title(title)
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
                                .conversation_title(title, &process.launch.working_directory)
                        {
                            self.set_conversation_title(name.into());
                        }
                        let display =
                            located.unwrap_or_else(|| canonical_agent_title(title, observed));
                        if display != self.display_title {
                            self.display_title = display.clone();
                            self.emit(TerminalEvent::TitleChanged(display));
                        }
                    }
                }
            }
        }
        self.reply_backpressured = false;
    }

    /// OSC titles win, except a shell's own that only says where it is:
    /// fish's default abbreviates the whole path, which reads as noise
    /// beside the checkout every pane row already names. Shells that publish
    /// no title, or only that one, still expose the foreground command
    /// through the PTY owner's existing observation timer, which also
    /// catches a change of directory.
    fn publish_fallback_title(&mut self) {
        let reported = self
            .terminal
            .title()
            .ok()
            .filter(|title| !title.trim().is_empty())
            .map(str::to_owned);
        if reported.is_some() && self.agent_provider.is_some() {
            return;
        }
        let Some(title) = self.foreground_title(reported.as_deref()) else {
            return;
        };
        if title != self.display_title {
            self.display_title = title.clone();
            self.emit(TerminalEvent::TitleChanged(title));
        }
    }

    /// "command · directory" for the foreground group, unless `reported`,
    /// the shell's own title, says more than where the group is.
    fn foreground_title(&self, reported: Option<&str>) -> Option<String> {
        // Only a title holding a path can say where a shell is; any other
        // costs no /proc read.
        if reported.is_some_and(|title| !title.contains(['/', '~'])) {
            return None;
        }
        let group = self.foreground_group();
        let (mut process, mut directory) = foreground_process(group, &self.launch);
        let current = self.foreground_group();
        if current != group {
            // The group moved while it was read: read the one holding it now.
            (process, directory) = foreground_process(current, &self.launch);
        }
        if reported
            .is_some_and(|title| !title_locates_only(title, &directory, HOME_DIRECTORY.as_deref()))
        {
            return None;
        }
        Some(crate::names::process_title(&process, &directory))
    }

    fn publish_bell(&mut self) {
        if self.bell.pending && self.bell.take_ready(std::time::Instant::now()) {
            self.emit(TerminalEvent::Bell);
        }
    }

    /// Stops reading the terminal, takes in everything already read, and
    /// detaches its holder with the screen that output drew. A terminal that
    /// cannot be detached is read again; one whose reader would not stop is
    /// left to that reader.
    fn carry(&mut self, socket: &std::path::Path) -> Result<Carried, String> {
        self.settle_viewport_reads_for_handoff();
        let result = (|| {
            self.input_drained_for_handoff()?;
            self.refresh_resume_hint(true);
            self.stop_reading()?;
            self.input_drained_for_handoff()?;
            carry::screen(&mut self.terminal, holder::MAX_SCREEN)
                .and_then(|screen| self.holder.detach(socket, &screen))?;
            self.carried = true;
            self.complete_input_handoff();
            Ok(Carried {
                size: self.current_size,
                theme: self.theme.get(),
                hint: self.resume_hint.clone(),
            })
        })();
        if result.is_err() && self.input_handoff.is_none() {
            self.restart_reader_after_refusal();
        }
        result
    }

    /// Restore the same lossless reader ordering. Its existing bounded output
    /// channel remains the sole owner of all unread batches; never replace
    /// that receiver before the stopped reader is joined and it is drained.
    fn restart_reader(&mut self) -> Result<bool, String> {
        if self.reader.is_some() && self.reader_wake.is_some() {
            return Ok(true);
        }
        if self.reader.is_some() {
            if matches!(self.reader_done.try_recv(), Err(TryRecvError::Empty)) {
                return Ok(false);
            }
            if let Some(reader) = self.reader.take() {
                let _ = reader.join();
            }
        }
        self.drain_effects();
        if self.reply_pending() {
            return Ok(false);
        }
        // Actual join above means no producer can extend this fixed batch.
        for _ in 0..self.pty_rx.len() {
            match self.pty_rx.try_recv() {
                Ok(PtyMessage::Bytes(bytes)) => self.process_output(bytes),
                Ok(PtyMessage::Eof | PtyMessage::Error(_)) => {}
                Err(_) => break,
            }
            if self.reply_pending() {
                return Ok(false);
            }
        }
        let reader = Reader::start(
            self.terminal_id,
            &self.master,
            self.pty_capacity,
            Vec::new(),
        )?;
        self.reader = Some(reader.thread);
        self.reader_wake = Some(reader.wake);
        self.reader_done = reader.done;
        self.reader_discard_pending = reader.discard_pending;
        self.pty_rx = reader.output;
        self.pty_eof = false;
        Ok(true)
    }

    fn restart_reader_after_refusal(&mut self) {
        self.reader_restart_pending = true;
        let _ = self.resume_reader_after_refusal();
    }

    fn resume_reader_after_refusal(&mut self) -> bool {
        if !self.reader_restart_pending || self.input_handoff.is_some() {
            return false;
        }
        match self.restart_reader() {
            Ok(true) => {
                self.reader_restart_pending = false;
                let mut order = self
                    .published
                    .input_order
                    .lock()
                    .unwrap_or_else(|p| p.into_inner());
                if order.prepared_input.is_none() {
                    order.handoff = false;
                    order.cleanup_sealed = false;
                }
                true
            }
            Ok(false) => false,
            Err(error) => {
                self.reader_restart_pending = false;
                self.error("resume reading terminal", error);
                false
            }
        }
    }

    fn resume_cancelled_input_handoff(&mut self) -> bool {
        let Some(cut) = &self.input_handoff else {
            return false;
        };
        if cut.token.load(Ordering::Acquire) {
            return false;
        }
        match self.restart_reader() {
            Ok(true) => {}
            Ok(false) => return false,
            Err(error) => {
                if let Some(reply) = self
                    .input_handoff
                    .as_mut()
                    .and_then(|cut| cut.resumed.take())
                {
                    let _ = reply.send(Err(error));
                }
                return false;
            }
        }
        let cut = self.input_handoff.take().expect("matching cancelled cut");
        let mut order = self
            .published
            .input_order
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        if order
            .prepared_input
            .as_ref()
            .is_some_and(|token| Arc::ptr_eq(token, &cut.token))
        {
            order.prepared_input = None;
            order.handoff = false;
            order.cleanup_sealed = false;
        }
        drop(order);
        if let Some(reply) = cut.resumed {
            let _ = reply.send(Ok(()));
        }
        true
    }

    fn complete_input_handoff(&mut self) {
        if let Some(cut) = self.input_handoff.take() {
            let mut order = self
                .published
                .input_order
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            if order
                .prepared_input
                .as_ref()
                .is_some_and(|token| Arc::ptr_eq(token, &cut.token))
            {
                order.prepared_input = None;
            }
            drop(order);
            if let Some(reply) = cut.resumed {
                let _ = reply.send(Ok(()));
            }
        }
    }

    /// Stops the reader and takes in everything it read. A reader that will
    /// not stop may still read, which leaves the terminal to it.
    fn stop_reading(&mut self) -> Result<(), String> {
        if self.reply_pending() {
            return Err("Terminal reply is still pending; reader cut remains unconfirmed".into());
        }
        if self.reader.is_none() {
            return if self.pty_rx.is_empty() {
                Ok(())
            } else {
                Err("Terminal output is still pending; reader cut remains unconfirmed".into())
            };
        }
        drop(self.reader_wake.take());
        let deadline = std::time::Instant::now() + READER_SHUTDOWN_GRACE;
        let mut reading = true;
        while reading {
            if self.reply_pending() {
                return Err(
                    "Terminal reply is still pending; reader cut remains unconfirmed".into(),
                );
            }
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
        while !self.reply_pending() {
            match self.pty_rx.try_recv() {
                Ok(PtyMessage::Bytes(bytes)) => self.process_output(bytes),
                Ok(PtyMessage::Eof | PtyMessage::Error(_)) => {}
                Err(_) => break,
            }
        }
        if self.reply_pending() {
            return Err("Terminal reply is still pending; reader cut remains unconfirmed".into());
        }
        Ok(())
    }

    /// A viewport read waits for the next complete frame, which a handoff may
    /// never draw here. The daemon admits a snapshot only while its read is
    /// queued, so a read queued just before a handoff can still be waiting
    /// here, inside a synchronized update, when ownership starts to change.
    /// Draw the frame now, inside a synchronized update if need be, and refuse
    /// any read that still cannot be answered; a read changes nothing a
    /// handoff must preserve.
    fn settle_viewport_reads_for_handoff(&mut self) {
        if self.viewport_reads.is_empty() {
            return;
        }
        self.capture_when_ready(false);
        self.finish_viewport_reads();
        for (_, _, reply, _permit) in std::mem::take(&mut self.viewport_reads) {
            let _ = reply.send(Err(
                "Terminal ownership is changing; read its screen again".into()
            ));
        }
    }

    fn input_drained_for_handoff(&self) -> Result<(), String> {
        if !self.pending_input.is_empty()
            || self.reply_pending()
            || self.pending_input.has_held_input()
            || !self.viewport_reads.is_empty()
            || !self
                .published
                .cleanup_commands
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .is_empty()
            || !self.commands.is_empty()
            || !self.retained_commands.is_empty()
        {
            return Err(
                "Terminal input or control is still pending and cannot be checkpointed".into(),
            );
        }
        Ok(())
    }

    fn checked_close(
        &mut self,
        allow_active: bool,
        deadline: std::time::Instant,
    ) -> super::ipc::TerminalCloseResult {
        match self.begin_checked_close(allow_active, deadline, false) {
            CheckedClose::Finished(result) => result,
            CheckedClose::AwaitNaturalExit => unreachable!("startup close cannot defer its reader"),
        }
    }

    fn refuse_natural_close(&mut self) {
        if let Some(close) = self.natural_close.take() {
            self.published
                .input_order
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .handoff = false;
            let _ = close.reply.send(super::ipc::TerminalCloseResult::Unknown(
                "Terminal process cleanup remains unconfirmed".into(),
            ));
        }
    }

    fn begin_checked_close(
        &mut self,
        allow_active: bool,
        deadline: Instant,
        allow_natural_exit: bool,
    ) -> CheckedClose {
        use super::ipc::TerminalCloseResult;

        if Instant::now() >= deadline {
            return CheckedClose::Finished(TerminalCloseResult::Unknown(
                "Close inspection expired; stopped processes were released".into(),
            ));
        }

        // Fence new input at the shared admission lock. Reserved cleanup can
        // still balance a prior owned press while inspection is reversible;
        // it is sealed too before any destructive process signal. No mutex is
        // held during /proc inspection or process cleanup.
        let natural_exit = {
            let mut order = self
                .published
                .input_order
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            if order.handoff && !order.cleanup_sealed {
                return CheckedClose::Finished(TerminalCloseResult::Unknown(
                    "Terminal ownership is already changing".into(),
                ));
            }
            order.handoff = true;
            // A previous partial destructive close must retain its original
            // family proof; root exit alone cannot replace that obligation.
            allow_natural_exit && !order.cleanup_sealed && !self.checked_close_started
        };
        if natural_exit && self.child.exited_within(Duration::ZERO).unwrap_or(false) {
            return CheckedClose::AwaitNaturalExit;
        }
        let inspection = self.inspect_close(deadline);
        #[cfg(test)]
        if let Some((inspected, resume)) =
            self.published.close_inspection_gate.lock().unwrap().take()
        {
            inspected.send(()).unwrap();
            resume.recv_timeout(Duration::from_secs(5)).unwrap();
        }
        let outcome = match inspection {
            Ok((true, family)) if !allow_active => {
                // Thaw the inspected family before restoring input admission.
                drop(family);
                TerminalCloseResult::Active
            }
            Err(_) if natural_exit && self.child.exited_within(Duration::ZERO).unwrap_or(false) => {
                return CheckedClose::AwaitNaturalExit;
            }
            Err(detail) => TerminalCloseResult::Unknown(detail),
            Ok((_, mut family)) => {
                // Once kill is attempted, an error cannot prove the original
                // family is intact. Seal even reserved cleanup admission at
                // the same lock before the first destructive process signal.
                self.published
                    .input_order
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .cleanup_sealed = true;
                self.checked_close_started = true;
                return CheckedClose::Finished(match family.kill(deadline) {
                    Ok(()) => {
                        self.checked_shutdown = true;
                        self.holder.prove_cleanup();
                        TerminalCloseResult::Closed
                    }
                    Err(detail) => TerminalCloseResult::Unknown(detail),
                });
            }
        };
        self.published
            .input_order
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .handoff = false;
        CheckedClose::Finished(outcome)
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
            family.has_jobs()
                || foreground != root.process.group
                || !idle_shell
                || !self.pending_input.is_empty()
                || self.pending_input.has_held_input()
                || !self.commands.is_empty()
                || !self.retained_commands.is_empty()
                || self.published.interrupt_requested.load(Ordering::Acquire)
                || !self
                    .published
                    .cleanup_commands
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .is_empty(),
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
        self.publish_fallback_title();
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

    fn reply_pending(&self) -> bool {
        !self.effects.borrow().ordered.is_empty()
    }

    fn admit_pty_reply(&mut self, bytes: Vec<u8>) -> Result<(), Vec<u8>> {
        self.flush_pty_input();
        let Some(storage) = self.published.input_storage.reserve(bytes.len(), 1) else {
            return Err(bytes);
        };
        let permit = Some(admission::Permit::generated(storage));
        if !self.pending_input.admit_bytes(&bytes, false, permit) {
            return Err(bytes);
        }
        // A fatal/partial OS write is not an admission failure: its original
        // remaining frame is already owned by PendingInput, never replay it.
        self.flush_pty_input();
        Ok(())
    }

    fn write_pty(&mut self, bytes: &[u8], operation: &'static str) -> bool {
        self.write_bytes(bytes, operation, false)
    }

    fn write_user_input(&mut self, bytes: &[u8], operation: &'static str) -> bool {
        self.write_bytes(bytes, operation, true)
    }

    fn input_backpressure(&self, operation: &'static str) {
        self.input_notice(operation, format!(
            "Terminal is not reading input; {} KiB pending. Some input may already have been sent. Ctrl+C cancels input that has not been sent.",
            self.pending_input.len().div_ceil(1024),
        ));
    }

    fn write_bytes(&mut self, bytes: &[u8], operation: &'static str, user: bool) -> bool {
        self.flush_pty_input();
        if bytes.is_empty() {
            return !self.writer_failed;
        }
        let permit = if self
            .current_admission
            .as_ref()
            .is_some_and(|permit| permit.has_storage())
        {
            self.current_admission.clone()
        } else {
            let Some(storage) = self.published.input_storage.reserve(bytes.len(), 1) else {
                self.input_backpressure(operation);
                return false;
            };
            Some(admission::Permit::generated(storage))
        };
        if !self.pending_input.admit_bytes(bytes, user, permit) {
            self.input_backpressure(operation);
            return false;
        }
        self.flush_pty_input()
    }

    fn flush_pty_input(&mut self) -> bool {
        if self.natural_close.is_some() {
            // Parser replies can reach here while final output drains. Keep
            // older admitted user input fenced too until close settles.
            return false;
        }
        if self.writer_failed {
            return false;
        }
        let mut budget = PTY_BATCH_LIMIT;
        #[cfg(test)]
        if let Some(limit) = self.test_write_budget {
            budget = limit;
        }
        while !self.pending_input.is_empty() && budget > 0 {
            let mut ready = libc::pollfd {
                fd: self.writer.as_raw_fd(),
                events: libc::POLLOUT,
                revents: 0,
            };
            if unsafe { libc::poll(&mut ready, 1, 0) } <= 0
                || ready.revents & (libc::POLLOUT | libc::POLLERR | libc::POLLHUP) == 0
            {
                break;
            }
            let front = self.pending_input.front();
            match self.writer.write(&front[..front.len().min(budget)]) {
                Ok(0) => {
                    self.writer_failed = true;
                    self.error(
                        "write terminal input",
                        "PTY write returned zero bytes".into(),
                    );
                    return false;
                }
                Ok(written) => {
                    self.pending_input.consume(written);
                    budget -= written;
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(error) => {
                    self.writer_failed = true;
                    self.error("write terminal input", error.to_string());
                    return false;
                }
            }
        }
        true
    }

    #[cfg(test)]
    fn interrupt_input(&mut self) -> u64 {
        self.interrupt_input_with_carried(None)
    }

    fn interrupt_input_with_carried(&mut self, carried: Option<&Command>) -> u64 {
        let (boundary, mut key, interrupt_claim, _interrupt_ticket) = {
            let mut order = self
                .published
                .input_order
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            self.published
                .interrupt_requested
                .store(false, Ordering::Release);
            (
                order.generation,
                order.interrupt_key.take(),
                order.interrupt_claim.take(),
                order.interrupt_ticket.take(),
            )
        };
        if let Some(input) = &mut key
            && input.action == KeyAction::Repeat
            && !self.pending_input.has_key_owner(input)
        {
            input.action = KeyAction::Press;
        }
        let interrupt_key = key.clone();
        let encoded = match key {
            Some(key) => match encode_key_input(&self.terminal, &mut self.key_encoder, key) {
                Ok(encoded) => encoded,
                Err(error) => {
                    self.error("encode interrupt", error.to_string());
                    return boundary;
                }
            },
            None => vec![3],
        };
        let can_flush_kernel = self.pending_input.can_flush_kernel();
        let pending_before_cancel = self.pending_input.len();
        self.pending_input.cancel();
        // The loop drops its coalesced local owner after this call. Account
        // for that exact older-generation input without moving its custody.
        let mut discarded_unsent = self.pending_input.len() < pending_before_cancel
            || carried.is_some_and(|command| is_cancelable_input_before(command, boundary));
        self.retained_commands.retain(|command| {
            let keep = !is_cancelable_input_before(command, boundary);
            discarded_unsent |= !keep;
            keep
        });
        // A producer may keep filling the bounded inbox. Drain only the
        // commands present at this boundary so cancellation itself is bounded.
        for _ in 0..self.commands.len() {
            let Ok(command) = self.commands.try_recv() else {
                break;
            };
            if is_cancelable_input_before(&command, boundary) {
                discarded_unsent = true;
            } else {
                self.retained_commands.push_back(command);
            }
        }
        let mut attributes: libc::termios = unsafe { std::mem::zeroed() };
        // The master exposes its exact slave's line discipline. POLLOUT only
        // describes the flip queue: even a writable master can have a full
        // slave input buffer that strands a conventional interrupt byte.
        let conventional_interrupt =
            unsafe { libc::tcgetattr(self.writer.as_raw_fd(), &mut attributes) } == 0
                && attributes.c_lflag & libc::ISIG != 0
                && encoded.len() == 1
                && encoded[0] != 0
                && encoded[0] == attributes.c_cc[libc::VINTR];
        if can_flush_kernel {
            self.cancel_kernel_input();
        } else if !self.kernel_preservation_warned {
            self.kernel_preservation_warned = true;
            // A started protocol frame is deliberately preserved, not a
            // failed transport. Keep activity and the attachment live while
            // the original endings/releases remain ordered before new input.
            let message = if discarded_unsent {
                "Unsent input was canceled. Previously sent input was preserved."
            } else {
                "Previously sent input was preserved."
            };
            self.input_notice("cancel terminal input", message.into());
        }
        // The flush above may discard only the old cancelable kernel input.
        // Retry original blocked replies AFTER that flush, before a newer
        // interrupt frame can overtake them. Never flush these new replies.
        self.drain_effects();
        // Finish older owned releases before encoding a newly requested
        // interrupt press. A release of this not-yet-encoded interrupt waits
        // on its queued-press ticket and cannot run ahead of its own down.
        for _ in 0..admission::CLEANUP_LIMIT {
            let Some(command) = self.published.take_cleanup_command(true) else {
                break;
            };
            self.handle_command(command);
        }
        let _admitted = if conventional_interrupt {
            // TIOCSIG targets the foreground group bound to this exact PTY.
            // Raw applications and extended key encodings retain their bytes.
            let result =
                unsafe { libc::ioctl(self.writer.as_raw_fd(), libc::TIOCSIG, libc::SIGINT) };
            if result < 0 {
                self.error(
                    "interrupt terminal",
                    std::io::Error::last_os_error().to_string(),
                );
            }
            result >= 0
        } else if let Some(input) = interrupt_key {
            let mut release = input.clone();
            release.action = KeyAction::Release;
            release.text = None;
            let paired = encode_key_input(&self.terminal, &mut self.key_encoder, release)
                .is_ok_and(|bytes| !bytes.is_empty());
            let admitted = self.pending_input.admit_key(
                &input,
                &encoded,
                paired,
                self.current_admission.clone(),
                interrupt_claim.clone(),
            );
            if !admitted {
                self.input_backpressure("interrupt terminal");
            }
            admitted && self.flush_pty_input()
        } else {
            let previous = self
                .current_admission
                .replace(self.published.priority_permit.clone());
            let admitted = self.write_user_input(&encoded, "interrupt terminal");
            self.current_admission = previous;
            admitted
        };
        boundary
    }

    fn cancel_kernel_input(&self) {
        // The peer ioctl opens this exact slave without a reusable path or a
        // controlling-terminal change. Flushing TCIFLUSH on the master would
        // discard child output; flushing the peer discards canceled input.
        let peer = unsafe {
            libc::ioctl(
                self.writer.as_raw_fd(),
                libc::TIOCGPTPEER,
                libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC | libc::O_NONBLOCK,
            )
        };
        if peer < 0 {
            self.error(
                "cancel terminal input",
                std::io::Error::last_os_error().to_string(),
            );
            return;
        }
        // SAFETY: the successful ioctl returned a fresh owned descriptor.
        let peer = unsafe { OwnedFd::from_raw_fd(peer) };
        if unsafe { libc::tcflush(peer.as_raw_fd(), libc::TCIFLUSH) } < 0 {
            self.error(
                "cancel terminal input",
                std::io::Error::last_os_error().to_string(),
            );
        }
    }

    fn emit(&self, event: TerminalEvent) {
        self.published.emit_event(&self.events, event);
    }

    fn error(&self, operation: &'static str, message: String) {
        self.set_agent_activity(None);
        self.emit(TerminalEvent::Error { operation, message });
    }

    fn input_notice(&self, operation: &'static str, message: String) {
        self.emit(TerminalEvent::InputNotice { operation, message });
    }
}

/// The foreground group's command name and working directory, read from
/// /proc, each falling back to the launch's when it cannot be read.
fn foreground_process(group: Option<libc::pid_t>, launch: &LaunchRecipe) -> (String, PathBuf) {
    #[cfg(target_os = "linux")]
    let (process, directory) = match group.filter(|group| *group > 0) {
        Some(group) => {
            // The group comes from this PTY's tcgetpgrp. Two bounded reads
            // avoid a process-tree scan for ordinary shells.
            let process = std::fs::read_to_string(format!("/proc/{group}/comm"))
                .ok()
                .and_then(|name| crate::names::normalize_card_name(name.trim()));
            (
                process,
                std::fs::read_link(format!("/proc/{group}/cwd")).ok(),
            )
        }
        None => (None, None),
    };
    #[cfg(not(target_os = "linux"))]
    let (process, directory): (Option<String>, Option<PathBuf>) = {
        let _ = group;
        (None, None)
    };
    let process = process.unwrap_or_else(|| {
        std::path::Path::new(&launch.program)
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned()
    });
    (
        process,
        directory.unwrap_or_else(|| launch.working_directory.clone()),
    )
}

#[cfg(test)]
fn foreground_process_title(group: Option<libc::pid_t>, launch: &LaunchRecipe) -> String {
    let (process, directory) = foreground_process(group, launch);
    crate::names::process_title(&process, &directory)
}

/// The home directory a shell writes as `~`.
static HOME_DIRECTORY: std::sync::LazyLock<Option<PathBuf>> =
    std::sync::LazyLock::new(|| std::env::var_os("HOME").map(PathBuf::from));

/// Whether a shell's title only says where it is: fish's default
/// `prompt_pwd` path, bash's and zsh's `user@host: ~/dir`, or a full path,
/// possibly after the running command or a host. The path ending the title
/// must name `directory` itself, in full, under `~` for `home`, or with its
/// parents abbreviated as `prompt_pwd` does; a title naming anything else,
/// such as an editor's file, says more and is kept.
fn title_locates_only(
    title: &str,
    directory: &std::path::Path,
    home: Option<&std::path::Path>,
) -> bool {
    let title = title.trim_end();
    // A path starts the title or follows a space or a `host:` prefix; what
    // precedes it is the command or the host, and only the path decides.
    title
        .char_indices()
        .filter(|&(index, character)| {
            matches!(character, '/' | '~')
                && title[..index]
                    .chars()
                    .next_back()
                    .is_none_or(|before| before.is_whitespace() || before == ':')
        })
        .any(|(index, _)| path_names(&title[index..], directory, home))
}

/// Whether `path`, as a shell writes it, names `directory`.
fn path_names(path: &str, directory: &std::path::Path, home: Option<&std::path::Path>) -> bool {
    fn names(path: &std::path::Path) -> Option<Vec<&str>> {
        path.components()
            .filter_map(|component| match component {
                std::path::Component::Normal(name) => Some(name.to_str()),
                _ => None,
            })
            .collect()
    }
    let Some(actual) = names(directory) else {
        return false;
    };
    let (mut expected, rest) = match path.strip_prefix('~') {
        Some(rest) if rest.is_empty() || rest.starts_with('/') => {
            let Some(home) = home.and_then(names) else {
                return false;
            };
            (home, rest)
        }
        Some(_) => return false,
        None => (Vec::new(), path),
    };
    let fixed = expected.len();
    expected.extend(rest.split('/').filter(|name| !name.is_empty()));
    expected.len() == actual.len()
        && expected
            .iter()
            .zip(&actual)
            .enumerate()
            .all(|(index, (written, name))| {
                // Home and the directory's own name are written whole; the
                // parents between may be cut to a prefix, `.l` for `.local`.
                written == name
                    || (index >= fixed
                        && index + 1 < actual.len()
                        && !written.is_empty()
                        && name.starts_with(written))
            })
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
    if activity.is_none_or(|activity| activity.provider != crate::agent::Provider::Omp) {
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
    coalesce_scroll_command_with(command, commands, carried, || {})
}

fn coalesce_scroll_command_with(
    mut command: Command,
    commands: &Receiver<Command>,
    carried: &mut Option<Command>,
    mut folded: impl FnMut(),
) -> Command {
    fn delta(command: &Command) -> Option<isize> {
        match command {
            Command::Admitted { command, .. } => delta(command),
            Command::Scroll(ScrollCommand::Delta(delta)) => Some(*delta),
            _ => None,
        }
    }
    fn replace(command: &mut Command, value: isize) {
        match command {
            Command::Admitted { command, .. } => replace(command, value),
            Command::Scroll(ScrollCommand::Delta(delta)) => *delta = value,
            _ => unreachable!("coalesced command is a scroll delta"),
        }
    }
    let Some(mut value) = delta(&command) else {
        return command;
    };
    // Returning a folded command's admission credit lets a producer refill
    // the inbox. Bound this turn to the queue observed at entry so a wheel
    // flood cannot postpone priority interrupts or PTY output indefinitely.
    for _ in 0..commands.len() {
        let Ok(next) = commands.try_recv() else {
            break;
        };
        if let Some(next_delta) = delta(&next) {
            value = value.saturating_add(next_delta);
            drop(next);
            folded();
        } else {
            *carried = Some(next);
            break;
        }
    }
    replace(&mut command, value);
    command
}

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
    release_after: Option<Duration>,
) -> WorkerEvent {
    // Foreground observation only applies while the child runs; a withheld
    // synchronized update must be released either way. An unbounded wait is
    // `Duration::MAX`, which the channel treats as no deadline.
    let timer = |observe: Option<Duration>| {
        observe
            .into_iter()
            .chain(release_after)
            .min()
            .unwrap_or(Duration::MAX)
    };
    match (pty_open, child_running) {
        (true, true) => crossbeam_channel::select! {
            recv(commands) -> message => WorkerEvent::Command(message.ok()),
            recv(capture) -> _ => WorkerEvent::Capture,
            recv(pty) -> message => WorkerEvent::Pty(message.ok()),
            recv(child_exit) -> message => WorkerEvent::ChildExit(message.ok()),
            default(timer(Some(observe_after))) => WorkerEvent::Timer,
        },
        (true, false) => crossbeam_channel::select! {
            recv(commands) -> message => WorkerEvent::Command(message.ok()),
            recv(capture) -> _ => WorkerEvent::Capture,
            recv(pty) -> message => WorkerEvent::Pty(message.ok()),
            default(timer(None)) => WorkerEvent::Timer,
        },
        (false, true) => crossbeam_channel::select! {
            recv(commands) -> message => WorkerEvent::Command(message.ok()),
            recv(capture) -> _ => WorkerEvent::Capture,
            recv(child_exit) -> message => WorkerEvent::ChildExit(message.ok()),
            default(timer(Some(observe_after))) => WorkerEvent::Timer,
        },
        (false, false) => crossbeam_channel::select! {
            recv(commands) -> message => WorkerEvent::Command(message.ok()),
            // A stable cut can outlive its child's exit report. Its exact
            // cancellation still has to wake/restart the original reader.
            recv(capture) -> _ => WorkerEvent::Capture,
            default(timer(None)) => WorkerEvent::Timer,
        },
    }
}

/// A root exit alone cannot release the terminal family. Capture its exact
/// jobs before HUP, preserve the shell's graceful exit, and require cleanup
/// before accepting the holder's report.
fn shutdown_child_exit(
    child: &holder::HeldChild,
    master: &holder::PtyMaster,
    identity: Option<&RootIdentity>,
    exits: &Receiver<ChildExit>,
    checked: bool,
    budget: ShutdownBudget,
) -> ChildExit {
    let deadline = Instant::now() + budget.deadline;
    let report = exits.try_recv().ok();
    if !checked {
        // Capture must leave both the full HUP grace and time to confirm KILL.
        let kill_deadline = deadline - budget.confirmation_reserve;
        let capture_deadline = kill_deadline - budget.hangup_grace;
        let captured = identity
            .ok_or_else(|| "Child identity unavailable".to_owned())
            .and_then(|root| FrozenFamily::capture(root, capture_deadline));
        match captured {
            Ok(mut family) => {
                if let Err(error) = family.hangup(child) {
                    tracing::warn!(%error, "could not hang up the complete terminal family");
                }
                let grace = (Instant::now() + budget.hangup_grace).min(deadline);
                if !family.wait_until(grace).unwrap_or(false) {
                    if !child.exited_within(Duration::ZERO).unwrap_or(false) {
                        // Re-census while the exact root still lives, retaining
                        // the first set even if the shell exits during capture.
                        let stopped = FrozenFamily::capture(
                            identity.expect("captured root identity"),
                            kill_deadline,
                        )
                        .and_then(|mut current| current.kill_survivors());
                        if let Err(error) = stopped {
                            tracing::warn!(%error, "could not stop terminal family; escalating only the pinned root");
                            if let Err(error) = child.force_kill() {
                                tracing::warn!(%error, "could not kill the pinned terminal root");
                            }
                        }
                    }
                    // Retained pidfd authority does not expire with census
                    // time. Issue the finite survivor signals even if the
                    // re-census consumed the remaining wait budget.
                    if let Err(error) = family.kill_survivors() {
                        tracing::warn!(%error, "could not stop the complete terminal family");
                    }
                    if !family.wait_until(deadline)? {
                        return Err("Terminal process cleanup remains unconfirmed".into());
                    }
                }
            }
            Err(error) => {
                // Failed capture resumes its stops on drop. Never let a
                // process-scan failure discard the independently pinned root.
                if !child
                    .exited_within(Duration::ZERO)
                    .map_err(|error| format!("observe child exit: {error}"))?
                {
                    tracing::warn!(%error, "could not stop terminal family; escalating only the pinned root");
                    if let Err(error) = child.kill() {
                        tracing::warn!(%error, "could not hang up the pinned terminal root");
                    }
                    if !child
                        .exited_within(
                            budget
                                .hangup_grace
                                .min(deadline.saturating_duration_since(Instant::now())),
                        )
                        .map_err(|error| format!("observe child exit: {error}"))?
                    {
                        child
                            .force_kill()
                            .map_err(|error| format!("terminate child: {error}"))?;
                    }
                }
                if !child
                    .exited_within(deadline.saturating_duration_since(Instant::now()))
                    .map_err(|error| format!("observe child exit: {error}"))?
                {
                    return Err("Terminal process cleanup remains unconfirmed".into());
                }
            }
        }
        confirm_exited_family(child, master, deadline, budget.census_reserve)?;
    }
    if let Some(report) = report {
        return report;
    }
    exits
        .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        .map_err(|error| match error {
            RecvTimeoutError::Disconnected => "child reaper closed".to_owned(),
            RecvTimeoutError::Timeout => {
                "terminal shutdown did not receive a confirmed child exit before its deadline"
                    .to_owned()
            }
        })?
}

fn confirm_exited_family(
    child: &holder::HeldChild,
    master: &holder::PtyMaster,
    deadline: Instant,
    census_reserve: Duration,
) -> Result<(), String> {
    let ended = |deadline| -> Result<bool, String> {
        Ok(child
            .exited_within(Duration::ZERO)
            .map_err(|error| format!("observe child exit: {error}"))?
            && close_processes::session_is_empty(child.pid(), deadline)?)
    };
    // Usually the family is already gone. A process outside it (one that left
    // the session, such as `setsid -f`) may keep the PTY open indefinitely;
    // cleanup never claims or depends on it, so its PTY is no reason to wait.
    // A first census that fails or runs late defers to the final one below.
    let drained = deadline.checked_sub(census_reserve).unwrap_or(deadline);
    if ended(drained).unwrap_or(false) {
        return Ok(());
    }
    // A job can fork between thaw and HUP, outside the tracked set. Give the
    // session's last members the bounded budget to finish, ending early once
    // nothing holds the PTY, and keep the final census its own time.
    let master = master.duplicate().map_err(|error| error.to_string())?;
    let mut target = libc::pollfd {
        fd: master.as_raw_fd(),
        events: 0,
        revents: 0,
    };
    loop {
        let remaining = drained.saturating_duration_since(Instant::now());
        // SAFETY: one live, owned descriptor; this only observes readiness.
        match unsafe {
            libc::poll(
                &mut target,
                1,
                remaining
                    .as_nanos()
                    .div_ceil(1_000_000)
                    .min(i32::MAX as u128) as libc::c_int,
            )
        } {
            -1 if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted => {}
            -1 => return Err(std::io::Error::last_os_error().to_string()),
            _ => break,
        }
    }
    // A final census that cannot finish, for instance one a loaded host's
    // scan of /proc runs past the deadline, confirms nothing: the root has
    // exited and nothing was stopped, so report the open obligation itself.
    match ended(deadline) {
        Ok(true) => Ok(()),
        Ok(false) => Err("Terminal process cleanup remains unconfirmed".into()),
        Err(error) => {
            tracing::warn!(%error, "could not confirm the terminal family's cleanup");
            Err("Terminal process cleanup remains unconfirmed".into())
        }
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

/// Drops every inherited marker from a command's environment, and the
/// identity of any provider session the host was started from.
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
    let sessions = command
        .iter_full_env_as_str()
        .map(|(name, _)| name)
        .filter(|name| crate::agent::is_session_variable(name))
        .map(str::to_owned)
        .collect::<Vec<_>>();
    for name in sessions {
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
    // Last, so an explicit per-session entry still wins over our defaults. A
    // recipe saved before parent sessions were withheld may still name one.
    for (name, value) in &recipe.environment {
        if !crate::agent::is_session_variable(name) {
            command.env(name, value);
        }
    }
    for name in crate::account::launch_credential_overrides(recipe) {
        command.env_remove(name);
    }
    command
}

/// A returned conversation's exact execution environment, in place of the
/// host's own.
fn command_from_execution(
    execution: &LaunchRecipe,
    environment: &crate::terminal_ipc::ChatExecutionEnvironment,
) -> CommandBuilder {
    let mut command = CommandBuilder::new(&execution.program);
    command.args(&execution.arguments);
    command.cwd(&execution.working_directory);
    command.env_clear();
    for (name, value) in &environment.0 {
        if !execution.unset_environment.contains(name) {
            command.env(name, value);
        }
    }
    scrub_inherited_markers(&mut command);
    terminal_environment(&mut command);
    command
}

fn terminal_environment(command: &mut CommandBuilder) {
    // We ship no private terminfo entry; advertise the emulator we implement.
    command.env("TERM", "xterm-256color");
    command.env("COLORTERM", "truecolor");
    command.env("TERM_PROGRAM", "cagents");
    command.env("TERM_PROGRAM_VERSION", crate::version());
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

fn read_pty(
    reader: &mut dyn Read,
    sender: &Sender<PtyMessage>,
    readiness: OwnedFd,
    wake: OwnedFd,
    discard_pending: &AtomicBool,
) {
    let mut buffer = vec![0u8; PTY_READ_SIZE];
    loop {
        match wait_readable(&readiness, &wake) {
            Readiness::Readable => {}
            Readiness::Shutdown => return,
            Readiness::Failed(error) => {
                send_pty_message(sender, PtyMessage::Error(error), &wake, discard_pending);
                return;
            }
        }
        match reader.read(&mut buffer) {
            Ok(0) => {
                send_pty_message(sender, PtyMessage::Eof, &wake, discard_pending);
                return;
            }
            // A PTY master reads EIO, not end of file, once its terminal's
            // last slave closes: the ordinary end of a terminal.
            Err(error) if error.raw_os_error() == Some(libc::EIO) => {
                send_pty_message(sender, PtyMessage::Eof, &wake, discard_pending);
                return;
            }
            Ok(read) => {
                if !send_pty_message(
                    sender,
                    PtyMessage::Bytes(buffer[..read].to_vec()),
                    &wake,
                    discard_pending,
                ) {
                    return;
                }
            }
            // A duplicate shares file-status flags with the writer. If that
            // owner uses nonblocking I/O, another read/line-discipline change
            // can consume readiness before this read. Re-arm the original
            // descriptor instead of treating EAGAIN as terminal failure.
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::Interrupted | std::io::ErrorKind::WouldBlock
                ) => {}
            Err(error) => {
                send_pty_message(
                    sender,
                    PtyMessage::Error(error.to_string()),
                    &wake,
                    discard_pending,
                );
                return;
            }
        }
    }
}

fn send_pty_message(
    sender: &Sender<PtyMessage>,
    mut message: PtyMessage,
    wake: &OwnedFd,
    discard_pending: &AtomicBool,
) -> bool {
    loop {
        // Carry must deliver the already-read message even if checking a
        // cancellation descriptor fails. Only destructive teardown permits
        // discarding it; ordinary delivery needs no extra poll syscall.
        if discard_pending.load(Ordering::Acquire) {
            let mut cancellation = libc::pollfd {
                fd: wake.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            if unsafe { libc::poll(&mut cancellation, 1, 0) } < 0 {
                if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return false;
            }
            if cancellation.revents != 0 {
                return false;
            }
        }
        match sender.send_timeout(message, READER_SEND_POLL) {
            Ok(()) => return true,
            Err(crossbeam_channel::SendTimeoutError::Timeout(pending)) => message = pending,
            Err(crossbeam_channel::SendTimeoutError::Disconnected(_)) => return false,
        }
    }
}

struct TranscriptWriter {
    location: Option<crate::transcript_location::ValidatedTranscript>,
    sender: Sender<TranscriptMessage>,
    /// Published by the writer once the size cap is reached, so the worker can
    /// stop handing over batches the writer would only drop.
    capped: Arc<AtomicBool>,
    worker: JoinHandle<()>,
}

impl TranscriptWriter {
    fn start(
        path: PathBuf,
        terminal: TerminalId,
        capacity: usize,
        max_bytes: u64,
    ) -> Result<Self, String> {
        let file = open_private_append_file(&path)
            .map_err(|error| format!("open transcript {}: {error}", path.display()))?;
        let location = crate::transcript_location::capture(terminal, &path, &file);
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
                    // A killed host never reaches the final flush, and its
                    // transcript is what a person has left. A busy channel
                    // still coalesces, and this thread is off the PTY path.
                    if receiver.is_empty() && writer.flush().is_err() {
                        break;
                    }
                }
                let _ = writer.flush();
            })
            .map_err(|error| format!("start transcript writer: {error}"))?;
        Ok(Self {
            location,
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
    // Use the same checked bound reserved before Handle admission. The pinned
    // wrapper retries a short buffer only once; allocating the proved bound
    // also handles large associated-text events across negotiated modes.
    let capacity = key_storage_bound(&input).ok_or(libghostty_vt::Error::OutOfSpace {
        required: usize::MAX,
    })?;
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
    let mut encoded = Vec::with_capacity(capacity);
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

/// `local_host` is asked only when the URL names a host; see
/// [`crate::file_url::localize`].
fn working_directory_path<S: AsRef<str>>(
    raw: &str,
    local_host: impl FnOnce() -> Option<S>,
) -> Option<PathBuf> {
    if let Ok(mut uri) = url::Url::parse(raw) {
        if uri.scheme() != "file" {
            return None;
        }
        crate::file_url::localize(&mut uri, local_host);
        uri.to_file_path().ok()
    } else {
        let path = PathBuf::from(raw);
        path.is_absolute().then_some(path)
    }
}

#[cfg(test)]
mod tests;
