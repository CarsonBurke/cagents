use std::{
    collections::BTreeMap,
    io,
    net::Shutdown,
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
    sync::{
        Arc, Condvar, Mutex, OnceLock,
        atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use super::daemon_start::{HostedTerminalStartError, ensure_daemon_ready};
use arc_swap::{ArcSwap, ArcSwapOption};
use crossbeam_channel::{Receiver, Sender, TryRecvError, TrySendError};
#[cfg(test)]
use std::process::Command;

use crate::model::{HostSessionProof, TerminalId, TerminalSession};

use super::{
    KeyInput, MouseInput, PasteOrigin, RenderSnapshot, TerminalEvent, TerminalSendError,
    TerminalSize, TerminalWake,
    ipc::{
        ClientEnvelope, ClientRequest, FrameCodec, FrameError, MAX_FRAME_BYTES, MAX_INPUT_BYTES,
        PROTOCOL_VERSION, ProtocolError, ProtocolErrorCode, ResumeCursor, ServerEnvelope,
        ServerMessage, ServerResponse, TerminalScroll, TerminalSnapshot, TerminalSpawnOptions,
        TerminalVisibility, WireTerminalSize, write_frame,
    },
};

// A last-frame placeholder only covers rapid navigation and the reveal
// round trip. Cold terminals must release it even when no new output arrives.
const RETAINED_FRAME_TTL: Duration = Duration::from_secs(5);
const COMMAND_CAPACITY: usize = 256;
const EVENT_CAPACITY: usize = 256;
const MAX_IN_FLIGHT_REQUESTS: usize = 64;
// Used only while authenticating, before the terminal is attached or accepts
// input. Connected clients are wake-driven by `wait_for_client_work`.
const HANDSHAKE_POLL_INTERVAL: Duration = Duration::from_millis(100);
const INITIAL_RECONNECT_DELAY: Duration = Duration::from_millis(50);
const MAX_RECONNECT_DELAY: Duration = Duration::from_secs(1);
const DAEMON_READY_TIMEOUT: Duration = Duration::from_secs(5);

type WakeCallback = dyn Fn(TerminalWake) + Send + Sync + 'static;

struct WorkerStop {
    requested: AtomicBool,
    async_wake: tokio::sync::Notify,
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
            async_wake: tokio::sync::Notify::new(),
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
        self.async_wake.notify_one();
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

struct RetainedFrame {
    snapshot: Arc<RenderSnapshot>,
    expires: Instant,
}

struct HostedPublishedState {
    snapshot: ArcSwap<RenderSnapshot>,
    /// The last grid a hibernated view showed.
    ///
    /// Hibernating gives the grid up as a delta base: the daemon releases the
    /// view's visuals, so `snapshot` becomes a detached marker and the next
    /// frame is asked for whole. The picture itself is kept here, so a pane
    /// that comes back shows what it last showed until that frame lands
    /// rather than an empty rectangle. The worker releases it after a short
    /// grace, or immediately when a live frame replaces it.
    retained: ArcSwapOption<RetainedFrame>,
    generation: AtomicU64,
    event_sequence: AtomicU64,
    activity: Mutex<Option<crate::model::TerminalOutputActivity>>,
    agent_activity: Mutex<Option<crate::agent::Activity>>,
    activity_current: AtomicBool,
    acknowledged_generation: AtomicU64,
    acknowledgement_tx: Sender<()>,
    work_available: tokio::sync::Notify,
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
            retained: ArcSwapOption::empty(),
            generation: AtomicU64::new(0),
            event_sequence: AtomicU64::new(0),
            activity: Mutex::new(None),
            agent_activity: Mutex::new(None),
            activity_current: AtomicBool::new(false),
            acknowledged_generation: AtomicU64::new(0),
            acknowledgement_tx,
            work_available: tokio::sync::Notify::new(),
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
        let live = !snapshot.detached;
        self.snapshot.store(Arc::new(snapshot));
        if live {
            self.retained.store(None);
        }
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
        // The terminal could not be attached, so there is no next frame for
        // an old picture to stand in for.
        self.retained.store(None);
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
        self.hibernate_at(Instant::now());
    }

    fn hibernate_at(&self, now: Instant) {
        self.set_visibility(TerminalVisibility::Hibernated);
        let generation = self.generation.load(Ordering::Acquire);
        // Kept before the marker replaces it, so no draw falls between the
        // two and finds neither. A view hibernated again before its next
        // frame arrived keeps the picture it already had.
        let last = self.snapshot.load_full();
        if !last.detached && last.rows > 0 {
            self.retained.store(Some(Arc::new(RetainedFrame {
                snapshot: Arc::clone(&last),
                expires: now + RETAINED_FRAME_TTL,
            })));
            // A failed attach replaces the grid and then drops the picture.
            // If it did the first before the store above, it did the second
            // before it too, and the picture must not outlive it.
            if !Arc::ptr_eq(&last, &self.snapshot.load_full()) {
                self.retained.store(None);
            }
        }
        self.snapshot
            .store(Arc::new(RenderSnapshot::detached(generation)));
        // This channel is a data-free worker wake. The acknowledgement
        // watermark is unchanged, so this does not return extra redraw credit.
        // Wake after publication even if Hibernate was already drained before
        // the UI stored the placeholder and its expiration deadline.
        let _ = self.acknowledgement_tx.try_send(());
        self.work_available.notify_one();
    }

    fn expire_retained(&self, now: Instant) {
        if self
            .retained
            .load()
            .as_ref()
            .is_none_or(|frame| frame.expires > now)
        {
            return;
        }
        self.retained.rcu(|retained| {
            retained
                .as_ref()
                .filter(|frame| frame.expires > now)
                .cloned()
        });
    }

    fn retained_expiration(&self) -> Receiver<Instant> {
        match self.retained.load().as_ref() {
            Some(frame) => {
                crossbeam_channel::after(frame.expires.saturating_duration_since(Instant::now()))
            }
            None => crossbeam_channel::never(),
        }
    }

    /// What the view draws: the live grid, or the last one this terminal
    /// showed while a hibernated view waits for its next.
    fn frame(&self) -> Arc<RenderSnapshot> {
        let snapshot = self.snapshot.load_full();
        if snapshot.detached
            && let Some(retained) = self.retained.load_full()
            && retained.expires > Instant::now()
        {
            return Arc::clone(&retained.snapshot);
        }
        snapshot
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
            self.work_available.notify_one();
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
    ClearScrollback,
    CopySelection {
        request: uuid::Uuid,
        selection: super::SearchMatch,
    },
    Search {
        request: u64,
        query: String,
        case_sensitive: bool,
    },
    Visibility(TerminalVisibility),
    AcknowledgeOutput(u64),
    Hibernate,
    Resume {
        visible: bool,
    },
    ShutdownTerminal(Sender<()>),
    Stop,
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
            Self::ClearScrollback => "clear terminal scrollback",
            Self::CopySelection { .. } => "copy terminal selection",
            Self::Search { .. } => "search terminal",
            Self::Visibility(_) => "set terminal visibility",
            Self::AcknowledgeOutput(_) => "acknowledge terminal output",
            Self::Hibernate => "hibernate terminal",
            Self::Resume { .. } => "resume terminal",
            Self::ShutdownTerminal(_) => "shutdown terminal",
            Self::Stop => "detach terminal",
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
            Self::ClearScrollback => ClientRequest::ClearTerminalScrollback { terminal_id },
            Self::CopySelection { request, selection } => ClientRequest::CopyTerminalSelection {
                terminal_id,
                request,
                selection,
            },
            Self::Search {
                request,
                query,
                case_sensitive,
            } => ClientRequest::SearchTerminal {
                terminal_id,
                request,
                query,
                case_sensitive,
            },
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
        }
    }
}

struct HostedHandleInner {
    terminal_id: TerminalId,
    commands: Sender<ClientCommand>,
    events: Receiver<TerminalEvent>,
    published: Arc<HostedPublishedState>,
    stop: Arc<WorkerStop>,
    worker: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl Drop for HostedHandleInner {
    fn drop(&mut self) {
        self.stop.request();
        let _ = self.commands.try_send(ClientCommand::Stop);
        if let Ok(worker) = self.worker.get_mut()
            && let Some(worker) = worker.take()
        {
            worker.abort();
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
        let runtime = super::transport::runtime()
            .map_err(|error| HostedTerminalStartError::Worker(io::Error::other(error)))?;
        let worker = runtime.spawn(async move {
            client_loop(
                &socket,
                terminal_id,
                intent,
                command_rx,
                event_tx,
                worker_published,
                worker_stop,
            )
            .await;
        });

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

    /// The grid to draw. Unlike [`Self::snapshot`], a hibernated terminal
    /// answers with the last grid it showed instead of a detached marker, so
    /// its pane is not blank while the next frame is on its way. Everything
    /// that reads the terminal's current state still asks `snapshot`.
    pub fn frame(&self) -> Arc<RenderSnapshot> {
        self.inner.published.frame()
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

    pub fn scroll_to_match(&self, range: super::SearchMatch) -> Result<(), TerminalSendError> {
        self.try_command(ClientCommand::Scroll(TerminalScroll::Match(range)))
    }

    pub fn set_theme(&self, theme: super::TerminalTheme) -> Result<(), TerminalSendError> {
        self.try_command(ClientCommand::SetTheme(theme))
    }

    pub fn search(
        &self,
        request: u64,
        query: String,
        case_sensitive: bool,
    ) -> Result<(), TerminalSendError> {
        self.try_command(ClientCommand::Search {
            request,
            query,
            case_sensitive,
        })
    }
    pub fn clear_scrollback(&self) -> Result<(), TerminalSendError> {
        self.try_command(ClientCommand::ClearScrollback)
    }
    pub fn copy_selection(
        &self,
        request: uuid::Uuid,
        selection: super::SearchMatch,
    ) -> Result<(), TerminalSendError> {
        self.try_command(ClientCommand::CopySelection { request, selection })
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
        // Stop can race the command's publication. A terminated worker cannot
        // service the expiration, so do not leave a placeholder behind it.
        if self.inner.stop.is_requested() {
            self.inner.published.retained.store(None);
        }
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
            })?;
        self.inner.published.work_available.notify_one();
        Ok(())
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

/// Socket reads share the transport reactor. A bounded inbox and explicit
/// read credit preserve backpressure without parking one OS thread per pane.
struct ConnectionIo {
    writer: Arc<tokio::io::unix::AsyncFd<UnixStream>>,
    codec: FrameCodec,
    incoming: Option<Receiver<ServerRead>>,
    credit: Arc<tokio::sync::Notify>,
    reader: Option<tokio::task::JoinHandle<()>>,
}

impl ConnectionIo {
    fn new(
        writer: Arc<tokio::io::unix::AsyncFd<UnixStream>>,
        published: Arc<HostedPublishedState>,
    ) -> io::Result<Self> {
        let (incoming_tx, incoming_rx) = crossbeam_channel::bounded(EVENT_CAPACITY);
        let socket = Arc::clone(&writer);
        let credit = Arc::new(tokio::sync::Notify::new());
        let read_credit = Arc::clone(&credit);
        let reader = super::transport::runtime()
            .map_err(io::Error::other)?
            .spawn(async move {
                let mut codec = FrameCodec::new(MAX_FRAME_BYTES);
                loop {
                    let mut message =
                        match super::transport::read::<ServerEnvelope>(&socket, &mut codec).await {
                            Ok(Some(envelope)) => ServerRead::Envelope(Box::new(envelope)),
                            Ok(None) | Err(_) => ServerRead::Closed,
                        };
                    let closed = matches!(message, ServerRead::Closed);
                    loop {
                        match incoming_tx.try_send(message) {
                            Ok(()) => {
                                published.work_available.notify_one();
                                break;
                            }
                            Err(TrySendError::Disconnected(_)) => return,
                            Err(TrySendError::Full(pending)) => message = pending,
                        }
                        read_credit.notified().await;
                    }
                    if closed {
                        return;
                    }
                    // A peer with buffered snapshots must return the shared I/O
                    // executor between frames, even while every read is ready.
                    tokio::task::yield_now().await;
                }
            });
        Ok(Self {
            writer,
            codec: FrameCodec::new(MAX_FRAME_BYTES),
            incoming: Some(incoming_rx),
            credit,
            reader: Some(reader),
        })
    }
    fn writer(&mut self) -> (&tokio::io::unix::AsyncFd<UnixStream>, &mut FrameCodec) {
        (&self.writer, &mut self.codec)
    }
    fn incoming(&self) -> &Receiver<ServerRead> {
        self.incoming.as_ref().expect("inbox until close")
    }
    fn close(&mut self) {
        self.incoming.take();
        let _ = self.writer.get_ref().shutdown(Shutdown::Both);
        if let Some(reader) = self.reader.take() {
            reader.abort();
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
    RetainedExpired,
    Command(ClientCommand),
    Server(Box<ServerEnvelope>),
    CommandsClosed,
    ServerClosed,
}

#[cfg(test)]
fn wait_for_client_work(
    stop: &WorkerStop,
    commands: &Receiver<ClientCommand>,
    incoming: &Receiver<ServerRead>,
    published: &HostedPublishedState,
) -> ClientWork {
    let expiration = published.retained_expiration();
    crossbeam_channel::select_biased! {
        recv(stop.notification()) -> _ => ClientWork::Stop,
        recv(published.acknowledgement_rx) -> _ => ClientWork::Acknowledge,
        recv(expiration) -> _ => ClientWork::RetainedExpired,
        recv(commands) -> command => match command {
            Ok(command) => ClientWork::Command(command),
            Err(_) => ClientWork::CommandsClosed,
        },
        recv(incoming) -> message => server_work(message),
    }
}

#[cfg(test)]
fn wait_for_server_or_stop(
    stop: &WorkerStop,
    incoming: &Receiver<ServerRead>,
    published: &HostedPublishedState,
) -> ClientWork {
    let expiration = published.retained_expiration();
    crossbeam_channel::select_biased! {
        recv(stop.notification()) -> _ => ClientWork::Stop,
        recv(published.acknowledgement_rx) -> _ => ClientWork::Acknowledge,
        recv(expiration) -> _ => ClientWork::RetainedExpired,
        recv(incoming) -> message => server_work(message),
    }
}

fn server_work(message: Result<ServerRead, crossbeam_channel::RecvError>) -> ClientWork {
    match message {
        Ok(ServerRead::Envelope(envelope)) => ClientWork::Server(envelope),
        Ok(ServerRead::Closed) | Err(_) => ClientWork::ServerClosed,
    }
}

async fn wait_for_work_async(
    stop: &WorkerStop,
    commands: Option<&Receiver<ClientCommand>>,
    incoming: &Receiver<ServerRead>,
    published: &HostedPublishedState,
    credit: &tokio::sync::Notify,
) -> ClientWork {
    loop {
        let changed = published.work_available.notified();
        let stopping = stop.async_wake.notified();
        tokio::pin!(changed, stopping);
        changed.as_mut().enable();
        stopping.as_mut().enable();
        if stop.is_requested() {
            return ClientWork::Stop;
        }
        if published.acknowledgement_rx.try_recv().is_ok() {
            return ClientWork::Acknowledge;
        }
        let expires = published
            .retained
            .load()
            .as_ref()
            .map(|frame| frame.expires);
        if expires.is_some_and(|deadline| deadline <= Instant::now()) {
            return ClientWork::RetainedExpired;
        }
        if let Some(commands) = commands {
            match commands.try_recv() {
                Ok(command) => return ClientWork::Command(command),
                Err(TryRecvError::Disconnected) => return ClientWork::CommandsClosed,
                Err(TryRecvError::Empty) => {}
            }
        }
        match incoming.try_recv() {
            Ok(message) => {
                credit.notify_one();
                return server_work(Ok(message));
            }
            Err(TryRecvError::Disconnected) => return ClientWork::ServerClosed,
            Err(TryRecvError::Empty) => {}
        }
        let expiration = async {
            match expires {
                Some(deadline) => {
                    tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)).await
                }
                None => std::future::pending::<()>().await,
            }
        };
        tokio::select! { biased;
            _ = stopping => {},
            _ = changed => {},
            _ = expiration => {},
        }
    }
}

struct ConnectionContext<'a> {
    socket: &'a Path,
    terminal_id: TerminalId,
    intent: &'a ConnectionIntent,
    commands: &'a Receiver<ClientCommand>,
    events: &'a Sender<TerminalEvent>,
    published: &'a Arc<HostedPublishedState>,
    stop: &'a WorkerStop,
}

async fn client_loop(
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
        published.expire_retained(Instant::now());
        if stop.is_requested() {
            break;
        }
        let end = serve_connection(&connection, &mut carried, || {
            reconnect_started = None;
            reconnect_delay = INITIAL_RECONNECT_DELAY;
        })
        .await;
        published.invalidate_activity();
        let _ = published.observe_agent_activity(&events, None);
        match end {
            ConnectionEnd::Stop => break,
            ConnectionEnd::Retry => {
                let started = reconnect_started.get_or_insert_with(Instant::now);
                if started.elapsed() >= DAEMON_READY_TIMEOUT {
                    reject_handshake_async(
                        &events,
                        &published,
                        &stop,
                        ProtocolError::new(
                            ProtocolErrorCode::InvalidRequest,
                            "daemon connection was lost; rediscovery is required before this terminal can continue",
                            false,
                        ),
                    ).await;
                    break;
                }
            }
        }

        if wait_before_reconnect_async(&stop, &commands, &published, &mut carried, reconnect_delay)
            .await
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
    published.retained.store(None);
    published.fail_attachment("hosted terminal connection stopped".to_owned());
}

#[cfg(test)]
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
async fn wait_before_reconnect_async(
    stop: &WorkerStop,
    commands: &Receiver<ClientCommand>,
    published: &HostedPublishedState,
    carried: &mut Option<ClientCommand>,
    delay: Duration,
) -> ConnectionEnd {
    let deadline = tokio::time::Instant::now() + delay;
    loop {
        let changed = published.work_available.notified();
        let stopping = stop.async_wake.notified();
        tokio::pin!(changed, stopping);
        changed.as_mut().enable();
        stopping.as_mut().enable();
        if stop.is_requested() {
            return ConnectionEnd::Stop;
        }
        if carried.is_none() {
            match commands.try_recv() {
                Ok(ClientCommand::Stop) | Err(TryRecvError::Disconnected) => {
                    return ConnectionEnd::Stop;
                }
                Ok(command) => {
                    *carried = Some(command);
                    return ConnectionEnd::Retry;
                }
                Err(TryRecvError::Empty) => {}
            }
        }
        tokio::select! { biased;
            _ = stopping => {},
            _ = changed => {},
            _ = tokio::time::sleep_until(deadline) => return ConnectionEnd::Retry,
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

async fn serve_connection(
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
    let stream = match tokio::net::UnixStream::connect(socket)
        .await
        .and_then(|stream| stream.into_std())
    {
        Ok(stream) => stream,
        Err(_) => return ConnectionEnd::Retry,
    };
    let stream = match tokio::io::unix::AsyncFd::new(stream) {
        Ok(stream) => Arc::new(stream),
        Err(_) => return ConnectionEnd::Retry,
    };
    let _connection = match stop.register_connection(stream.get_ref()) {
        Ok(registration) => registration,
        Err(_) => return ConnectionEnd::Retry,
    };

    #[cfg(feature = "ui")]
    let health_request = crate::daemon_health::begin();
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
    let mut connect_codec = FrameCodec::new(MAX_FRAME_BYTES);
    if connect_codec.encode(&connect).is_err()
        || write_encoded(&stream, &connect_codec).await.is_err()
    {
        return ConnectionEnd::Retry;
    }

    let handshake_deadline = Instant::now() + DAEMON_READY_TIMEOUT;
    // Keep partially received frames across the short polling timeout. A
    // timeout can interrupt either the length prefix or its payload.
    let mut handshake_codec = FrameCodec::new(MAX_FRAME_BYTES);
    let (proof, mut attached) = loop {
        published.expire_retained(Instant::now());
        if stop.is_requested() {
            return ConnectionEnd::Stop;
        }
        if Instant::now() >= handshake_deadline {
            return ConnectionEnd::Retry;
        }
        let read_deadline = (Instant::now() + HANDSHAKE_POLL_INTERVAL).min(handshake_deadline);
        let frame = tokio::time::timeout_at(
            tokio::time::Instant::from_std(read_deadline),
            super::transport::read::<ServerEnvelope>(&stream, &mut handshake_codec),
        )
        .await
        .unwrap_or_else(|_| Err(FrameError::Io(io::ErrorKind::TimedOut.into())));
        let envelope = match frame {
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
            #[cfg(feature = "ui")]
            crate::daemon_health::incompatible(socket, envelope.version, health_request);
            crate::diagnostics::record(crate::diagnostics::Event::ProtocolMismatch);
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
                return reject_handshake_async(events, published, stop, error).await;
            }
            return ConnectionEnd::Retry;
        }
        match envelope.message {
            ServerMessage::Response {
                response: ServerResponse::Connected { proof, resumed },
            } => {
                if envelope.proof != proof || !published.pin_proof(&proof) {
                    return reject_handshake_async(
                        events,
                        published,
                        stop,
                        ProtocolError::new(
                            ProtocolErrorCode::ProofMismatch,
                            "the daemon returned a different or incomplete session proof",
                            false,
                        ),
                    )
                    .await;
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
                #[cfg(feature = "ui")]
                crate::daemon_health::frame(stream.get_ref(), None, health_request);
                break (proof, attached);
            }
            ServerMessage::Error(error) => {
                return reject_handshake_async(events, published, stop, error).await;
            }
            _ => return ConnectionEnd::Retry,
        }
    };

    let mut connection = match ConnectionIo::new(stream, Arc::clone(published)) {
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
        .await
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
    .await
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
        .await
        .is_err()
    {
        return ConnectionEnd::Retry;
    }

    loop {
        published.expire_retained(Instant::now());
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
            .await
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
                .await
                .is_err()
                {
                    return ConnectionEnd::Retry;
                }
                last_acknowledgement = acknowledged;
            }
        }

        let work = wait_for_work_async(
            stop,
            (pending.len() < MAX_IN_FLIGHT_REQUESTS).then_some(*commands),
            connection.incoming(),
            published,
            &connection.credit,
        )
        .await;
        let envelope = match work {
            ClientWork::Acknowledge | ClientWork::RetainedExpired => continue,
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
        if envelope.version != PROTOCOL_VERSION {
            #[cfg(feature = "ui")]
            crate::daemon_health::incompatible(
                socket,
                envelope.version,
                crate::daemon_health::begin(),
            );
            return ConnectionEnd::Retry;
        }
        if envelope.proof != proof {
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
                            .await
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
                    .await
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
                    .await
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
                    .await
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

async fn send_request(
    (stream, codec): (&tokio::io::unix::AsyncFd<UnixStream>, &mut FrameCodec),
    proof: &HostSessionProof,
    next_request_id: &mut u64,
    pending: &mut BTreeMap<u64, PendingRequest>,
    kind: PendingRequest,
    request: ClientRequest,
) -> Result<(), FrameError> {
    let request_id = *next_request_id;
    *next_request_id = (*next_request_id).wrapping_add(1).max(2);
    codec.encode(&ClientEnvelope {
        version: PROTOCOL_VERSION,
        request_id,
        proof: Some(proof.clone()),
        request,
    })?;
    write_encoded(stream, codec).await?;
    pending.insert(request_id, kind);
    Ok(())
}

async fn write_encoded(
    socket: &tokio::io::unix::AsyncFd<UnixStream>,
    codec: &FrameCodec,
) -> Result<(), FrameError> {
    tokio::time::timeout(
        Duration::from_secs(5),
        super::transport::write(socket, codec.encoded()),
    )
    .await
    .unwrap_or_else(|_| Err(io::ErrorKind::TimedOut.into()))
    .map_err(FrameError::Io)
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
            published.observe_agent_activity(events, activity)?;
        }
        event => {
            // Recovery metadata can change without an activity transition.
            // The daemon independently emits identity/activity clears. Clearing
            // here would lose a stable Working state until its next change,
            // since clients no longer poll to repair it.
            if matches!(
                event,
                TerminalEvent::Exited { .. }
                    | TerminalEvent::Completed { .. }
                    | TerminalEvent::Error { .. }
            ) {
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

#[cfg(test)]
fn reject_handshake(
    events: &Sender<TerminalEvent>,
    published: &HostedPublishedState,
    stop: &WorkerStop,
    error: ProtocolError,
) -> ConnectionEnd {
    super::transport::runtime()
        .unwrap()
        .block_on(reject_handshake_async(events, published, stop, error))
}

async fn reject_handshake_async(
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
    let mut event = event;
    let mut retry = Duration::from_millis(1);
    loop {
        let stopping = stop.async_wake.notified();
        tokio::pin!(stopping);
        stopping.as_mut().enable();
        if stop.is_requested() {
            break;
        }
        match events.try_send(event) {
            Ok(()) => {
                published.request_semantic_wake();
                break;
            }
            Err(TrySendError::Disconnected(_)) => break,
            Err(TrySendError::Full(pending)) => event = pending,
        }
        // Only permanent failure delivery retries: ordinary idle connections
        // have no timer. The existing public receiver has no consume hook;
        // back off while preserving its final error and cancel on handle drop.
        tokio::select! { biased;
            _ = stopping => break,
            _ = tokio::time::sleep(retry) => {},
        }
        retry = (retry * 2).min(Duration::from_millis(256));
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
        terminal::ipc::{TerminalDescriptor, read_frame},
    };

    use super::*;
    #[test]
    fn thirty_hosted_terminals_add_no_os_threads() {
        const FIXTURE: &str = "CAGENTS_HOSTED_TRANSPORT_CENSUS";
        if std::env::var_os(FIXTURE).is_none() {
            let result = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "terminal::hosted::tests::thirty_hosted_terminals_add_no_os_threads",
                    "--nocapture",
                    "--test-threads=1",
                ])
                .env(FIXTURE, "1")
                .output()
                .unwrap();
            assert!(
                result.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&result.stdout),
                String::from_utf8_lossy(&result.stderr)
            );
            return;
        }
        let runtime = super::super::transport::runtime().unwrap();
        let count = || std::fs::read_dir("/proc/self/task").unwrap().count();
        let idle = count();
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("transport-census.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        let pinned = proof("transport-census");
        let server_proof = pinned.clone();
        let server = runtime.spawn(async move {
            let listener = tokio::net::UnixListener::from_std(listener).unwrap();
            let mut clients = Vec::new();
            for _ in 0..30 {
                let (stream, _) = listener.accept().await.unwrap();
                let pinned = server_proof.clone();
                clients.push(tokio::spawn(async move {
                    let stream = tokio::io::unix::AsyncFd::new(stream.into_std().unwrap()).unwrap();
                    let mut codec = FrameCodec::new(MAX_FRAME_BYTES);
                    while let Ok(Some(request)) =
                        super::super::transport::read::<ClientEnvelope>(&stream, &mut codec).await
                    {
                        let response = match request.request {
                            ClientRequest::Connect { resume, .. } => {
                                let terminal_id = resume[0].terminal_id;
                                let mut terminal = TerminalDescriptor::relaunchable(
                                    terminal_id,
                                    session().launch,
                                    None,
                                );
                                terminal.attached = true;
                                ServerResponse::Connected {
                                    proof: pinned.clone(),
                                    resumed: vec![terminal],
                                }
                            }
                            ClientRequest::Snapshot { terminal_id, .. } => {
                                ServerResponse::Snapshot {
                                    terminal_id,
                                    snapshot: None,
                                }
                            }
                            _ => ServerResponse::Accepted,
                        };
                        codec
                            .encode(&ServerEnvelope {
                                version: PROTOCOL_VERSION,
                                request_id: Some(request.request_id),
                                proof: pinned.clone(),
                                message: ServerMessage::Response { response },
                            })
                            .unwrap();
                        if super::super::transport::write(&stream, codec.encoded())
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                }));
            }
            for client in clients {
                client.await.unwrap();
            }
        });
        let mut handles = Vec::new();
        for _ in 0..30 {
            let handle = HostedTerminalHandle::connect_at(
                socket.clone(),
                TerminalId::new(),
                Some(pinned.clone()),
                |_| {},
            )
            .unwrap();
            handle.wait_until_attached(Duration::from_secs(3)).unwrap();
            handles.push(handle);
        }
        let attached = count();
        assert!(
            attached <= idle,
            "idle={idle}, thirty_hosted_terminals={attached}"
        );
        println!("hosted_transport_thread_census idle={idle} thirty_terminals={attached}");
        drop(handles);
        runtime.block_on(async {
            tokio::time::timeout(Duration::from_secs(5), server)
                .await
                .unwrap()
                .unwrap();
        });
    }

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
    fn recovery_metadata_does_not_erase_pushed_activity() {
        use crate::agent::{Activity, ActivityState, Provider};
        let (published, _) = published_state(false);
        let (events, received) = crossbeam_channel::bounded(8);
        let working = Some(Activity {
            provider: Provider::Claude,
            state: ActivityState::Working,
        });
        accept_server_event(
            &events,
            &published,
            1,
            TerminalEvent::AgentActivityChanged(working),
        )
        .unwrap();
        accept_server_event(
            &events,
            &published,
            2,
            TerminalEvent::ResumeHintChanged(Some("new recovery evidence".into())),
        )
        .unwrap();
        assert_eq!(*published.agent_activity.lock().unwrap(), working);
        assert_eq!(received.try_iter().count(), 2, "no invented activity clear");
        accept_server_event(
            &events,
            &published,
            3,
            TerminalEvent::AgentActivityChanged(None),
        )
        .unwrap();
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
                unset_environment: Default::default(),
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
                execution_instance: uuid::Uuid::nil(),
                screen_lost_after_crash: false,
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
    fn terminal_handshake_survives_timeout_mid_frame() {
        use std::io::Write;

        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("daemon.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let session = session();
        let pinned = proof("split-handshake");
        let handle = HostedTerminalHandle::attach_or_spawn_at(
            socket,
            session.clone(),
            TerminalSpawnOptions::default(),
            Some(pinned.clone()),
            |_| {},
        )
        .unwrap();
        let mut stream = accept_connection(&listener);
        let connect: ClientEnvelope = read_frame(&mut stream).unwrap().unwrap();
        let mut frame = Vec::new();
        write_frame(
            &mut frame,
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
        stream.write_all(&frame[..2]).unwrap();
        thread::sleep(HANDSHAKE_POLL_INTERVAL + Duration::from_millis(50));
        stream.write_all(&frame[2..]).unwrap();
        let spawn: ClientEnvelope = read_frame(&mut stream).unwrap().unwrap();
        let terminal = TerminalDescriptor {
            execution_instance: uuid::Uuid::nil(),
            screen_lost_after_crash: false,
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
        write_frame(
            &mut stream,
            &ServerEnvelope {
                version: PROTOCOL_VERSION,
                request_id: Some(spawn.request_id),
                proof: pinned.clone(),
                message: ServerMessage::Response {
                    response: ServerResponse::Spawned { terminal },
                },
            },
        )
        .unwrap();
        handle.wait_until_attached(Duration::from_secs(2)).unwrap();
        assert_eq!(handle.proof(), Some(pinned));
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
                                execution_instance: uuid::Uuid::nil(),
                                screen_lost_after_crash: false,
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
                wrapped: false,
                hyperlinks: Vec::new(),
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

    fn frame_text(published: &HostedPublishedState) -> Option<String> {
        let frame = published.frame();
        (!frame.detached).then(|| frame.lines[0].cells[0].text.as_str().to_owned())
    }

    /// Hibernating gives up the grid as far as the protocol is concerned, and
    /// only that far: the pane goes on showing it until a live one arrives.
    #[test]
    fn a_hibernated_view_draws_its_last_grid_until_a_live_one_replaces_it() {
        let (published, _wake_count) = published_state(true);
        published.publish(published_grid(3, "a"));

        published.hibernate();
        assert!(published.snapshot.load().detached);
        assert_eq!(frame_text(&published).as_deref(), Some("a"));

        // Coming back still asks the daemon for a whole grid, because the
        // kept one is a picture and no longer a base for deltas.
        published.set_visible(true);
        assert_eq!(published.snapshot_cursor(), 0);
        assert_eq!(frame_text(&published).as_deref(), Some("a"));

        // Hibernated again before that grid arrived, it keeps the picture.
        published.hibernate();
        assert_eq!(frame_text(&published).as_deref(), Some("a"));

        // The same generation repopulates the view, and nothing stale is
        // held once it has.
        published.set_visible(true);
        published.publish(published_grid(3, "b"));
        assert_eq!(frame_text(&published).as_deref(), Some("b"));
        assert!(published.retained.load().is_none());
    }

    #[test]
    fn hibernated_grid_expires_releases_memory_and_reveals_from_a_full_snapshot() {
        let (published, _) = published_state(true);
        published.publish(published_grid(3, "before"));
        let old = Arc::downgrade(&published.snapshot.load_full());
        let now = Instant::now();
        published.hibernate_at(now);
        assert!(old.upgrade().is_some());
        assert_eq!(frame_text(&published).as_deref(), Some("before"));
        published.expire_retained(now + RETAINED_FRAME_TTL - Duration::from_nanos(1));
        assert!(old.upgrade().is_some());
        published.expire_retained(now + RETAINED_FRAME_TTL);
        assert!(
            old.upgrade().is_none(),
            "the cold terminal still owns its old grid"
        );
        assert!(published.retained.load().is_none());
        published.set_visible(true);
        assert_eq!(published.snapshot_cursor(), 0);
        assert!(published.frame().detached);
        published.publish(published_grid(3, "after"));
        assert_eq!(frame_text(&published).as_deref(), Some("after"));
    }

    #[test]
    fn idle_and_saturated_workers_wake_to_release_an_expired_grid() {
        for saturated in [false, true] {
            let (published, _) = published_state(true);
            published.publish(published_grid(3, "before"));
            let old = Arc::downgrade(&published.snapshot.load_full());
            let stop = WorkerStop::default();
            let (_command_tx, command_rx) = crossbeam_channel::bounded(COMMAND_CAPACITY);
            let (_server_tx, server_rx) = crossbeam_channel::bounded(EVENT_CAPACITY);
            published.hibernate_at(Instant::now() - RETAINED_FRAME_TTL);
            // Publication wakes a worker that might already be waiting before
            // the UI's Hibernate call finishes. It does not grant redraw credit.
            assert_eq!(published.acknowledgement_rx.try_recv(), Ok(()));
            assert_eq!(published.acknowledged_generation.load(Ordering::Acquire), 0);
            let work = if saturated {
                wait_for_server_or_stop(&stop, &server_rx, &published)
            } else {
                wait_for_client_work(&stop, &command_rx, &server_rx, &published)
            };
            assert!(matches!(work, ClientWork::RetainedExpired));
            published.expire_retained(Instant::now());
            assert!(old.upgrade().is_none());
            assert!(published.retained.load().is_none());
        }
    }

    #[test]
    fn a_view_with_nothing_to_show_keeps_nothing_when_hibernated() {
        // Never painted: there is no last grid.
        let (published, _wake_count) = published_state(true);
        published.hibernate();
        assert!(published.retained.load().is_none());
        assert_eq!(frame_text(&published), None);

        // A terminal that could not be attached has no next frame for an old
        // picture to stand in for.
        let (published, _wake_count) = published_state(true);
        published.publish(published_grid(1, "a"));
        published.hibernate();
        published.mark_detached();
        assert_eq!(frame_text(&published), None);
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
            wait_for_client_work(&stop, &command_rx, &server_rx, &published),
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
            wait_for_client_work(&stop, &command_rx, &server_rx, &published),
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
            wait_for_client_work(&stop, &command_rx, &server_rx, &published),
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
                        execution_instance: uuid::Uuid::nil(),
                        screen_lost_after_crash: false,
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
                    wrapped: false,
                    hyperlinks: Vec::new(),
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
        // The pane keeps showing the grid it hibernated with.
        assert_eq!(handle.frame().lines[0].cells[0].text, "c");

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
        let (published, _) = published_state(true);
        let stop = WorkerStop::default();
        let (_server_tx, server_rx) = crossbeam_channel::bounded(EVENT_CAPACITY);

        stop.request();

        assert!(matches!(
            wait_for_server_or_stop(&stop, &server_rx, &published),
            ClientWork::Stop
        ));
    }
}
