//! Native chat connection to the same authenticated daemon as hosted terminals.
use super::{Command, Intent, Session, Snapshot, StartFailure};
use crate::{
    account::LiveReport,
    agent::ActivityState,
    model::{HostSessionProof, TerminalId},
    terminal_ipc::{
        self as ipc, ClientEnvelope, ClientRequest, FrameCodec, PROTOCOL_VERSION, ServerEnvelope,
        ServerMessage, ServerResponse,
    },
};
use arc_swap::ArcSwap;
use std::{
    collections::{BTreeMap, VecDeque},
    io,
    net::Shutdown,
    os::unix::net::UnixStream,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

// Native holder startup can spend 30 seconds sending its control request and
// another 30 seconds receiving the reply. Leave room for the daemon response
// and a busy desktop scheduler.
mod append;

const CHAT_ATTACH_TIMEOUT: Duration = Duration::from_secs(70);

pub fn restore_intent(intent: Intent, policy: crate::instance::WindowRestorePolicy) -> Intent {
    if intent == Intent::Restore && policy == crate::instance::WindowRestorePolicy::ExistingOnly {
        Intent::RestoreExisting
    } else {
        intent
    }
}

/// The connection a conversation opened with `intent` goes on over. A
/// recovery attachment reaches only a daemon that already runs, over a bare
/// connection; anything else proves the daemon ready, starting it where
/// none runs, and goes on over the connection that proved it.
fn chat_stream(intent: Intent) -> Result<ChatStream, StartFailure> {
    if intent == Intent::RestoreExisting {
        let socket = crate::terminal::existing_daemon_socket_path()
            .map_err(|error| StartFailure::from(error.to_string()))?;
        connect_daemon(&socket)
            .map(ChatStream::from)
            .map_err(|e| format!("Cannot connect to the proven chat daemon: {e}").into())
    } else {
        crate::terminal::connect_ready_daemon_for_explicit_action()
            .map(ChatStream::from)
            .map_err(|error| error.to_string().into())
    }
}

/// A connection a native chat goes on over: either one whose control
/// handshake already proved the daemon, which the chat continues on instead
/// of connecting and handshaking again, or a bare one it handshakes itself.
/// Both handshakes are the same `Connect`, so the daemon can't tell them
/// apart.
enum ChatStream {
    Proven {
        stream: UnixStream,
        proof: HostSessionProof,
    },
    Bare(UnixStream),
}

impl From<UnixStream> for ChatStream {
    fn from(stream: UnixStream) -> Self {
        Self::Bare(stream)
    }
}

impl From<crate::control::ControlClient> for ChatStream {
    fn from(client: crate::control::ControlClient) -> Self {
        let (stream, proof) = client.into_handshaken_stream();
        Self::Proven { stream, proof }
    }
}

impl From<crate::terminal::ReadyDaemon> for ChatStream {
    fn from(ready: crate::terminal::ReadyDaemon) -> Self {
        ready.client.into()
    }
}

/// An account switch that failed before asking the daemon for anything, so
/// the original conversation is untouched whatever the cause. An update in
/// progress is the daemon busy with another operation. A daemon that took
/// the connection but did not finish the handshake in time may be held by a
/// long operation or hung, which the client cannot tell apart.
fn switch_unadmitted(error: crate::terminal::HostedTerminalStartError) -> StartFailure {
    use crate::terminal::HostedTerminalStartError;
    let recovery = match &error {
        HostedTerminalStartError::UpdateInProgress(_) => super::Recovery::Busy,
        // A selected device's daemon is reached without a readiness proof,
        // so its handshake fails as a connection does.
        HostedTerminalStartError::HostProtocol(crate::control::ControlError::ReplyTimeout {
            ..
        })
        | HostedTerminalStartError::Connect(crate::control::ControlError::ReplyTimeout {
            ..
        }) => super::Recovery::Unresponsive,
        _ => super::Recovery::Unreached,
    };
    StartFailure {
        message: error.to_string(),
        recovery,
    }
}

/// [`super::UNPROVEN_OWNER_REFUSAL`] as an earlier daemon worded it. A
/// daemon outlives the client upgrades it serves, so a client still meets
/// this sentence until that daemon is replaced.
const EARLIER_UNPROVEN_OWNER_REFUSAL: &str = "This conversation's earlier Claude process ended without recording its close, and cagents cannot prove that nothing else still holds the conversation, so nothing was started in its place. It can be resumed after this computer restarts";

fn start_failure(error: ipc::ProtocolError) -> StartFailure {
    let recovery = match error.code {
        ipc::ProtocolErrorCode::AccountRemoved => super::Recovery::Account,
        ipc::ProtocolErrorCode::ChatCapacity => super::Recovery::Capacity,
        _ if error.is_daemon_busy() => super::Recovery::Busy,
        ipc::ProtocolErrorCode::NotAdmitted => super::Recovery::NotAdmitted,
        _ if error.retryable => super::Recovery::Retry,
        // Only the daemon's own whole sentence, never text that quotes it.
        _ if [
            super::UNPROVEN_OWNER_REFUSAL,
            EARLIER_UNPROVEN_OWNER_REFUSAL,
        ]
        .contains(&error.message.as_str()) =>
        {
            super::Recovery::AfterRestart
        }
        _ => super::Recovery::Blocked,
    };
    StartFailure {
        message: error.message,
        recovery,
    }
}

/// Stack ranges are virtual reservations, not committed memory. A profiler
/// can intersect these ranges with smaps RSS/PSS rather than multiplying an
/// assumed stack size by the number of chat workers.
fn trace_worker_stack(role: &'static str) {
    #[cfg(target_os = "linux")]
    if tracing::enabled!(target: "cagents::memory", tracing::Level::TRACE) {
        // SAFETY: pthread initializes attr on success; all output pointers are
        // live local storage and the initialized attribute is destroyed once.
        unsafe {
            let mut attr = std::mem::MaybeUninit::<libc::pthread_attr_t>::uninit();
            if libc::pthread_getattr_np(libc::pthread_self(), attr.as_mut_ptr()) != 0 {
                return;
            }
            let mut attr = attr.assume_init();
            let mut base = std::ptr::null_mut();
            let mut bytes = 0;
            let mut guard_bytes = 0;
            let valid = libc::pthread_attr_getstack(&attr, &mut base, &mut bytes) == 0;
            let _ = libc::pthread_attr_getguardsize(&attr, &mut guard_bytes);
            let _ = libc::pthread_attr_destroy(&mut attr);
            if valid {
                tracing::trace!(target: "cagents::memory", process_id = std::process::id(), owner = "thread_stack", role,
                    thread_id = libc::syscall(libc::SYS_gettid) as u64,
                    stack_base = base as usize, reserved_stack_bytes = bytes, guard_bytes);
            }
        }
    }
    #[cfg(not(target_os = "linux"))]
    let _ = role;
}

fn connect_daemon(path: &std::path::Path) -> io::Result<UnixStream> {
    #[cfg(feature = "ui")]
    let health_request = crate::daemon_health::begin();
    let result = UnixStream::connect(path);
    #[cfg(feature = "ui")]
    crate::daemon_health::connection_io(path, &result, health_request);
    result
}

/// Whether a read failed because its timeout passed with no answer.
fn frame_timed_out(error: &ipc::FrameError) -> bool {
    matches!(
        error,
        ipc::FrameError::Io(io_error)
            if matches!(
                io_error.kind(),
                io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
            )
    )
}

fn frame_read_error(error: ipc::FrameError, timeout: Option<Duration>) -> String {
    if !frame_timed_out(&error) {
        return format!("Cannot decode matched daemon protocol: {error}");
    }
    timeout.map_or_else(
        || "Timed out waiting for the daemon response".into(),
        |timeout| {
            format!(
                "the daemon did not answer within {}",
                crate::control::reply_budget(timeout)
            )
        },
    )
}

/// A conversation's subagents by the address their provider gives them, each
/// with the ID the snapshot lists it under. Shared, so a surface can keep a
/// copy for every frame without copying the addresses.
pub type AgentAddresses = Arc<BTreeMap<String, String>>;

/// Adds the addresses not yet known, which keep the agent they first named,
/// and says whether any was.
fn learn_addresses(known: &mut AgentAddresses, addresses: Vec<super::AgentAddress>) -> bool {
    let new = addresses
        .into_iter()
        .filter(|address| !known.contains_key(&address.address))
        .collect::<Vec<_>>();
    if new.is_empty() {
        return false;
    }
    let known = Arc::make_mut(known);
    for address in new {
        known.entry(address.address).or_insert(address.subagent);
    }
    true
}

#[derive(Debug, Clone)]
pub enum Wake {
    Changed,
    Observed,
    /// Consume the newest state with `take_activity`; never fetches a transcript.
    Activity,
    /// Consume the reports with `take_account_usage`.
    AccountUsage,
    /// The daemon's judgement of the usage limit stopping the conversation
    /// changed; read it with `limit`.
    Limit,
    /// The daemon said how the provider addresses more of the conversation's
    /// subagents; read them with `agent_addresses`.
    AgentAddresses,
    Disconnected(String),
}
#[derive(Clone)]
pub struct ClientHandle {
    inner: Arc<Inner>,
}
struct Wire {
    stream: UnixStream,
    codec: FrameCodec,
    body_requested: Arc<AtomicBool>,
    /// Whether the last visibility this connection sent was visible. The
    /// daemon refuses reads from a hidden subscriber, and visibility is sent
    /// from its own worker, so a read goes out only behind a visible frame.
    announced: bool,
    /// How many hidden frames this connection has sent. A read refused
    /// because a hide overtook it on the daemon is told apart by this count
    /// having moved since the read was sent.
    hidings: u64,
}
struct PendingCommand {
    prompt: bool,
    reply: crossbeam_channel::Sender<Result<Option<crate::model::TerminalSession>, String>>,
}
type Pending = Arc<Mutex<BTreeMap<u64, PendingCommand>>>;
type FilePending =
    Arc<Mutex<BTreeMap<u64, crossbeam_channel::Sender<Result<super::FileChangeReply, String>>>>>;
type TextCopyPending =
    Arc<Mutex<BTreeMap<u64, crossbeam_channel::Sender<Result<super::TextCopyPage, String>>>>>;
const TEXT_COPY_TIMEOUT: Duration = Duration::from_secs(40);

/// A daemon timestamp paired with local receipt time. It survives delayed UI
/// hydration and advances while the provider is quiet without relying on the
/// display machine's wall clock.
#[derive(Clone, Copy, Debug)]
pub struct SourceClock {
    sampled_at: u64,
    received_at: Instant,
}

impl SourceClock {
    fn from_sample(sampled_at: Option<u64>, received_at: Instant) -> Option<Self> {
        sampled_at.map(|sampled_at| Self {
            sampled_at,
            received_at,
        })
    }

    fn observe(clock: &mut Option<Self>, sampled_at: Option<u64>, received_at: Instant) {
        if let Some(mut next) = Self::from_sample(sampled_at, received_at) {
            // Network jitter and wall-clock adjustments must not make a live
            // duration count backwards on the next publication.
            if let Some(previous) = clock {
                next.sampled_at = next.sampled_at.max(previous.at(received_at));
            }
            *clock = Some(next);
        }
    }

    pub fn now(self) -> u64 {
        self.at(Instant::now())
    }

    fn at(self, now: Instant) -> u64 {
        self.sampled_at.saturating_add(
            u64::try_from(now.saturating_duration_since(self.received_at).as_millis())
                .unwrap_or(u64::MAX),
        )
    }
}

/// An exact recovery authenticated on one daemon stream and confirmed by its
/// grant-bound attachment. This is deliberately neither serializable nor
/// constructible from descriptors outside the handshake.
#[derive(Clone, Debug)]
pub struct UpgradeRecovery {
    sessions: (Session, Session),
    kind: UpgradeRecoveryKind,
}

#[derive(Clone, Copy, Debug)]
enum UpgradeRecoveryKind {
    Exact,
    FirstIdentity,
    BoundRetainedReset,
}

impl UpgradeRecovery {
    /// Shape alone authorizes only an exact lineage query, never activation or
    /// persistence. Only the authenticated handshake can produce a witness.
    pub fn is_candidate(source: &Session, replacement: &Session) -> bool {
        Self::candidate(source, replacement).is_some()
    }

    fn candidate(source: &Session, replacement: &Session) -> Option<UpgradeRecoveryKind> {
        if super::is_upgrade_recovery(source, replacement) {
            return Some(UpgradeRecoveryKind::Exact);
        }
        if super::is_upgrade_identity_learning(source, replacement) {
            return Some(UpgradeRecoveryKind::FirstIdentity);
        }
        if !matches!(
            source.native_continuity,
            super::NativeContinuity::RetainedClaudeActor { protocol: 2 }
        ) || !source.has_valid_native_continuity()
            || source.launch.session_id.is_none()
            || replacement.launch.session_id.is_none()
            || source.launch.session_id == replacement.launch.session_id
        {
            return None;
        }
        // Shape validation alone grants no authority. The HOST must resolve
        // the exact issued alias and bind this stream's attachment to that
        // helper, including its actor identity and attachment epoch.
        let mut normalized = source.clone();
        normalized.launch.session_id = replacement.launch.session_id.clone();
        super::is_upgrade_recovery(&normalized, replacement)
            .then_some(UpgradeRecoveryKind::BoundRetainedReset)
    }

    fn confirmed(
        source: Session,
        replacement: Session,
        attached: &Session,
        snapshot: &Snapshot,
    ) -> Result<Self, StartFailure> {
        if attached != &replacement || snapshot.session_id != replacement.launch.session_id {
            return Err(
                "The attached conversation did not confirm the daemon's exact upgrade replacement"
                    .into(),
            );
        }
        let kind = Self::candidate(&source, &replacement)
            .ok_or("The authenticated upgrade changed the saved conversation configuration")?;
        Ok(Self {
            sessions: (source, replacement),
            kind,
        })
    }

    pub fn source(&self) -> &Session {
        &self.sessions.0
    }
    pub fn replacement(&self) -> &Session {
        &self.sessions.1
    }

    pub fn validate(&self) -> bool {
        matches!(
            (
                self.kind,
                Self::candidate(self.source(), self.replacement())
            ),
            (UpgradeRecoveryKind::Exact, Some(UpgradeRecoveryKind::Exact))
                | (
                    UpgradeRecoveryKind::FirstIdentity,
                    Some(UpgradeRecoveryKind::FirstIdentity)
                )
                | (
                    UpgradeRecoveryKind::BoundRetainedReset,
                    Some(UpgradeRecoveryKind::BoundRetainedReset)
                )
        )
    }
}

/// One immutable, coherent presentation. Append evidence owns only bounded
/// Weak item edges; neither hidden state nor lineage keeps transcript bodies.
struct Publication {
    owned: Option<Box<RetiredPublication>>,
    retirement: Arc<ClientRetirement>,
    // One exact connection reader, registered inside its owned thread. This
    // preserves producer-side destruction backpressure without labeling callers
    // or trusting the thread which happened to construct the attach snapshot.
    source_thread: Arc<std::sync::OnceLock<std::thread::ThreadId>>,
}

struct PublicationPayload {
    snapshot: Arc<Snapshot>,
    clock: Option<SourceClock>,
    resident: bool,
    lineage: append::Lineage,
    // Ready before installation: hide performs no body/metadata derivation.
    // Metadata nodes have no pair and own no body or append authority.
    hidden: Option<Arc<Publication>>,
    #[cfg(any(test, feature = "test-support"))]
    retirement_complete: Mutex<Option<std::sync::mpsc::Sender<std::thread::ThreadId>>>,
}

impl std::ops::Deref for Publication {
    type Target = PublicationPayload;

    fn deref(&self) -> &Self::Target {
        &self
            .owned
            .as_ref()
            .expect("held until Publication::drop")
            .payload
    }
}

impl Drop for Publication {
    fn drop(&mut self) {
        if let Some(retired) = self.owned.take() {
            // ArcSwap guards can become last owners on a presentation reader.
            // Retire on EVERY final Publication path, not only hide/swap paths.
            if self
                .source_thread
                .get()
                .is_some_and(|id| *id == thread::current().id())
            {
                // Ordinary producer swaps keep their existing disposal cost;
                // only a different final owner hands this work to the reaper.
                retired.finish();
            } else {
                self.retirement.enqueue(RetirementJob::Publication(retired));
            }
        }
    }
}

struct RetiredPublication {
    payload: PublicationPayload,
    next: Option<RetirementJob>,
}

impl RetiredPublication {
    fn finish(self: Box<Self>) {
        debug_assert!(self.next.is_none());
        #[cfg(any(test, feature = "test-support"))]
        let complete = self
            .payload
            .retirement_complete
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take();
        // Snapshot/control vectors, weak lineage and ready metadata ownership
        // all belong to this same node, regardless of the last Arc's thread.
        drop(self);
        #[cfg(any(test, feature = "test-support"))]
        if let Some(complete) = complete {
            let _ = complete.send(thread::current().id());
        }
    }
}

impl Publication {
    fn owned(
        snapshot: Snapshot,
        clock: Option<SourceClock>,
        resident: bool,
        lineage: append::Lineage,
        hidden: Option<Arc<Publication>>,
        source_thread: Arc<std::sync::OnceLock<std::thread::ThreadId>>,
    ) -> Arc<Self> {
        // The connection starts this owner before any handshake/body admission.
        // Direct test constructors use the same fallible process initialization.
        let retirement = ClientRetirement::shared()
            .expect("client retirement must prestart before body admission");
        Arc::new(Self {
            owned: Some(Box::new(RetiredPublication {
                payload: PublicationPayload {
                    snapshot: Arc::new(snapshot),
                    clock,
                    resident,
                    lineage,
                    hidden,
                    #[cfg(any(test, feature = "test-support"))]
                    retirement_complete: Mutex::new(None),
                },
                next: None,
            })),
            retirement,
            source_thread,
        })
    }

    fn initial(snapshot: Snapshot) -> Arc<Self> {
        let clock = SourceClock::from_sample(snapshot.sampled_at, Instant::now());
        Self::resident(
            snapshot,
            clock,
            append::Lineage::default(),
            Arc::new(std::sync::OnceLock::new()),
        )
    }

    fn resident(
        snapshot: Snapshot,
        clock: Option<SourceClock>,
        lineage: append::Lineage,
        source_thread: Arc<std::sync::OnceLock<std::thread::ThreadId>>,
    ) -> Arc<Self> {
        let mut metadata = snapshot.clone();
        metadata.evict_transcript();
        let hidden = Self::owned(
            metadata,
            clock,
            false,
            append::Lineage::default(),
            None,
            source_thread.clone(),
        );
        Self::owned(snapshot, clock, true, lineage, Some(hidden), source_thread)
    }

    fn next(
        base: &Self,
        mut snapshot: Snapshot,
        resident: bool,
        lineage: append::Lineage,
    ) -> Arc<Self> {
        snapshot.active_model = base.snapshot.active_model.clone();
        snapshot.active_model_revision = base.snapshot.active_model_revision;
        let mut clock = base.clock;
        SourceClock::observe(&mut clock, snapshot.sampled_at, Instant::now());
        if resident {
            Self::resident(snapshot, clock, lineage, base.source_thread.clone())
        } else {
            Self::owned(
                snapshot,
                clock,
                false,
                append::Lineage::default(),
                None,
                base.source_thread.clone(),
            )
        }
    }
}

// Local readability ownership, independent of daemon/provider ownership and
// ordinary offscreen interest. A frozen UI projection replaces the client's
// final-body fallback only for this exact connection.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum BodyOwner {
    #[default]
    Client,
    FrozenProjection,
}

#[derive(Default)]
struct PublicationPolicy {
    revision: u64,
    body_owner: BodyOwner,
}

struct Publications {
    current: ArcSwap<Publication>,
    // Only writers use this gate. All body derivation and destruction happens
    // outside it; UI presentation readers never acquire a producer lock.
    commit: Mutex<PublicationPolicy>,
}

impl Publications {
    fn new(snapshot: Snapshot) -> Self {
        Self {
            current: ArcSwap::from(Publication::initial(snapshot)),
            commit: Mutex::new(PublicationPolicy::default()),
        }
    }

    fn bind_source_thread(&self) {
        self.current
            .load()
            .source_thread
            .set(thread::current().id())
            .expect("one owned reader per publication lineage");
    }

    /// Pair the installed identity with body-policy revision before deriving
    /// any proposal. Metadata can be the base of a full response, so identity
    /// alone cannot fence a hide/show cycle while that response is built.
    fn capture(&self) -> (Arc<Publication>, u64) {
        let revision = self.commit.lock().unwrap_or_else(|p| p.into_inner());
        (self.current.load_full(), revision.revision)
    }

    fn replace(
        &self,
        mut snapshot: Snapshot,
        body: &AtomicBool,
        sample_clock: bool,
        final_body: bool,
    ) -> Option<bool> {
        let (base, revision) = self.capture();
        snapshot.active_model = base.snapshot.active_model.clone();
        snapshot.active_model_revision = base.snapshot.active_model_revision;
        let mut clock = base.clock;
        if sample_clock {
            SourceClock::observe(&mut clock, snapshot.sampled_at, Instant::now());
        }
        // A full snapshot over a body this client kept shares every item that
        // did not change, so the interface keeps what it prepared from them.
        let mut lineage = append::Lineage::default();
        if base.resident {
            lineage = base.lineage.clone();
            lineage.adopt(&base.snapshot, &mut snapshot);
        }
        let full = Publication::resident(snapshot, clock, lineage, base.source_thread.clone());
        let metadata = full
            .hidden
            .as_ref()
            .expect("resident publication has ready metadata")
            .clone();
        #[cfg(test)]
        tests::pause_before_replacement(self as *const Self as usize);
        self.publish(&base, revision, body, Some(full), metadata, final_body)
    }

    fn active_model(
        &self,
        revision: u64,
        model: Option<super::ActiveModel>,
        body: &AtomicBool,
    ) -> bool {
        loop {
            let (base, policy_revision) = self.capture();
            if revision <= base.snapshot.active_model_revision {
                return false;
            }
            let mut snapshot = (*base.snapshot).clone();
            snapshot.active_model = model.clone();
            snapshot.active_model_revision = revision;
            let next = if base.resident {
                Publication::resident(
                    snapshot,
                    base.clock,
                    base.lineage.clone(),
                    base.source_thread.clone(),
                )
            } else {
                Publication::owned(
                    snapshot,
                    base.clock,
                    false,
                    append::Lineage::default(),
                    None,
                    base.source_thread.clone(),
                )
            };
            let metadata = next.hidden.as_ref().unwrap_or(&next).clone();
            let full = base.resident.then_some(next);
            if self
                .publish(&base, policy_revision, body, full, metadata, false)
                .is_some()
            {
                return true;
            }
        }
    }

    /// The connection reader is the only source writer; visibility is a second
    /// writer. A hide (even followed by show) invalidates any derived body.
    /// Visibility preserves generation/clock, so a validated metadata fallback
    /// can still advance controls. A different source base requires resync.
    fn publish(
        &self,
        base: &Arc<Publication>,
        revision: u64,
        body: &AtomicBool,
        mut full: Option<Arc<Publication>>,
        metadata: Arc<Publication>,
        final_body: bool,
    ) -> Option<bool> {
        let (old, current, result) = {
            let current_revision = self.commit.lock().unwrap_or_else(|p| p.into_inner());
            let current = self.current.load_full();
            if !final_body && current.snapshot.generation != base.snapshot.generation {
                (None, current, None)
            } else {
                // A final body is the cold kept-card fallback after ordinary
                // hiding. Only an explicit handoff to an already readable
                // frozen projection relinquishes that fallback. Final control
                // and identity metadata remains authoritative in either mode.
                let resident = full.is_some()
                    && current_revision.body_owner == BodyOwner::Client
                    && (final_body
                        || (current_revision.revision == revision
                            && Arc::ptr_eq(base, &current)
                            && body.load(Ordering::Acquire)));
                let next = if resident {
                    full.take().unwrap()
                } else {
                    metadata
                };
                let old = self.current.swap(next);
                (Some(old), current, Some(resident))
            }
        };
        // ArcSwap can retire the last owner of a large old transcript. Do not
        // let that destruction, or an unused proposal's, run under the gate.
        drop(old);
        drop(current);
        result
    }
}

// Keep the public ClientHandle/Arc API unchanged. The private payload can move
// as one owner, including its installed body, even after both threads settled.
struct Inner {
    owned: Option<Box<RetiredClient>>,
    retirement: Arc<ClientRetirement>,
}

struct InnerPayload {
    id: TerminalId,
    session: Session,
    upgrade_recovery: Option<UpgradeRecovery>,
    /// The stranded card this handle's daemon-prepared successor replaces.
    resumed_from: Option<Session>,
    proof: HostSessionProof,
    wire: Arc<Mutex<Wire>>,
    cancel: UnixStream,
    publication: Arc<Publications>,
    body_requested: Arc<AtomicBool>,
    // First-prompt receipt evidence survives body eviction, without retaining
    // user text. This is scoped to this exact authenticated connection.
    user_item_seen: Arc<AtomicBool>,
    prompt_accepted: Arc<AtomicBool>,
    activity: Arc<Mutex<ActivityObservation>>,
    observed: Arc<Mutex<OutputObservation>>,
    account_usage: Arc<Mutex<AccountUsage>>,
    limit: Arc<Mutex<Option<crate::agent_execution::directory::UsageLimit>>>,
    agent_addresses: Arc<Mutex<AgentAddresses>>,
    next: Arc<AtomicU64>,
    alive: Arc<AtomicBool>,
    visible: Arc<AtomicBool>,
    wake: Arc<dyn Fn(Wake) + Send + Sync>,
    reader: Mutex<Option<JoinHandle<()>>>,
    pending: Pending,
    file_pending: FilePending,
    text_copy_pending: TextCopyPending,
    text_copies: AtomicUsize,
    visibility_tx: crossbeam_channel::Sender<()>,
    visibility_worker: Mutex<Option<JoinHandle<()>>>,
    #[cfg(any(test, feature = "test-support"))]
    retirement_complete: Option<std::sync::mpsc::Sender<std::thread::ThreadId>>,
}

impl Inner {
    fn new(payload: InnerPayload, retirement: Arc<ClientRetirement>) -> Self {
        Self {
            owned: Some(Box::new(RetiredClient {
                payload,
                next: None,
            })),
            retirement,
        }
    }
}

impl std::ops::Deref for Inner {
    type Target = InnerPayload;

    fn deref(&self) -> &Self::Target {
        &self.owned.as_ref().expect("held until Inner::drop").payload
    }
}

impl std::ops::DerefMut for Inner {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.owned.as_mut().expect("held until Inner::drop").payload
    }
}

struct RetiredClient {
    payload: InnerPayload,
    next: Option<RetirementJob>,
}

impl RetiredClient {
    fn ready(&mut self) -> bool {
        self.payload
            .reader
            .get_mut()
            .unwrap_or_else(|p| p.into_inner())
            .as_ref()
            .is_none_or(JoinHandle::is_finished)
            && self
                .payload
                .visibility_worker
                .get_mut()
                .unwrap_or_else(|p| p.into_inner())
                .as_ref()
                .is_none_or(JoinHandle::is_finished)
    }

    fn finish(mut self: Box<Self>) {
        debug_assert!(self.next.is_none());
        debug_assert!(self.ready());
        if let Some(reader) = self
            .payload
            .reader
            .get_mut()
            .unwrap_or_else(|p| p.into_inner())
            .take()
        {
            let _ = reader.join();
        }
        if let Some(worker) = self
            .payload
            .visibility_worker
            .get_mut()
            .unwrap_or_else(|p| p.into_inner())
            .take()
        {
            let _ = worker.join();
        }
        let id = self.payload.id;
        #[cfg(any(test, feature = "test-support"))]
        let completed = self.payload.retirement_complete.take();
        // Actual publication, codec scratch, pending replies, session metadata
        // and callback ownership all leave the event thread together.
        drop(self);
        tracing::trace!(target: "cagents::memory", process_id = std::process::id(), owner = "chat_client", chat_id = %id,
            released = true, resident_bytes = 0usize);
        #[cfg(any(test, feature = "test-support"))]
        if let Some(completed) = completed {
            let _ = completed.send(std::thread::current().id());
        }
    }
}

enum RetirementJob {
    Client(Box<RetiredClient>),
    Publication(Box<RetiredPublication>),
}

impl RetirementJob {
    fn next_mut(&mut self) -> &mut Option<Self> {
        match self {
            Self::Client(client) => &mut client.next,
            Self::Publication(publication) => &mut publication.next,
        }
    }

    fn ready(&mut self) -> bool {
        match self {
            Self::Client(client) => client.ready(),
            Self::Publication(_) => true,
        }
    }

    fn finish(self) {
        match self {
            Self::Client(client) => client.finish(),
            Self::Publication(publication) => publication.finish(),
        }
    }
}

/// One process-owned worker, separate from the parser/presentation FIFO. Each
/// admitted client and constructed publication already owns its intrusive node;
/// Drop allocates no queue node and never starts a thread. Outstanding client
/// and publication retirements have no fixed global byte cap. Never join an
/// unfinished client here.
struct ClientRetirement {
    incoming: Mutex<Option<RetirementJob>>,
    wake: std::sync::Condvar,
    worker: Mutex<Option<JoinHandle<()>>>,
}

/// Only a successful process owner is cached. An OS thread-spawn error must
/// refuse this admission without permanently poisoning later chat attempts.
struct ClientRetirementInitializer {
    owner: std::sync::OnceLock<Arc<ClientRetirement>>,
    initialize: Mutex<()>,
}

impl ClientRetirementInitializer {
    const fn new() -> Self {
        Self {
            owner: std::sync::OnceLock::new(),
            initialize: Mutex::new(()),
        }
    }

    fn get_or_start(
        &self,
        start: impl FnOnce() -> Result<Arc<ClientRetirement>, String>,
    ) -> Result<Arc<ClientRetirement>, String> {
        if let Some(owner) = self.owner.get() {
            return Ok(owner.clone());
        }
        let _initializing = self.initialize.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(owner) = self.owner.get() {
            return Ok(owner.clone());
        }
        let owner = start()?;
        // This is the sole setter, serialized by initialize. Failure leaves
        // OnceLock empty; concurrent admissions can never start duplicate owners.
        assert!(
            self.owner.set(owner.clone()).is_ok(),
            "serialized retirement initialization"
        );
        Ok(owner)
    }
}

impl ClientRetirement {
    fn shared() -> Result<Arc<Self>, String> {
        static OWNER: ClientRetirementInitializer = ClientRetirementInitializer::new();
        OWNER.get_or_start(|| {
            Self::start_with(|worker| {
                thread::Builder::new()
                    .name("chat-client-retire".into())
                    .spawn(worker)
            })
        })
    }

    fn for_client(id: TerminalId) -> Result<Arc<Self>, String> {
        #[cfg(test)]
        if tests::fail_retirement_start(id) {
            return Self::start_with(|_| {
                Err(io::Error::other("fixture retirement worker startup failed"))
            });
        }
        #[cfg(not(test))]
        let _ = id;
        Self::shared()
    }

    fn start_with(
        spawn: impl FnOnce(Box<dyn FnOnce() + Send>) -> io::Result<JoinHandle<()>>,
    ) -> Result<Arc<Self>, String> {
        let owner = Arc::new(Self {
            incoming: Mutex::new(None),
            wake: std::sync::Condvar::new(),
            worker: Mutex::new(None),
        });
        let running = owner.clone();
        // No client payload or connection thread has been admitted yet. A spawn
        // failure drops only this empty startup state and returns a real error.
        let worker = spawn(Box::new(move || running.run())).map_err(|error| error.to_string())?;
        *owner.worker.lock().unwrap_or_else(|p| p.into_inner()) = Some(worker);
        Ok(owner)
    }

    fn enqueue(&self, mut retired: RetirementJob) {
        let mut incoming = self.incoming.lock().unwrap_or_else(|p| p.into_inner());
        *retired.next_mut() = incoming.take();
        *incoming = Some(retired);
        drop(incoming);
        self.wake.notify_one();
    }

    fn run(&self) {
        let mut waiting = None;
        loop {
            let incoming = {
                let mut incoming = self.incoming.lock().unwrap_or_else(|p| p.into_inner());
                while incoming.is_none() && waiting.is_none() {
                    incoming = self.wake.wait(incoming).unwrap_or_else(|p| p.into_inner());
                }
                incoming.take()
            };
            // Both lists are owned only by this worker. No scan, join, trace or
            // payload destruction holds the short UI handoff mutex.
            for mut list in [incoming, waiting.take()] {
                while let Some(mut retired) = list {
                    list = retired.next_mut().take();
                    if retired.ready() {
                        if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            retired.finish()
                        }))
                        .is_err()
                        {
                            // Each node is unlinked first: an ordinary payload
                            // destructor panic cannot drop or strand later jobs.
                            // Rust abort/double-panic behavior is unchanged.
                            tracing::error!("chat client retirement payload panicked");
                        }
                    } else {
                        *retired.next_mut() = waiting.take();
                        waiting = Some(retired);
                    }
                }
            }
            if waiting.is_some() {
                let incoming = self.incoming.lock().unwrap_or_else(|p| p.into_inner());
                if incoming.is_none() {
                    drop(
                        self.wake
                            .wait_timeout(incoming, Duration::from_millis(10))
                            .unwrap_or_else(|p| p.into_inner()),
                    );
                }
            }
        }
    }
}

#[derive(Default)]
struct OutputObservation {
    epoch: u64,
    pending: bool,
}

impl OutputObservation {
    fn observe(&mut self, epoch: u64) -> bool {
        if epoch != self.epoch {
            return false;
        }
        !std::mem::replace(&mut self.pending, true)
    }

    fn begin_acknowledgement(&mut self) -> u64 {
        self.epoch = self
            .epoch
            .checked_add(1)
            .expect("chat observation epoch exhausted");
        self.pending = false;
        self.epoch
    }

    fn take(&mut self) -> bool {
        std::mem::take(&mut self.pending)
    }
}

/// Account reports not yet taken by the surface, oldest first.
///
/// They are kept rather than coalesced to the newest, because a Codex update
/// names only the bucket that moved and the next may name another. The bound
/// only matters to a surface that stopped taking them, and drops the oldest.
#[derive(Default)]
struct AccountUsage {
    reports: VecDeque<(LiveReport, u64)>,
}

impl AccountUsage {
    const LIMIT: usize = 32;

    /// Queues a report, and says whether the surface has to be woken for it:
    /// only the first since the surface last took them.
    fn push(&mut self, report: LiveReport, observed_at: u64) -> bool {
        if self.reports.len() == Self::LIMIT {
            self.reports.pop_front();
        }
        self.reports.push_back((report, observed_at));
        self.reports.len() == 1
    }
}

struct ActivityObservation {
    generation: u64,
    state: ActivityState,
    pending: bool,
}

impl ActivityObservation {
    fn observe(&mut self, generation: u64, state: ActivityState) -> bool {
        if generation < self.generation {
            return false;
        }
        self.generation = generation;
        if self.state == state {
            return false;
        }
        self.state = state;
        !std::mem::replace(&mut self.pending, true)
    }

    fn take(&mut self) -> ActivityState {
        self.pending = false;
        self.state
    }
}
fn transmit(
    wire: &Mutex<Wire>,
    proof: &HostSessionProof,
    id: u64,
    request: ClientRequest,
) -> Result<(), String> {
    let mut wire = wire.lock().unwrap_or_else(|p| p.into_inner());
    write_request(&mut wire, proof, id, request)
}
/// Send a presentation read if the daemon was last told the chat is
/// visible; otherwise the visible frame still to come makes the daemon
/// announce a change, which reads then. A sent read is returned as its
/// [`SentRead`].
fn transmit_read(
    wire: &Mutex<Wire>,
    proof: &HostSessionProof,
    id: u64,
    request: ClientRequest,
) -> Result<Option<SentRead>, String> {
    let mut wire = wire.lock().unwrap_or_else(|p| p.into_inner());
    if !wire.announced {
        return Ok(None);
    }
    let hidings = wire.hidings;
    write_request(&mut wire, proof, id, request).map(|()| Some(SentRead { id, hidings }))
}
/// A presentation read on the wire: its request id, and how many hidden
/// frames had been sent before it.
#[derive(Clone, Copy)]
struct SentRead {
    id: u64,
    hidings: u64,
}
impl SentRead {
    /// Whether `error`, answering `request`, is the daemon refusing this read
    /// because a hide sent after it arrived first. Showing the chat again
    /// makes the daemon announce a change, so the read is simply made again.
    fn overtaken(
        self,
        request: Option<u64>,
        error: &ipc::ProtocolError,
        wire: &Mutex<Wire>,
    ) -> bool {
        request == Some(self.id)
            && error.code == ipc::ProtocolErrorCode::InvalidRequest
            && wire.lock().unwrap_or_else(|p| p.into_inner()).hidings != self.hidings
    }
}
fn write_request(
    wire: &mut Wire,
    proof: &HostSessionProof,
    id: u64,
    request: ClientRequest,
) -> Result<(), String> {
    let visibility = match &request {
        ClientRequest::ChatVisibility { visible, .. } => Some(*visible),
        _ => None,
    };
    let hidden = visibility == Some(false);
    let Wire {
        stream,
        codec,
        body_requested,
        announced,
        hidings,
    } = wire;
    codec
        .write(
            stream,
            &ClientEnvelope {
                version: PROTOCOL_VERSION,
                request_id: id,
                proof: Some(proof.clone()),
                request,
            },
        )
        .map_err(|e| e.to_string())?;
    if let Some(visible) = visibility {
        *announced = visible;
    }
    if hidden {
        *hidings += 1;
    }
    if hidden || !body_requested.load(Ordering::Acquire) {
        codec.release_spare();
    }
    Ok(())
}
fn text_copy_remaining(deadline: Instant) -> Result<Duration, String> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|remaining| !remaining.is_zero())
        .ok_or_else(|| "Text copy request timed out".into())
}

/// Bound waiting for the shared writer as well as every partial socket write.
/// A failed frame must close the connection so later writes cannot continue it.
fn transmit_text_copy(
    wire: &Mutex<Wire>,
    proof: &HostSessionProof,
    id: u64,
    request: ClientRequest,
    deadline: Instant,
) -> Result<(), String> {
    let mut wire = loop {
        text_copy_remaining(deadline)?;
        match wire.try_lock() {
            Ok(wire) => break wire,
            Err(std::sync::TryLockError::Poisoned(poisoned)) => break poisoned.into_inner(),
            Err(std::sync::TryLockError::WouldBlock) => {
                thread::sleep(text_copy_remaining(deadline)?.min(Duration::from_millis(1)));
            }
        }
    };
    struct DeadlineWriter<'a> {
        stream: &'a mut UnixStream,
        deadline: Instant,
    }
    impl io::Write for DeadlineWriter<'_> {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            let remaining = text_copy_remaining(self.deadline)
                .map_err(|message| io::Error::new(io::ErrorKind::TimedOut, message))?;
            self.stream.set_write_timeout(Some(remaining))?;
            io::Write::write(self.stream, bytes)
        }
        fn flush(&mut self) -> io::Result<()> {
            io::Write::flush(self.stream)
        }
    }
    let Wire { stream, codec, .. } = &mut *wire;
    let previous_timeout = stream.write_timeout().map_err(|error| error.to_string())?;
    let result = codec
        .write(
            &mut DeadlineWriter { stream, deadline },
            &ClientEnvelope {
                version: PROTOCOL_VERSION,
                request_id: id,
                proof: Some(proof.clone()),
                request,
            },
        )
        .map_err(|error| error.to_string());
    let restored = stream
        .set_write_timeout(previous_timeout)
        .map_err(|error| error.to_string());
    if result.is_err() || restored.is_err() {
        let _ = stream.shutdown(Shutdown::Both);
    }
    result.and(restored)
}

fn text_copy_response(
    chat: TerminalId,
    message: ServerMessage,
) -> Result<super::TextCopyPage, String> {
    match message {
        ServerMessage::Response {
            response: ServerResponse::ChatTextCopy { chat_id, page },
        } if chat_id == chat => Ok(page),
        ServerMessage::Error(error) => Err(error.message),
        _ => Err("Unexpected text copy reply".into()),
    }
}

fn append_text_copy_page(
    source: &super::TextCopySource,
    text: &mut String,
    page: super::TextCopyPage,
) -> Result<(), String> {
    if source.bytes > super::TEXT_COPY_LIMIT_BYTES as u64 {
        return Err("Text copy exceeds the supported byte limit".into());
    }
    if page.source != *source || page.offset != text.len() as u64 {
        return Err("Text copy source or page offset changed".into());
    }
    let bytes = page.text.len();
    let end = page
        .offset
        .checked_add(bytes as u64)
        .ok_or("Text copy page overflow")?;
    if bytes > super::TEXT_COPY_PAGE_BYTES
        || end > source.bytes
        || page.done != (end == source.bytes)
        || (!page.done && bytes == 0)
    {
        return Err("Invalid text copy page length or completion marker".into());
    }
    text.push_str(&page.text);
    Ok(())
}

enum HandshakeError {
    Protocol(ipc::ProtocolError),
    /// No response from a different proof can attach. Only its well-framed
    /// explicit refusal can describe why an initial hard target was rejected.
    ProofChanged(Option<ipc::ProtocolError>),
    /// The daemon took the connection but its answer did not arrive before the
    /// read timeout.
    Unanswered(String),
    Transport(String),
}
impl From<String> for HandshakeError {
    fn from(message: String) -> Self {
        Self::Transport(message)
    }
}
impl From<&str> for HandshakeError {
    fn from(message: &str) -> Self {
        Self::Transport(message.into())
    }
}
fn receive(
    stream: &mut UnixStream,
    codec: &mut FrameCodec,
    request_id: u64,
    proof: Option<&HostSessionProof>,
) -> Result<ServerResponse, String> {
    receive_handshake(stream, codec, request_id, proof).map_err(|error| match error {
        HandshakeError::Protocol(error) => error.message,
        HandshakeError::ProofChanged(_) => "Daemon proof changed".into(),
        HandshakeError::Unanswered(message) | HandshakeError::Transport(message) => message,
    })
}
fn receive_start(
    stream: &mut UnixStream,
    codec: &mut FrameCodec,
    request_id: u64,
    proof: Option<&HostSessionProof>,
) -> Result<ServerResponse, StartFailure> {
    receive_handshake(stream, codec, request_id, proof).map_err(|error| match error {
        HandshakeError::Protocol(error) => start_failure(error),
        HandshakeError::ProofChanged(error) => StartFailure::retry(
            error.map_or_else(|| "Daemon proof changed".into(), |error| error.message),
        ),
        HandshakeError::Unanswered(message) | HandshakeError::Transport(message) => {
            StartFailure::retry(message)
        }
    })
}
#[cfg(test)]
fn receive_connect(
    stream: &mut UnixStream,
    codec: &mut FrameCodec,
    request_id: u64,
    expected: Option<&HostSessionProof>,
) -> Result<ServerResponse, StartFailure> {
    receive_handshake(stream, codec, request_id, expected)
        .map_err(|error| connect_failure(error, expected.is_some()))
}
/// Why a native chat connection could not be opened, for a start whose
/// `Connect` named the daemon it `expected`, or none.
fn connect_failure(error: HandshakeError, expected: bool) -> StartFailure {
    match error {
        HandshakeError::Protocol(error) | HandshakeError::ProofChanged(Some(error)) => {
            start_failure(error)
        }
        HandshakeError::ProofChanged(None) if expected => StartFailure {
            message: "Daemon proof changed".into(),
            recovery: super::Recovery::Blocked,
        },
        HandshakeError::ProofChanged(None) => StartFailure::retry("Daemon proof changed"),
        HandshakeError::Unanswered(message) | HandshakeError::Transport(message) => {
            StartFailure::retry(message)
        }
    }
}
/// An account switch whose connection failed before its request was
/// written. Nothing was asked of the daemon, so the conversation it would
/// have moved is untouched whatever the cause, as with [`switch_unadmitted`]
/// for the readiness check before it.
fn switch_unasked(error: HandshakeError) -> StartFailure {
    let (message, recovery) = match error {
        HandshakeError::Protocol(error) | HandshakeError::ProofChanged(Some(error))
            if error.is_daemon_busy() =>
        {
            (error.message, super::Recovery::Busy)
        }
        HandshakeError::Protocol(error) | HandshakeError::ProofChanged(Some(error))
            if error.code == ipc::ProtocolErrorCode::UnsupportedVersion =>
        {
            (error.message, super::Recovery::Unreached)
        }
        HandshakeError::Protocol(error) | HandshakeError::ProofChanged(Some(error)) => {
            (error.message, super::Recovery::NotAdmitted)
        }
        HandshakeError::ProofChanged(None) => {
            ("Daemon proof changed".into(), super::Recovery::Unreached)
        }
        HandshakeError::Unanswered(message) => (message, super::Recovery::Unresponsive),
        HandshakeError::Transport(message) => (message, super::Recovery::Unreached),
    };
    StartFailure { message, recovery }
}
/// Opens a native chat connection with its `Connect` handshake, and returns
/// the daemon's proof. No request has been written yet when this fails.
fn connect_handshake(
    stream: &mut UnixStream,
    codec: &mut FrameCodec,
    expected: Option<&HostSessionProof>,
) -> Result<HostSessionProof, HandshakeError> {
    set_chat_timeouts(stream)?;
    codec
        .write(
            stream,
            &ClientEnvelope {
                version: PROTOCOL_VERSION,
                request_id: 1,
                proof: None,
                request: ClientRequest::Connect {
                    expected_host: expected.cloned(),
                    resume: Vec::new(),
                },
            },
        )
        .map_err(|e| e.to_string())?;
    match receive_handshake(stream, codec, 1, expected)? {
        ServerResponse::Connected { proof, .. } if proof.is_complete() => Ok(proof),
        ServerResponse::Connected { .. } => {
            Err("Daemon returned an incomplete native chat ownership proof".into())
        }
        _ => Err("Daemon did not complete native chat handshake".into()),
    }
}
/// Opens a native chat on `stream`, handshaking only a bare connection, and
/// returns the stream with the daemon's proof. A proven connection meets
/// `expected` as the daemon would have answered its `Connect`.
fn chat_handshake(
    stream: ChatStream,
    codec: &mut FrameCodec,
    expected: Option<&HostSessionProof>,
) -> Result<(UnixStream, HostSessionProof), HandshakeError> {
    match stream {
        ChatStream::Bare(mut stream) => {
            connect_handshake(&mut stream, codec, expected).map(|proof| (stream, proof))
        }
        ChatStream::Proven { stream, proof } => {
            set_chat_timeouts(&stream)?;
            if !proof.is_complete() {
                return Err("Daemon returned an incomplete native chat ownership proof".into());
            }
            if let Some(expected) = expected {
                ipc::validate_proof(Some(expected), &proof)
                    .map_err(|error| HandshakeError::ProofChanged(Some(error)))?;
            }
            Ok((stream, proof))
        }
    }
}
fn set_chat_timeouts(stream: &UnixStream) -> Result<(), String> {
    stream
        .set_read_timeout(Some(CHAT_ATTACH_TIMEOUT))
        .map_err(|e| e.to_string())?;
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .map_err(|e| e.to_string())
}
fn receive_handshake(
    stream: &mut UnixStream,
    codec: &mut FrameCodec,
    request_id: u64,
    proof: Option<&HostSessionProof>,
) -> Result<ServerResponse, HandshakeError> {
    #[cfg(feature = "ui")]
    let health_request = crate::daemon_health::begin();
    let frame = codec.read(stream);
    #[cfg(feature = "ui")]
    match &frame {
        Err(error) => crate::daemon_health::frame(stream, Some(error), health_request),
        Ok(None) => crate::daemon_health::disconnected(stream, health_request),
        _ => {}
    }
    let envelope: ServerEnvelope = frame
        .map_err(|error| {
            let unanswered = frame_timed_out(&error);
            let message = frame_read_error(error, stream.read_timeout().ok().flatten());
            if unanswered {
                HandshakeError::Unanswered(message)
            } else {
                HandshakeError::Transport(message)
            }
        })?
        .ok_or("Daemon disconnected")?;
    if envelope.version != PROTOCOL_VERSION {
        #[cfg(feature = "ui")]
        crate::daemon_health::incompatible_stream(stream, envelope.version, health_request);
        return Err("Daemon version mismatch; use matched cagents executables".into());
    }
    if !envelope.proof.is_complete() {
        return Err(HandshakeError::ProofChanged(None));
    }
    if envelope.request_id != Some(request_id) {
        return Err("Unexpected daemon response ordering".into());
    }
    if proof.is_some_and(|proof| proof != &envelope.proof) {
        return Err(HandshakeError::ProofChanged(match envelope.message {
            ServerMessage::Error(error) => Some(error),
            _ => None,
        }));
    }
    match envelope.message {
        ServerMessage::Response { response } => {
            #[cfg(feature = "ui")]
            crate::daemon_health::frame(stream, None, health_request);
            Ok(response)
        }
        ServerMessage::Error(error) => {
            #[cfg(feature = "ui")]
            crate::daemon_health::frame(stream, None, health_request);
            Err(HandshakeError::Protocol(error))
        }
        _ => Err("Unexpected daemon handshake frame".into()),
    }
}

/// The daemon's own native configuration, which is what a conversation with no
/// proven source terminal has to start from.
fn native_environment(session: &Session, standalone: bool) -> ipc::ChatExecutionEnvironment {
    let mut environment = if standalone && session.terminal.is_none() {
        crate::model::agent_recovery::fresh_native_environment(session.launch.provider, false)
    } else {
        BTreeMap::new()
    };
    session.launch.recipe.withhold_environment(&mut environment);
    ipc::ChatExecutionEnvironment(environment)
}

/// Works out how a saved card rejoins its conversation, and returns the request
/// that does it.
///
/// The saved proof names the daemon incarnation that was hosting the
/// conversation. An application update replaces the executables and restarts
/// the daemon, so after one every saved proof is stale by definition and an
/// exact attach can only be refused. Attaching is therefore conditional on the
/// live proof; the daemon's own upgrade checkpoint is the second chance; and
/// starting the provider again on the saved conversation id is the answer when
/// neither holds, because the conversation itself is on disk and the provider
/// is the thing that can read it back.
fn restore(
    stream: &mut UnixStream,
    codec: &mut FrameCodec,
    attach_request: &mut u64,
    proof: &HostSessionProof,
    mut session: Session,
    upgrade_recovery: &mut Option<(Session, Session)>,
    resumed_from: &mut Option<Session>,
    intent: Intent,
) -> Result<ClientRequest, StartFailure> {
    let retry_explicit_start = intent == Intent::RetryStart;
    let existing_only = intent == Intent::RestoreExisting;
    if session.proof.as_ref() == Some(proof) && !retry_explicit_start {
        return Ok(ClientRequest::ChatAttach { session });
    }
    if crate::upgrade::floor::load().is_ok_and(|ledger| {
        ledger
            .resume_source(&session)
            .is_some_and(|original| original.launch.session_id.is_none())
    }) {
        return Err(StartFailure::retry(
            "This conversation was stopped for the update before it recorded a conversation. Its saved pane is kept; start a fresh conversation from the pane.",
        ));
    }
    let mut refusal: Option<StartFailure> = None;
    if retry_explicit_start
        || !session
            .proof
            .as_ref()
            .is_some_and(HostSessionProof::is_complete)
    {
        // Shared panes can be published before their first attachment records
        // ownership. Recover the daemon's exact descriptor before considering
        // a restart; a missing presentation proof does not mean no writer exists.
        let request_id = *attach_request;
        *attach_request += 1;
        codec
            .write(
                stream,
                &ClientEnvelope {
                    version: PROTOCOL_VERSION,
                    request_id,
                    proof: Some(proof.clone()),
                    request: ClientRequest::ChatList,
                },
            )
            .map_err(|error| error.to_string())?;
        let sessions = match receive_start(stream, codec, request_id, Some(proof))? {
            ServerResponse::ChatListed { sessions } => sessions,
            _ => return Err("Daemon did not return its native chat inventory".into()),
        };
        if let Some(hosted) = sessions.into_iter().find(|hosted| hosted.id == session.id) {
            if hosted.proof.as_ref() != Some(proof)
                || hosted.launch.provider != session.launch.provider
                || (hosted.launch.recipe != session.launch.recipe
                    && !session.resolved_to_owned_codex_listener(&hosted))
                || (session.launch.session_id.is_some()
                    && session.launch.session_id != hosted.launch.session_id)
            {
                return Err("The hosted chat does not match the exact saved conversation".into());
            }
            if retry_explicit_start && session.launch.provider == crate::agent::Provider::Claude {
                // A lost successful reply attaches the existing owner. A
                // genuinely failed named initialization may instead retain an
                // exact unfinished Close; only normal authenticated resume
                // preparation can finish it and return the original request.
                let request_id = *attach_request;
                *attach_request += 1;
                codec
                    .write(
                        stream,
                        &ClientEnvelope {
                            version: PROTOCOL_VERSION,
                            request_id,
                            proof: Some(proof.clone()),
                            request: ClientRequest::ChatPrepareResume {
                                source: hosted.clone(),
                            },
                        },
                    )
                    .map_err(|error| error.to_string())?;
                let prepared = match receive_start(stream, codec, request_id, Some(proof))? {
                    ServerResponse::ChatResumePrepared { session } => session,
                    _ => {
                        return Err(
                            "The failed start did not receive exact resume preparation".into()
                        );
                    }
                };
                if prepared == hosted {
                    if hosted.is_saved_claude_history() {
                        if hosted != session {
                            return Err(
                                "The saved-history reader changed the explicit retry request"
                                    .into(),
                            );
                        }
                        let execution_environment = native_environment(&hosted, true);
                        return Ok(ClientRequest::ChatStart {
                            session: hosted,
                            source: None,
                            terminate_source: false,
                            execution_environment,
                        });
                    }
                    return Ok(ClientRequest::ChatAttach { session: hosted });
                }
                if prepared == session {
                    let execution_environment = native_environment(&prepared, true);
                    return Ok(ClientRequest::ChatStart {
                        session: prepared,
                        source: None,
                        terminate_source: false,
                        execution_environment,
                    });
                }
                return Err("Resume preparation changed the exact failed-start request".into());
            }
            return Ok(ClientRequest::ChatAttach { session: hosted });
        }
    }
    if session
        .proof
        .as_ref()
        .is_some_and(HostSessionProof::is_complete)
    {
        let request_id = *attach_request;
        *attach_request += 1;
        codec
            .write(
                stream,
                &ClientEnvelope {
                    version: PROTOCOL_VERSION,
                    request_id,
                    proof: Some(proof.clone()),
                    request: ClientRequest::ResolveUpgradeChat {
                        source: session.clone(),
                    },
                },
            )
            .map_err(|error| error.to_string())?;
        match receive_start(stream, codec, request_id, Some(proof)) {
            Ok(ServerResponse::UpgradeChatResolved { session: restored })
                if restored.id == session.id
                    && restored.proof.as_ref() == Some(proof)
                    && restored.launch.provider == session.launch.provider =>
            {
                if retry_explicit_start && restored.is_saved_claude_history() {
                    if restored != session {
                        return Err(
                            "The saved-history reader changed the explicit retry request".into(),
                        );
                    }
                    let execution_environment = native_environment(&restored, true);
                    return Ok(ClientRequest::ChatStart {
                        session: restored,
                        source: None,
                        terminate_source: false,
                        execution_environment,
                    });
                }
                if session != restored {
                    if UpgradeRecovery::candidate(&session, &restored).is_none() {
                        return Err("The daemon's upgrade replacement changed the saved conversation configuration".into());
                    }
                    *upgrade_recovery = Some((session, restored.clone()));
                }
                return Ok(ClientRequest::ChatAttach { session: restored });
            }
            Ok(_) => {
                refusal = Some(StartFailure {
                    message: "The daemon answered this chat's upgrade lineage with a different conversation.".into(),
                    recovery: super::Recovery::Blocked,
                });
            }
            // A daemon an installer stopped and restarted never wrote an
            // upgrade checkpoint, which is the ordinary case after an
            // application update rather than a reason to give up.
            Err(message) => refusal = Some(message),
        }
    }
    if crate::upgrade::floor::load().is_ok_and(|ledger| ledger.resume_source(&session).is_some()) {
        return Err(StartFailure::retry(
            "This conversation was stopped for the update. Its saved files are kept; the transcript preview could not be attached. Nothing was restarted.",
        ));
    }
    if existing_only {
        // Read-only restoration must not start a provider, nor turn a typed
        // permanent refusal into permission to repeat the same request.
        return Err(refusal.unwrap_or_else(|| StartFailure::retry(
            "This recovery pane could not attach to an existing conversation. Its saved conversation is kept; use the pane's Start or Resume action to continue it explicitly. No exact daemon-owned conversation was available.",
        )));
    }
    if matches!(
        session.native_continuity,
        super::NativeContinuity::RetainedClaudeActor { .. }
    ) && session.launch.provider == crate::agent::Provider::Claude
    {
        return resume_retained_claude(
            stream,
            codec,
            attach_request,
            proof,
            session,
            resumed_from,
            refusal.map(|failure| failure.message),
        );
    }
    if session.native_continuity.retains_provider() {
        let mut failure = refusal.unwrap_or_else(|| {
            StartFailure::retry("No authenticated native holder was available.")
        });
        failure.message = format!(
            "The saved native conversation could not be reattached. Its uncertain work was not restarted or resubmitted. {}",
            failure.message
        );
        return Err(failure);
    }
    if session.launch.provider == crate::agent::Provider::Claude && !retry_explicit_start {
        let mut failure = refusal.unwrap_or_else(|| {
            StartFailure::retry("No authenticated history attachment was available.")
        });
        failure.message = format!(
            "Saved Claude history could not be opened without starting a provider. Its exact conversation and draft are retained. {}",
            failure.message
        );
        return Err(failure);
    }
    if session.launch.session_id.is_none() {
        return Err(StartFailure::fresh(format!(
            "The daemon that was hosting this conversation is no longer running, and this card never recorded a conversation to resume. {}",
            refusal
                .map(|failure| failure.message)
                .unwrap_or_else(|| "No saved daemon proof was available either.".to_owned())
        )));
    }
    // The live daemon proved its identity during the handshake, so the start
    // carries that proof rather than the one this card saved before the
    // restart; the daemon validates it whenever it still holds a closed record
    // under this chat's id.
    session.proof = Some(proof.clone());
    let execution_environment = native_environment(&session, true);
    Ok(ClientRequest::ChatStart {
        session,
        source: None,
        terminate_source: false,
        execution_environment,
    })
}
/// A retained Claude card whose helper no daemon hosts any more, typically
/// because an application update or crash outlived it.
///
/// Its conversation is still on disk, so the card asks the daemon for the same
/// proof-checked successor the resume picker uses: the daemon only prepares
/// one when the old owner's exit is proved (a previous kernel boot, a verified
/// retirement, or an exited helper and provider with no surviving process
/// group), and allocates it under a new identity so the old allocation's
/// census is never reused. Nothing the old actor held is resubmitted: the
/// successor starts the provider on the saved conversation with no input.
/// A card that never learned its conversation ID resumes the one the daemon
/// proves its ended actor confirmed, or starts one in place when the daemon
/// proves that actor confirmed none; it never becomes a draft.
fn resume_retained_claude(
    stream: &mut UnixStream,
    codec: &mut FrameCodec,
    attach_request: &mut u64,
    proof: &HostSessionProof,
    session: Session,
    resumed_from: &mut Option<Session>,
    refusal: Option<String>,
) -> Result<ClientRequest, StartFailure> {
    // An unnamed card also goes to the daemon: only its retirement evidence
    // proves which conversation, if any, the ended actor confirmed. Claude can
    // write a transcript before it reports the conversation ID, so the card
    // never infers a draft from the missing ID itself.
    let request_id = *attach_request;
    *attach_request += 1;
    codec
        .write(
            stream,
            &ClientEnvelope {
                version: PROTOCOL_VERSION,
                request_id,
                proof: Some(proof.clone()),
                request: ClientRequest::ChatPrepareResume {
                    source: session.clone(),
                },
            },
        )
        .map_err(|error| error.to_string())?;
    let prepared = match receive_start(stream, codec, request_id, Some(proof)) {
        Ok(ServerResponse::ChatResumePrepared { session: prepared }) => prepared,
        Ok(_) => return Err("The daemon did not answer the saved conversation's resume".into()),
        Err(mut failure) => {
            // "Yet" only where a later attempt can succeed.
            failure.message = format!(
                "The saved conversation could not be reopened{}, so nothing was restarted or resent. {}",
                if failure.recovery == super::Recovery::Blocked {
                    ""
                } else {
                    " yet"
                },
                failure.message
            );
            return Err(failure);
        }
    };
    if prepared == session {
        // The daemon still sees an owner for this allocation. It is restored,
        // never replaced, so the card waits for that owner rather than racing
        // it. When no daemon can take that owner over, closing the pane is
        // the way out: it stops the helper and keeps the saved conversation.
        return Err(StartFailure::retry(format!(
            "An earlier process still holds this conversation, so nothing was started in its place. {} If retrying does not reconnect it, close this pane to end that process; the conversation stays saved and reopens from Resume.",
            refusal.unwrap_or_else(|| "Retry once it has finished closing.".to_owned())
        )));
    }
    if !super::is_retained_resume_successor(&session, &prepared) {
        return Err("The daemon's resume changed the saved conversation".into());
    }
    *resumed_from = Some(session);
    let execution_environment = native_environment(&prepared, true);
    Ok(ClientRequest::ChatStart {
        session: prepared,
        source: None,
        terminate_source: false,
        execution_environment,
    })
}

impl ClientHandle {
    /// A handle whose snapshot the UI can render without a daemon.
    ///
    /// The peer sockets are kept open for the process lifetime so dropping the
    /// handle does not look like a protocol failure to a test that only draws.
    #[cfg(any(test, feature = "test-support"))]
    #[doc(hidden)]
    pub fn fixture(session: Session, snapshot: Snapshot) -> Self {
        let retirement = ClientRetirement::shared().expect("start fixture client retirement");
        let body_requested = Arc::new(AtomicBool::new(true));
        let user_item_seen = snapshot
            .items
            .iter()
            .any(|item| item.role == super::Role::User);
        let (stream, peer) = UnixStream::pair().expect("socket pair");
        let (cancel, cancel_peer) = UnixStream::pair().expect("socket pair");
        let (visibility_tx, visibility_rx) = crossbeam_channel::bounded(1);
        std::mem::forget(peer);
        std::mem::forget(cancel_peer);
        std::mem::forget(visibility_rx);
        let proof = HostSessionProof {
            host_session_id: crate::model::HostSessionId::new("fixture-host"),
            boot_id: crate::model::BootId::new("fixture-boot"),
            host_incarnation: crate::model::HostIncarnation::new("fixture-incarnation"),
        };
        Self {
            inner: Arc::new(Inner::new(
                InnerPayload {
                    id: session.id,
                    session,
                    upgrade_recovery: None,
                    resumed_from: None,
                    proof,
                    wire: Arc::new(Mutex::new(Wire {
                        stream,
                        codec: FrameCodec::new(ipc::MAX_FRAME_BYTES),
                        body_requested: body_requested.clone(),
                        announced: false,
                        hidings: 0,
                    })),
                    cancel,
                    publication: Arc::new(Publications::new(snapshot)),
                    body_requested,
                    user_item_seen: Arc::new(AtomicBool::new(user_item_seen)),
                    prompt_accepted: Arc::new(AtomicBool::new(false)),
                    activity: Arc::new(Mutex::new(ActivityObservation {
                        generation: 0,
                        state: ActivityState::Idle,
                        pending: false,
                    })),
                    observed: Arc::new(Mutex::new(OutputObservation::default())),
                    account_usage: Arc::default(),
                    limit: Arc::default(),
                    agent_addresses: Arc::default(),
                    next: Arc::new(AtomicU64::new(1)),
                    alive: Arc::new(AtomicBool::new(true)),
                    visible: Arc::new(AtomicBool::new(true)),
                    wake: Arc::new(|_| {}),
                    reader: Mutex::new(None),
                    pending: Arc::new(Mutex::new(BTreeMap::new())),
                    file_pending: Arc::new(Mutex::new(BTreeMap::new())),
                    text_copy_pending: Arc::new(Mutex::new(BTreeMap::new())),
                    text_copies: AtomicUsize::new(0),
                    visibility_tx,
                    visibility_worker: Mutex::new(None),
                    retirement_complete: None,
                },
                retirement,
            )),
        }
    }

    #[cfg(any(test, feature = "test-support"))]
    #[doc(hidden)]
    pub fn fixture_upgrade(source: Session, session: Session, snapshot: Snapshot) -> Self {
        assert!(super::is_upgrade_recovery(&source, &session));
        assert_eq!(snapshot.session_id, session.launch.session_id);
        let proof = session.proof.clone().unwrap();
        let mut handle = Self::fixture(session.clone(), snapshot);
        let inner = Arc::get_mut(&mut handle.inner).unwrap();
        inner.proof = proof;
        inner.upgrade_recovery = Some(UpgradeRecovery {
            sessions: (source, session),
            kind: UpgradeRecoveryKind::Exact,
        });
        handle
    }

    /// A started resume successor of the stranded retained card `source`.
    #[cfg(any(test, feature = "test-support"))]
    #[doc(hidden)]
    pub fn fixture_resumed(source: Session, session: Session, snapshot: Snapshot) -> Self {
        assert!(super::is_retained_resume_commit(&source, &session));
        let proof = session.proof.clone().unwrap();
        let mut handle = Self::fixture(session, snapshot);
        let inner = Arc::get_mut(&mut handle.inner).unwrap();
        inner.proof = proof;
        inner.resumed_from = Some(source);
        handle
    }

    /// Drives the production connect/lineage/attach path for UI writer tests.
    #[cfg(any(test, feature = "test-support"))]
    #[doc(hidden)]
    pub fn fixture_upgrade_handshake(
        source: Session,
        session: Session,
        snapshot: Snapshot,
    ) -> Result<Self, StartFailure> {
        fixture::restore_with_lineage(source, Some(session), move |request| {
            let ClientRequest::ChatAttach { session } = request else {
                panic!("expected exact upgrade attach");
            };
            ServerMessage::Response {
                response: ServerResponse::ChatAttached { session, snapshot },
            }
        })
    }

    /// Test-only access to the real start handshake and reader on a private
    /// socket. It never discovers a daemon or launches a provider.
    #[cfg(any(test, feature = "test-support"))]
    #[doc(hidden)]
    pub fn fixture_start_on(
        stream: UnixStream,
        session: Session,
        wake: impl Fn(Wake) + Send + Sync + 'static,
    ) -> Result<Self, StartFailure> {
        Self::connect_on(
            stream,
            session,
            Intent::Start,
            None,
            false,
            None,
            None,
            None,
            wake,
        )
    }

    /// Publishes `snapshot` as the reader thread would on a presentation
    /// frame, so a test can stream a transcript into a surface.
    #[cfg(any(test, feature = "test-support"))]
    #[doc(hidden)]
    pub fn publish_snapshot_for_test(&self, snapshot: Snapshot) {
        self.install_snapshot_for_test(snapshot);
    }

    /// Apply a fixture publication through the production delta validator and
    /// append-lineage recorder. A full-snapshot fixture intentionally resets
    /// that lineage and cannot stand in for a streaming delta benchmark.
    #[cfg(any(test, feature = "test-support"))]
    #[doc(hidden)]
    pub fn publish_presentation_for_test(&self, update: super::presentation::Presentation) {
        let (base, revision) = self.inner.publication.capture();
        let mut lineage = base.lineage.clone();
        let candidates = append::Lineage::candidates(&update, &base.snapshot);
        let next = update
            .apply(&base.snapshot)
            .expect("fixture publication obeys production fences");
        lineage.record(candidates, &next);
        let full = Publication::next(&base, next, true, lineage);
        let metadata = full.hidden.as_ref().unwrap().clone();
        self.inner.publication.publish(
            &base,
            revision,
            &self.inner.body_requested,
            Some(full),
            metadata,
            false,
        );
    }

    /// Build normal daemon-style deltas while retaining the production
    /// transaction validator and weak append authority. Full reset fixtures
    /// deliberately keep using publish_snapshot_for_test instead.
    #[cfg(any(test, feature = "test-support"))]
    #[doc(hidden)]
    pub fn publish_delta_snapshot_for_test(&self, mut next: Snapshot) {
        use super::presentation::{ChildUpdate, ItemChange, Presentation};
        let previous = self.snapshot();
        let changes = |old: &[Arc<super::Item>], items: &[Arc<super::Item>]| {
            let old: std::collections::HashMap<_, _> = old
                .iter()
                .map(|item| (item.id.as_str(), item.as_ref()))
                .collect();
            items
                .iter()
                .filter_map(|item| {
                    let before = old.get(item.id.as_str()).copied();
                    (before != Some(item.as_ref())).then(|| ItemChange::between(before, item))
                })
                .collect()
        };
        let order = next.items.iter().map(|item| item.id.clone()).collect();
        let parent = changes(&previous.items, &next.items);
        next.items.clear();
        let children = next
            .subagents
            .iter_mut()
            .map(|child| {
                let old = previous
                    .subagents
                    .iter()
                    .find(|old| old.id == child.id)
                    .map_or(&[][..], |old| old.items.as_slice());
                let update = ChildUpdate {
                    id: child.id.clone(),
                    order: child.items.iter().map(|item| item.id.clone()).collect(),
                    changes: changes(old, &child.items),
                };
                child.items.clear();
                update
            })
            .collect();
        self.publish_presentation_for_test(Presentation {
            base_generation: previous.generation,
            file_changes_unchanged: false,
            metadata: next,
            order,
            changes: parent,
            children,
        });
    }

    /// Queues a report as the reader thread would on its arrival, and says
    /// whether it would have woken the surface.
    #[cfg(any(test, feature = "test-support"))]
    #[doc(hidden)]
    pub fn push_account_usage_for_test(&self, report: LiveReport, observed_at: u64) -> bool {
        self.inner
            .account_usage
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(report, observed_at)
    }

    /// Records the subagent addresses the daemon sent as the reader thread
    /// would on their arrival.
    #[cfg(any(test, feature = "test-support"))]
    #[doc(hidden)]
    pub fn learn_agent_addresses_for_test(&self, addresses: Vec<super::AgentAddress>) {
        learn_addresses(
            &mut self
                .inner
                .agent_addresses
                .lock()
                .unwrap_or_else(|p| p.into_inner()),
            addresses,
        );
    }

    /// Records the daemon's judgement of a usage limit as the reader thread
    /// would on its arrival.
    #[cfg(any(test, feature = "test-support"))]
    #[doc(hidden)]
    pub fn judge_limit_for_test(
        &self,
        limit: Option<crate::agent_execution::directory::UsageLimit>,
    ) {
        *self.inner.limit.lock().unwrap_or_else(|p| p.into_inner()) = limit;
    }

    /// Records an activity edge as the reader thread would, and says whether
    /// it would have woken the surface.
    #[cfg(any(test, feature = "test-support"))]
    #[doc(hidden)]
    pub fn observe_activity_for_test(&self, state: ActivityState) -> bool {
        let mut activity = self
            .inner
            .activity
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let generation = activity.generation;
        activity.observe(generation, state)
    }

    /// Whether the handle has asked its daemon to keep the snapshot current.
    #[cfg(any(test, feature = "test-support"))]
    #[doc(hidden)]
    pub fn is_visible_for_test(&self) -> bool {
        self.inner.visible.load(Ordering::Acquire)
    }

    /// Marks the connection lost, as the reader thread does when its daemon
    /// goes away.
    #[cfg(any(test, feature = "test-support"))]
    #[doc(hidden)]
    pub fn disconnect_for_test(&self) {
        self.inner.alive.store(false, Ordering::Release);
    }

    /// Deliver a full snapshot the way the reader thread takes a daemon's
    /// `ChatSnapshot` reply, sharing unchanged items with a kept body.
    #[cfg(any(test, feature = "test-support"))]
    #[doc(hidden)]
    pub fn deliver_snapshot_for_test(&self, snapshot: Snapshot) {
        self.inner
            .publication
            .replace(snapshot, &self.inner.body_requested, false, false);
    }

    #[cfg(any(test, feature = "test-support"))]
    #[doc(hidden)]
    pub fn replace_snapshot_for_test(&self, snapshot: Snapshot) {
        self.install_snapshot_for_test(snapshot);
    }

    /// Observe disposal of this exact publication, independently of the UI's
    /// presentation reclaimer. The signal follows destruction of its payload.
    #[cfg(any(test, feature = "test-support"))]
    #[doc(hidden)]
    pub fn observe_snapshot_retirement_for_test(
        &self,
    ) -> std::sync::mpsc::Receiver<std::thread::ThreadId> {
        let (completed, observed) = std::sync::mpsc::channel();
        let publication = self.inner.publication.current.load();
        assert!(
            publication.resident,
            "observe a resident transcript publication"
        );
        assert!(
            publication
                .retirement_complete
                .lock()
                .unwrap()
                .replace(completed)
                .is_none()
        );
        observed
    }

    /// Read-only metadata lookup. Does not subscribe, resume, or write a turn.
    pub fn preview(
        session: Session,
        source: HostSessionProof,
    ) -> Result<super::BindingPreview, String> {
        let socket = ipc::daemon_socket_path().map_err(|error| error.to_string())?;
        let mut stream = connect_daemon(&socket).map_err(|error| error.to_string())?;
        stream
            .set_read_timeout(Some(Duration::from_secs(20)))
            .map_err(|error| error.to_string())?;
        stream
            .set_write_timeout(Some(Duration::from_secs(5)))
            .map_err(|error| error.to_string())?;
        let mut codec = FrameCodec::new(ipc::MAX_FRAME_BYTES);
        codec
            .write(
                &mut stream,
                &ClientEnvelope {
                    version: PROTOCOL_VERSION,
                    request_id: 1,
                    proof: None,
                    request: ClientRequest::Connect {
                        expected_host: Some(source.clone()),
                        resume: Vec::new(),
                    },
                },
            )
            .map_err(|error| error.to_string())?;
        match receive(&mut stream, &mut codec, 1, Some(&source))? {
            ServerResponse::Connected { proof, .. } if proof == source => {}
            _ => return Err("The source daemon did not confirm its identity".into()),
        }
        codec
            .write(
                &mut stream,
                &ClientEnvelope {
                    version: PROTOCOL_VERSION,
                    request_id: 2,
                    proof: Some(source.clone()),
                    request: ClientRequest::ChatPreview {
                        session,
                        source: source.clone(),
                    },
                },
            )
            .map_err(|error| error.to_string())?;
        match receive(&mut stream, &mut codec, 2, Some(&source))? {
            ServerResponse::ChatPreviewed { preview } => Ok(preview),
            _ => Err("The daemon did not return a verified conversation preview".into()),
        }
    }

    /// Read-only listing of the source account's open conversations. It returns
    /// no token, so a listed row still has to be previewed before it attaches.
    pub fn candidates(
        session: Session,
        source: HostSessionProof,
    ) -> Result<Vec<super::BindingCandidate>, String> {
        let socket = ipc::daemon_socket_path().map_err(|error| error.to_string())?;
        let mut stream = connect_daemon(&socket).map_err(|error| error.to_string())?;
        stream
            .set_read_timeout(Some(Duration::from_secs(20)))
            .map_err(|error| error.to_string())?;
        stream
            .set_write_timeout(Some(Duration::from_secs(5)))
            .map_err(|error| error.to_string())?;
        let mut codec = FrameCodec::new(ipc::MAX_FRAME_BYTES);
        codec
            .write(
                &mut stream,
                &ClientEnvelope {
                    version: PROTOCOL_VERSION,
                    request_id: 1,
                    proof: None,
                    request: ClientRequest::Connect {
                        expected_host: Some(source.clone()),
                        resume: Vec::new(),
                    },
                },
            )
            .map_err(|error| error.to_string())?;
        match receive(&mut stream, &mut codec, 1, Some(&source))? {
            ServerResponse::Connected { proof, .. } if proof == source => {}
            _ => return Err("The source daemon did not confirm its identity".into()),
        }
        codec
            .write(
                &mut stream,
                &ClientEnvelope {
                    version: PROTOCOL_VERSION,
                    request_id: 2,
                    proof: Some(source.clone()),
                    request: ClientRequest::ChatCandidates {
                        session,
                        source: source.clone(),
                    },
                },
            )
            .map_err(|error| error.to_string())?;
        match receive(&mut stream, &mut codec, 2, Some(&source))? {
            ServerResponse::ChatCandidates { candidates } => Ok(candidates),
            _ => Err("The daemon did not return a verified conversation list".into()),
        }
    }

    pub fn connect(
        session: Session,
        intent: Intent,
        source: Option<HostSessionProof>,
        terminate_source: bool,
        selection: Option<String>,
        wake: impl Fn(Wake) + Send + Sync + 'static,
    ) -> Result<Self, StartFailure> {
        // A restore runs right after an application update has stopped the
        // daemon it is about to ask for, so it starts one for the same reason a
        // new conversation does instead of failing on an absent socket.
        Self::connect_on(
            chat_stream(intent)?,
            session,
            intent,
            source,
            terminate_source,
            selection,
            None,
            None,
            wake,
        )
    }

    pub fn connect_in_pane(
        pane: ipc::PaneChatStart,
        session: Session,
        wake: impl Fn(Wake) + Send + Sync + 'static,
    ) -> Result<Self, StartFailure> {
        let ready = crate::terminal::connect_ready_daemon().map_err(|error| error.to_string())?;
        Self::connect_on_with_pane(
            ready,
            session,
            Intent::Start,
            None,
            false,
            None,
            None,
            None,
            Some(pane),
            wake,
        )
    }

    /// Attach only the exact canonical replacement authenticated from this
    /// saved source. Unlike ordinary restore, this never starts a provider.
    pub fn connect_upgrade(
        source: Session,
        expected: Session,
        wake: impl Fn(Wake) + Send + Sync + 'static,
    ) -> Result<Self, StartFailure> {
        Self::connect_upgrade_with_intent(source, expected, Intent::Restore, wake)
    }

    pub fn connect_upgrade_with_intent(
        source: Session,
        expected: Session,
        intent: Intent,
        wake: impl Fn(Wake) + Send + Sync + 'static,
    ) -> Result<Self, StartFailure> {
        if !matches!(intent, Intent::Restore | Intent::RestoreExisting) {
            return Err("Canonical recovery must attach an existing conversation".into());
        }
        if UpgradeRecovery::candidate(&source, &expected).is_none() {
            return Err("The canonical chat is not an exact upgrade of the saved source".into());
        }
        Self::connect_on(
            chat_stream(intent)?,
            source,
            intent,
            None,
            false,
            None,
            None,
            Some(expected),
            wake,
        )
    }

    pub fn recover_account(
        source: Session,
        replacement: Session,
        wake: impl Fn(Wake) + Send + Sync + 'static,
    ) -> Result<Self, StartFailure> {
        if !super::is_account_recovery(&source, &replacement) {
            return Err("Account recovery changed the saved conversation".into());
        }
        Self::connect_on(
            chat_stream(Intent::Start)?,
            replacement,
            Intent::Start,
            None,
            false,
            None,
            Some((source, None)),
            None,
            wake,
        )
    }

    pub fn switch_account(
        source: Session,
        replacement: Session,
        generation: u64,
        wake: impl Fn(Wake) + Send + Sync + 'static,
    ) -> Result<Self, StartFailure> {
        if !super::is_account_recovery(&source, &replacement) {
            return Err(StartFailure {
                message: "Account switch changed the saved conversation".into(),
                recovery: super::Recovery::NotAdmitted,
            });
        }
        let ready = crate::terminal::connect_ready_daemon_for_explicit_action()
            .map_err(switch_unadmitted)?;
        Self::connect_on(
            ready,
            replacement,
            Intent::Start,
            None,
            false,
            None,
            Some((source, Some(generation))),
            None,
            wake,
        )
    }

    /// Exercise the real restore handshake against an in-process daemon in
    /// host tests, without discovering or launching any external daemon.
    #[cfg(all(test, feature = "host"))]
    pub(crate) fn fixture_restore(
        stream: UnixStream,
        session: Session,
    ) -> Result<Self, StartFailure> {
        Self::connect_on(
            stream,
            session,
            Intent::Restore,
            None,
            false,
            None,
            None,
            None,
            |_| {},
        )
    }

    #[cfg(all(test, feature = "host"))]
    pub(crate) fn fixture_connect(
        stream: UnixStream,
        session: Session,
        intent: Intent,
        source: Option<HostSessionProof>,
    ) -> Result<Self, StartFailure> {
        Self::connect_on(
            stream,
            session,
            intent,
            source,
            false,
            None,
            None,
            None,
            |_| {},
        )
    }

    fn connect_on(
        stream: impl Into<ChatStream>,
        session: Session,
        intent: Intent,
        source: Option<HostSessionProof>,
        terminate_source: bool,
        selection: Option<String>,
        account_recovery: Option<(Session, Option<u64>)>,
        upgrade_expected: Option<Session>,
        wake: impl Fn(Wake) + Send + Sync + 'static,
    ) -> Result<Self, StartFailure> {
        Self::connect_on_with_pane(
            stream,
            session,
            intent,
            source,
            terminate_source,
            selection,
            account_recovery,
            upgrade_expected,
            None,
            wake,
        )
    }

    fn connect_on_with_pane(
        stream: impl Into<ChatStream>,
        session: Session,
        intent: Intent,
        source: Option<HostSessionProof>,
        terminate_source: bool,
        selection: Option<String>,
        account_recovery: Option<(Session, Option<u64>)>,
        upgrade_expected: Option<Session>,
        pane: Option<ipc::PaneChatStart>,
        wake: impl Fn(Wake) + Send + Sync + 'static,
    ) -> Result<Self, StartFailure> {
        // Nothing has been asked of the daemon until the request below is
        // written. A switch that fails before then moved nothing.
        let unasked = account_recovery.is_some();
        let retirement = ClientRetirement::for_client(session.id).map_err(|message| {
            if unasked {
                StartFailure {
                    message,
                    recovery: super::Recovery::NotAdmitted,
                }
            } else {
                message.into()
            }
        })?;
        if intent == Intent::RestoreExisting
            && (source.is_some()
                || terminate_source
                || selection.is_some()
                || account_recovery.is_some())
        {
            return Err("Recovery restoration may only attach existing conversations".into());
        }
        if intent == Intent::RetryStart
            && (source.is_some()
                || terminate_source
                || selection.is_some()
                || account_recovery.is_some()
                || upgrade_expected.is_some()
                || session.terminal.is_some()
                || session.launch.session_id.is_none()
                || session.native_continuity.retains_provider())
        {
            return Err("Only an exact explicit history start can be retried".into());
        }
        if intent == Intent::Resume
            && (source.is_some()
                || terminate_source
                || selection.is_some()
                || account_recovery.is_some()
                || upgrade_expected.is_some()
                || session.launch.provider != crate::agent::Provider::Claude
                || !matches!(
                    session.native_continuity,
                    super::NativeContinuity::RetainedClaudeActor { .. }
                ))
        {
            return Err(StartFailure::from(
                "Only a Claude conversation whose provider stopped can be resumed in its card",
            ));
        }
        if let Some(expected) = &upgrade_expected
            && (!matches!(intent, Intent::Restore | Intent::RestoreExisting)
                || source.is_some()
                || terminate_source
                || selection.is_some()
                || account_recovery.is_some()
                || UpgradeRecovery::candidate(&session, expected).is_none())
        {
            return Err("The canonical chat is not an exact upgrade of the saved source".into());
        }
        let wake: Arc<dyn Fn(Wake) + Send + Sync> = Arc::new(wake);
        let mut codec = FrameCodec::new(ipc::MAX_FRAME_BYTES);
        let expected = upgrade_expected
            .as_ref()
            .and_then(|session| session.proof.clone())
            .or_else(|| source.clone());
        let (mut stream, proof) = chat_handshake(stream.into(), &mut codec, expected.as_ref())
            .map_err(|error| {
                if unasked {
                    switch_unasked(error)
                } else {
                    connect_failure(error, expected.is_some())
                }
            })?;
        let mut session = session;
        if intent == Intent::Start
            && source.is_none()
            && session.terminal.is_some()
            && session.owns_codex_listener()
        {
            // A stopped CLI has no live PTY proof. The daemon must match the
            // retained actor and exact descriptor under this authenticated epoch.
            session.proof = Some(proof.clone());
        }
        let mut id = session.id;
        let mut attach_request = 2;
        let mut upgrade_recovery = None;
        let mut resumed_from = None;
        let request = if let Some(expected) = upgrade_expected {
            if expected.proof.as_ref() != Some(&proof) {
                return Err("The canonical chat belongs to a different daemon".into());
            }
            let request_id = attach_request;
            attach_request += 1;
            codec
                .write(
                    &mut stream,
                    &ClientEnvelope {
                        version: PROTOCOL_VERSION,
                        request_id,
                        proof: Some(proof.clone()),
                        request: ClientRequest::ResolveUpgradeChat {
                            source: session.clone(),
                        },
                    },
                )
                .map_err(|error| error.to_string())?;
            match receive_start(&mut stream, &mut codec, request_id, Some(&proof))? {
                ServerResponse::UpgradeChatResolved { session: resolved }
                    if resolved == expected => {}
                _ => return Err("The daemon did not authenticate the exact canonical chat".into()),
            }
            upgrade_recovery = Some((session, expected.clone()));
            ClientRequest::ChatAttach { session: expected }
        } else if let Some((source, idle_generation)) = account_recovery {
            let execution_environment = native_environment(&session, true);
            ClientRequest::ChatRecoverAccount {
                source,
                replacement: session,
                idle_generation,
                execution_environment,
            }
        } else if let Some(token) = selection {
            if intent != Intent::Start || terminate_source {
                return Err("Selected conversation activation requires a non-destructive shared-server attachment".into());
            }
            ClientRequest::ChatStartSelected {
                session,
                source: source
                    .ok_or("Selected conversation activation requires a proven source terminal")?,
                token,
            }
        } else if intent == Intent::Resume {
            // Only this daemon's own proof names the ended actor it still
            // holds. A card from an earlier incarnation has no actor here to
            // retire, and its ordinary restore already resumes it from disk.
            if session.proof.as_ref() != Some(&proof) {
                restore(
                    &mut stream,
                    &mut codec,
                    &mut attach_request,
                    &proof,
                    session,
                    &mut upgrade_recovery,
                    &mut resumed_from,
                    Intent::Restore,
                )?
            } else {
                resume_retained_claude(
                    &mut stream,
                    &mut codec,
                    &mut attach_request,
                    &proof,
                    session,
                    &mut resumed_from,
                    None,
                )?
            }
        } else if intent == Intent::Start
            && let Ok(ledger) = crate::upgrade::floor::load()
            && let Some(original) = ledger.resume_source(&session).cloned()
        {
            let request_id = attach_request;
            attach_request += 1;
            codec
                .write(
                    &mut stream,
                    &ClientEnvelope {
                        version: PROTOCOL_VERSION,
                        request_id,
                        proof: Some(proof.clone()),
                        request: ClientRequest::ChatPrepareResume { source: original },
                    },
                )
                .map_err(|error| error.to_string())?;
            let prepared = match receive(&mut stream, &mut codec, request_id, Some(&proof))? {
                ServerResponse::ChatResumePrepared { session: prepared }
                    if super::is_retained_resume_successor(&session, &prepared) =>
                {
                    prepared
                }
                _ => return Err(
                    "The stopped conversation did not receive its exact proof-checked successor"
                        .into(),
                ),
            };
            resumed_from = Some(session);
            let execution_environment = native_environment(&prepared, true);
            ClientRequest::ChatStart {
                session: prepared,
                source: None,
                terminate_source: false,
                execution_environment,
            }
        } else if intent == Intent::Start {
            let execution_environment = native_environment(&session, source.is_none());
            match pane {
                Some(pane) => ClientRequest::ChatStartInPane {
                    pane,
                    session,
                    execution_environment,
                },
                None => ClientRequest::ChatStart {
                    session,
                    source,
                    terminate_source,
                    execution_environment,
                },
            }
        } else {
            restore(
                &mut stream,
                &mut codec,
                &mut attach_request,
                &proof,
                session,
                &mut upgrade_recovery,
                &mut resumed_from,
                intent,
            )?
        };
        if let ClientRequest::ChatStart { session, .. } = &request
            && resumed_from.is_some()
        {
            // A proved successor is attached under the identity the daemon
            // allocated for it, never the stranded allocation's.
            id = session.id;
        }
        let pane_retry =
            matches!(&request, ClientRequest::ChatStartInPane { .. }).then(|| request.clone());
        let pane_deadline = Instant::now() + Duration::from_secs(15);
        let mut pane_wait = Duration::from_millis(50);
        codec
            .write(
                &mut stream,
                &ClientEnvelope {
                    version: PROTOCOL_VERSION,
                    request_id: attach_request,
                    proof: Some(proof.clone()),
                    request,
                },
            )
            .map_err(|e| e.to_string())?;
        // Activity can race the attach response after subscription is installed.
        // Keep only the newest scalar state; the snapshot generation fences any
        // edge older than the handshake snapshot.
        let mut early_activity = None;
        let mut early_observed = OutputObservation::default();
        let mut early_usage = AccountUsage::default();
        let mut early_limit = None;
        let mut early_addresses = AgentAddresses::default();
        let mut early_model = None;
        let (session, mut initial) = loop {
            #[cfg(feature = "ui")]
            let health_request = crate::daemon_health::begin();
            let frame = codec.read(&mut stream);
            #[cfg(feature = "ui")]
            match &frame {
                Err(error) => crate::daemon_health::frame(&stream, Some(error), health_request),
                Ok(None) => crate::daemon_health::disconnected(&stream, health_request),
                _ => {}
            }
            let envelope: ServerEnvelope = frame
                .map_err(|error| frame_read_error(error, stream.read_timeout().ok().flatten()))?
                .ok_or("Daemon disconnected")?;
            if envelope.version != PROTOCOL_VERSION {
                #[cfg(feature = "ui")]
                crate::daemon_health::incompatible_stream(
                    &stream,
                    envelope.version,
                    health_request,
                );
                return Err("Daemon version mismatch; use matched cagents executables".into());
            }
            if envelope.proof != proof {
                return Err("Daemon identity changed during native chat attachment".into());
            }
            match envelope.message {
                ServerMessage::ChatActivity {
                    chat_id,
                    state,
                    generation,
                } if chat_id == id && envelope.request_id.is_none() => {
                    early_activity = Some((generation, state));
                }
                ServerMessage::ChatObserved { chat_id, epoch }
                    if chat_id == id && envelope.request_id.is_none() =>
                {
                    early_observed.observe(epoch);
                }
                ServerMessage::ChatActiveModel {
                    chat_id,
                    revision,
                    model,
                } if chat_id == id && envelope.request_id.is_none() => {
                    if early_model
                        .as_ref()
                        .is_none_or(|(known, _)| *known < revision)
                    {
                        early_model = Some((revision, model));
                    }
                }
                ServerMessage::ChatAccountUsage {
                    chat_id,
                    report,
                    observed_at,
                } if chat_id == id && envelope.request_id.is_none() => {
                    early_usage.push(report, observed_at);
                }
                ServerMessage::ChatLimit { chat_id, limit }
                    if chat_id == id && envelope.request_id.is_none() =>
                {
                    early_limit = limit;
                }
                ServerMessage::ChatAgentAddresses { chat_id, addresses }
                    if chat_id == id && envelope.request_id.is_none() =>
                {
                    learn_addresses(&mut early_addresses, addresses);
                }
                ServerMessage::Response {
                    response: ServerResponse::ChatAttached { session, snapshot },
                } if session.id == id && envelope.request_id == Some(attach_request) => {
                    #[cfg(feature = "ui")]
                    crate::daemon_health::frame(&stream, None, health_request);
                    break (session, snapshot);
                }
                ServerMessage::Error(error) if envelope.request_id == Some(attach_request) => {
                    #[cfg(feature = "ui")]
                    crate::daemon_health::frame(&stream, None, health_request);
                    if error.code == ipc::ProtocolErrorCode::QueueFull
                        && error.retryable
                        && Instant::now() < pane_deadline
                        && let Some(request) = &pane_retry
                    {
                        // The model publisher may still be registering the pane.
                        // This correlated refusal started nothing; retry this exact
                        // candidate off the UI thread, never a fresh identity.
                        thread::sleep(pane_wait);
                        pane_wait = (pane_wait * 2).min(Duration::from_millis(200));
                        codec
                            .write(
                                &mut stream,
                                &ClientEnvelope {
                                    version: PROTOCOL_VERSION,
                                    request_id: attach_request,
                                    proof: Some(proof.clone()),
                                    request: request.clone(),
                                },
                            )
                            .map_err(|error| error.to_string())?;
                        continue;
                    }
                    let mut failure = start_failure(error);
                    if intent == Intent::RestoreExisting && !failure.recovery.is_blocked() {
                        // An ordinary recovery attachment cannot enter the
                        // account-recovery path, whose callback starts a
                        // provider. Account changes remain explicit actions.
                        // A typed permanent refusal stays blocked: repeating
                        // the same attachment cannot succeed.
                        failure.recovery = super::Recovery::Retry;
                        failure.message.push_str(" This recovery pane only attaches existing conversations. Restore its account in Settings if needed, then choose Start or Resume explicitly; Retry only attempts attachment.");
                    }
                    return Err(failure);
                }
                _ => return Err("Daemon did not attach the exact native chat".into()),
            }
        };
        if let Some((revision, model)) = early_model {
            initial.active_model_revision = revision;
            initial.active_model = model;
        }
        let upgrade_recovery = upgrade_recovery
            .map(|(source, resolved)| {
                UpgradeRecovery::confirmed(source, resolved, &session, &initial)
            })
            .transpose()?;
        stream.set_read_timeout(None).map_err(|e| e.to_string())?;
        let mut reader_stream = stream.try_clone().map_err(|e| e.to_string())?;
        let cancel = stream.try_clone().map_err(|error| error.to_string())?;
        let body_requested = Arc::new(AtomicBool::new(true));
        let wire = Arc::new(Mutex::new(Wire {
            stream,
            codec: FrameCodec::new(ipc::MAX_CLIENT_FRAME_BYTES),
            body_requested: body_requested.clone(),
            announced: false,
            hidings: 0,
        }));
        let mut activity = ActivityObservation {
            generation: initial.generation,
            state: initial.activity(),
            pending: false,
        };
        if let Some((generation, state)) = early_activity {
            activity.observe(generation, state);
        }
        let activity = Arc::new(Mutex::new(activity));
        let observed = Arc::new(Mutex::new(early_observed));
        let account_usage = Arc::new(Mutex::new(early_usage));
        let limit = Arc::new(Mutex::new(early_limit));
        let agent_addresses = Arc::new(Mutex::new(early_addresses));
        // Attachment also serves non-UI callers that inspect the initial
        // publication directly. The UI immediately sets its actual visibility;
        // an unvisited hidden pane releases this body without preparing it.
        tracing::trace!(target: "cagents::memory", process_id = std::process::id(), owner = "chat_client", chat_id = %session.id, resident_bytes = initial.transcript_bytes());
        let user_item_seen = Arc::new(AtomicBool::new(
            initial
                .items
                .iter()
                .any(|item| item.role == super::Role::User),
        ));
        let prompt_accepted = Arc::new(AtomicBool::new(false));
        let publication = Arc::new(Publications::new(initial));
        let next = Arc::new(AtomicU64::new(attach_request + 1));
        let alive = Arc::new(AtomicBool::new(true));
        let visible = Arc::new(AtomicBool::new(false));
        let pending: Pending = Arc::new(Mutex::new(BTreeMap::new()));
        let file_pending: FilePending = Arc::new(Mutex::new(BTreeMap::new()));
        let text_copy_pending: TextCopyPending = Arc::new(Mutex::new(BTreeMap::new()));
        let (visibility_tx, visibility_rx) = crossbeam_channel::bounded(1);
        let inner = Arc::new(Inner::new(
            InnerPayload {
                id,
                session,
                upgrade_recovery,
                resumed_from,
                proof: proof.clone(),
                wire: wire.clone(),
                cancel,
                publication: publication.clone(),
                body_requested: body_requested.clone(),
                user_item_seen: user_item_seen.clone(),
                prompt_accepted: prompt_accepted.clone(),
                activity: activity.clone(),
                observed: observed.clone(),
                account_usage: account_usage.clone(),
                limit: limit.clone(),
                agent_addresses: agent_addresses.clone(),
                next: next.clone(),
                alive: alive.clone(),
                wake: wake.clone(),
                visible: visible.clone(),
                reader: Mutex::new(None),
                pending: pending.clone(),
                file_pending: file_pending.clone(),
                text_copy_pending: text_copy_pending.clone(),
                text_copies: AtomicUsize::new(0),
                visibility_tx,
                visibility_worker: Mutex::new(None),
                #[cfg(any(test, feature = "test-support"))]
                retirement_complete: None,
            },
            retirement,
        ));
        let visibility_wire = wire.clone();
        let visibility_alive = alive.clone();
        let visibility_value = visible.clone();
        let visibility_proof = proof.clone();
        let visibility_next = next.clone();
        let visibility_worker = thread::Builder::new()
            .name(format!("chat-visibility-{id}"))
            .spawn(move || {
                trace_worker_stack("chat_visibility");
                while visibility_rx.recv().is_ok() {
                    if !visibility_alive.load(Ordering::Acquire) {
                        break;
                    }
                    let request = ClientRequest::ChatVisibility {
                        chat_id: id,
                        visible: visibility_value.load(Ordering::Acquire),
                    };
                    if transmit(
                        &visibility_wire,
                        &visibility_proof,
                        visibility_next.fetch_add(1, Ordering::Relaxed),
                        request,
                    )
                    .is_err()
                    {
                        let _ = visibility_wire
                            .lock()
                            .unwrap_or_else(|p| p.into_inner())
                            .stream
                            .shutdown(Shutdown::Both);
                        break;
                    }
                }
            })
            .map_err(|error| error.to_string())?;
        *inner
            .visibility_worker
            .lock()
            .unwrap_or_else(|p| p.into_inner()) = Some(visibility_worker);
        let visibility_stop = inner.visibility_tx.clone();
        // Ready consumes any pre-attachment unread edge; do not publish a wake
        // before the UI has installed the matching runtime generation.
        let worker = thread::Builder::new().name(format!("chat-client-{id}")).spawn(move || {
            trace_worker_stack("chat_reader");
            publication.bind_source_thread();
            let mut read_pending = false;
            let mut sent_read = SentRead { id: 0, hidings: 0 };
            let mut read_again = false;
            let mut resync = false;
            let failure = loop {
                let envelope: ServerEnvelope = match codec.read(&mut reader_stream) {
                    Ok(Some(value)) => value,
                    Ok(None) => break "Chat daemon disconnected; exact session retained for reattachment".into(),
                    Err(error) => break format!("Chat daemon frame failed: {error}"),
                };
                if !body_requested.load(Ordering::Acquire) { codec.release_spare(); }
                if envelope.version != PROTOCOL_VERSION || envelope.proof != proof { break "Chat daemon identity/version changed; refusing replacement".into(); }
                if matches!(&envelope.message, ServerMessage::Response { response: ServerResponse::ChatClosed { .. } | ServerResponse::ChatTerminal { .. } }) {
                    let (chat_id, final_snapshot, terminal) = match envelope.message {
                        ServerMessage::Response { response: ServerResponse::ChatClosed { chat_id, snapshot } } => (chat_id, snapshot, None),
                        ServerMessage::Response { response: ServerResponse::ChatTerminal { chat_id, session, snapshot } } => (chat_id, snapshot, Some(session)),
                        _ => unreachable!(),
                    };
                    if chat_id!=id { break "Daemon closed a different chat".into(); }
                    if terminal.is_some() && !final_snapshot.exited {
                        // A shared-server CLI is another view, not a closed
                        // native actor. Keep this connection and pending work.
                        user_item_seen.fetch_or(final_snapshot.items.iter().any(|item| item.role == super::Role::User), Ordering::AcqRel);
                        let resident = publication.replace(final_snapshot, &body_requested, false, false);
                        if resident != Some(true) && body_requested.load(Ordering::Acquire) {
                            resync = true;
                            read_again = true;
                            if visible.load(Ordering::Acquire) && !read_pending {
                                match transmit_read(&wire, &proof, next.fetch_add(1, Ordering::Relaxed), ClientRequest::ChatRead { chat_id: id, after_generation: None }) {
                                    Ok(sent) => { read_pending = sent.is_some(); read_again = sent.is_none(); sent_read = sent.unwrap_or(sent_read); }
                                    Err(error) => break error,
                                }
                            }
                        }
                        if let Some(reply) = envelope.request_id.and_then(|id| pending.lock().unwrap_or_else(|p|p.into_inner()).remove(&id)) {
                            let _ = reply.reply.try_send(Ok(terminal));
                        }
                        wake(Wake::Changed);
                        continue;
                    }
                    alive.store(false, Ordering::Release);
                    user_item_seen.fetch_or(final_snapshot.items.iter().any(|item| item.role == super::Role::User), Ordering::AcqRel);
                    publication.replace(final_snapshot, &body_requested, false, true);
                    {
                        let mut activity = activity.lock().unwrap_or_else(|p| p.into_inner());
                        activity.state = ActivityState::Unknown;
                        activity.pending = false;
                    }
                    alive.store(false,Ordering::Release);
                    let _=visibility_stop.try_send(());
                    let _=reader_stream.shutdown(Shutdown::Both);
                    let replies=std::mem::take(&mut *pending.lock().unwrap_or_else(|p|p.into_inner()));
                    for (request,reply) in replies {
                        let _=reply.reply.try_send(if Some(request)==envelope.request_id { Ok(terminal.clone()) } else { Err("Chat was explicitly closed".into()) });
                    }
                    for (_,reply) in std::mem::take(&mut *file_pending.lock().unwrap_or_else(|p|p.into_inner())) {let _=reply.try_send(Err("Chat was explicitly closed".into()));}
                    for (_,reply) in std::mem::take(&mut *text_copy_pending.lock().unwrap_or_else(|p|p.into_inner())) {let _=reply.try_send(Err("Chat was explicitly closed".into()));}
                    wake(Wake::Changed);
                    return;
                }
                if let Some(reply)=envelope.request_id.and_then(|request|text_copy_pending.lock().unwrap_or_else(|p|p.into_inner()).remove(&request)) {
                    let result = text_copy_response(id, envelope.message);
                    let _=reply.try_send(result);
                    continue;
                }
                if let Some(reply)=envelope.request_id.and_then(|request|file_pending.lock().unwrap_or_else(|p|p.into_inner()).remove(&request)) {
                    let result=match envelope.message {
                        ServerMessage::Response{response:ServerResponse::ChatFileChanges{chat_id,summary}} if chat_id==id=>Ok(super::FileChangeReply::Files(summary)),
                        ServerMessage::Response{response:ServerResponse::ChatFileChangePatches{chat_id,page}} if chat_id==id=>Ok(super::FileChangeReply::Patches(page)),
                        ServerMessage::Error(error)=>Err(error.message),
                        _=>Err("Unexpected file changes reply".into()),
                    };
                    let _=reply.try_send(result);
                    continue;
                }
                if let Some(reply) = envelope.request_id.and_then(|id| pending.lock().unwrap_or_else(|p| p.into_inner()).remove(&id)) {
                    // Latch the authenticated prompt receipt before either EOF
                    // or delayed UI delivery can restore its sendable draft.
                    // Other Accepted replies do not prove prompt acceptance.
                    if reply.prompt && matches!(&envelope.message, ServerMessage::Response { response: ServerResponse::Accepted }) {
                        prompt_accepted.store(true, Ordering::Release);
                    }
                    let result = match &envelope.message { ServerMessage::Response { response: ServerResponse::Accepted | ServerResponse::Detached } => Ok(None), ServerMessage::Error(error) => Err(error.message.clone()), _ => Err("Unexpected chat command response".into()) };
                    let _ = reply.reply.try_send(result);
                    // Request-specific command failures are delivered to the
                    // caller; they do not make presentation unavailable.
                    continue;
                }
                match envelope.message {
                    ServerMessage::ChatChanged { chat_id } if chat_id == id && visible.load(Ordering::Acquire) => {
                        if read_pending { read_again = true; continue; }
                        read_again = true;
                    },
                    ServerMessage::ChatChanged { chat_id } if chat_id == id => {},
                    ServerMessage::ChatObserved { chat_id, epoch } if chat_id == id => {
                        if !visible.load(Ordering::Acquire)
                            && observed.lock().unwrap_or_else(|p| p.into_inner()).observe(epoch)
                        {
                            wake(Wake::Observed);
                        }
                    },
                    ServerMessage::ChatActivity { chat_id, state, generation } if chat_id == id => {
                        let changed = activity.lock().unwrap_or_else(|p| p.into_inner())
                            .observe(generation, state);
                        if changed { wake(Wake::Activity); }
                    },
                    ServerMessage::ChatActiveModel { chat_id, revision, model } if chat_id == id => {
                        if publication.active_model(revision, model, &body_requested) { wake(Wake::Changed); }
                    },
                    ServerMessage::ChatAccountUsage { chat_id, report, observed_at } if chat_id == id => {
                        let first = account_usage.lock().unwrap_or_else(|p| p.into_inner())
                            .push(report, observed_at);
                        if first { wake(Wake::AccountUsage); }
                    },
                    ServerMessage::ChatLimit { chat_id, limit: judged } if chat_id == id => {
                        let changed = {
                            let mut known = limit.lock().unwrap_or_else(|p| p.into_inner());
                            let changed = *known != judged;
                            *known = judged;
                            changed
                        };
                        if changed { wake(Wake::Limit); }
                    },
                    ServerMessage::ChatAgentAddresses { chat_id, addresses } if chat_id == id => {
                        let learned = learn_addresses(
                            &mut agent_addresses.lock().unwrap_or_else(|p| p.into_inner()),
                            addresses,
                        );
                        if learned { wake(Wake::AgentAddresses); }
                    },
                    ServerMessage::Response { response: ServerResponse::ChatSnapshot { chat_id, snapshot: update } } if chat_id == id => {
                        read_pending = false;
                        if let Some(update) = update {
                            resync = false;
                            user_item_seen.fetch_or(update.items.iter().any(|item| item.role == super::Role::User), Ordering::AcqRel);
                            let resident = publication.replace(update, &body_requested, true, false);
                            let state = publication.current.load();
                            tracing::trace!(target: "cagents::memory", process_id = std::process::id(), owner = "chat_client", chat_id = %id,
                                resident_bytes = state.snapshot.transcript_bytes());
                            if resident != Some(true) && body_requested.load(Ordering::Acquire) {
                                resync = true; read_again = true;
                            }
                            if visible.load(Ordering::Acquire) { wake(Wake::Changed); }
                        } else if body_requested.load(Ordering::Acquire) && !publication.current.load().resident {
                            // A conditional read was already in flight at hide.
                            // Its unchanged response cannot hydrate an evicted base.
                            resync = true;
                            read_again = true;
                        }
                    },
                    ServerMessage::Response { response: ServerResponse::ChatUpdate { chat_id, update } } if chat_id == id => {
                        read_pending = false;
                        let (base, revision) = publication.capture();
                        let observed_user = update.changes.iter().any(|change| {
                            let (super::presentation::ItemChange::Replace(item) | super::presentation::ItemChange::Append { item, .. }) = change;
                            item.role == super::Role::User
                        });
                        let retain_body = body_requested.load(Ordering::Acquire) && base.resident;
                        // Validate the full transaction before allowing a metadata
                        // fallback. A failed full patch never publishes just its clock.
                        let derived = if retain_body {
                            let candidates = append::Lineage::candidates(&update, &base.snapshot);
                            update.apply(&base.snapshot).map(|next| {
                                let mut lineage = base.lineage.clone(); lineage.record(candidates, &next);
                                let full = Publication::next(&base, next, true, lineage);
                                let metadata = full.hidden.as_ref().unwrap().clone();
                                (Some(full), metadata)
                            })
                        } else {
                            update.metadata_for(&base.snapshot).map(|mut metadata| {
                                metadata.evict_transcript();
                                (None, Publication::next(&base, metadata, false, append::Lineage::default()))
                            })
                        };
                        let committed = derived.map(|(full, metadata)| {
                            #[cfg(test)]
                            if full.is_some() { tests::pause_before_publication(id); }
                            publication.publish(&base, revision, &body_requested, full, metadata, false)
                        });
                        let accepted = matches!(committed, Ok(Some(_)));
                        user_item_seen.fetch_or(accepted && observed_user, Ordering::AcqRel);
                        let resident = publication.current.load().resident;
                        if !accepted || (body_requested.load(Ordering::Acquire) && !resident) {
                            resync = true; read_again = true;
                        } else if visible.load(Ordering::Acquire) { wake(Wake::Changed); }
                        let state = publication.current.load();
                        tracing::trace!(target: "cagents::memory", process_id = std::process::id(), owner = "chat_client", chat_id = %id,
                            resident_bytes = state.snapshot.transcript_bytes());
                    },
                    ServerMessage::Error(error)
                        if read_pending && sent_read.overtaken(envelope.request_id, &error, &wire) =>
                    {
                        // The daemon refused a read that a hide overtook, so
                        // a kept body is not marked with an error it no
                        // longer has. If the chat is visible again by now,
                        // read again; otherwise showing it will.
                        read_pending = false;
                        read_again = true;
                    },
                    ServerMessage::Error(error) => {
                        read_pending = false;
                        loop {
                            let (base, revision) = publication.capture();
                            let mut snapshot = (*base.snapshot).clone();
                            if error.code == ipc::ProtocolErrorCode::TerminalUnavailable { snapshot.ready = false; }
                            snapshot.error = Some(error.message.clone());
                            let mut proposed = Some(if base.resident {
                                Publication::resident(snapshot, base.clock, base.lineage.clone(), base.source_thread.clone())
                            } else {
                                Publication::owned(snapshot, base.clock, false, append::Lineage::default(), None, base.source_thread.clone())
                            });
                            let (old, current) = {
                                let current_revision = publication.commit.lock().unwrap_or_else(|p| p.into_inner());
                                let current = publication.current.load_full();
                                let old = if current_revision.revision == revision && Arc::ptr_eq(&base, &current) { Some(publication.current.swap(proposed.take().unwrap())) } else { None };
                                (old, current)
                            };
                            let applied = old.is_some(); drop(old); drop(current);
                            if applied { break; }
                        }
                        wake(Wake::Changed);
                    },
                    ServerMessage::HostShuttingDown => break "Chat daemon is shutting down".into(),
                    ServerMessage::Response { .. } => {},
                    _ => break "Unexpected native chat protocol frame".into(),
                }
                if read_again && !read_pending && visible.load(Ordering::Acquire) {
                    let after_generation = {
                        let state = publication.current.load();
                        (!resync && (state.resident || !body_requested.load(Ordering::Acquire))).then_some(state.snapshot.generation)
                    };
                    match transmit_read(&wire, &proof, next.fetch_add(1, Ordering::Relaxed), ClientRequest::ChatRead { chat_id: id, after_generation }) {
                        Ok(sent) => { read_pending = sent.is_some(); read_again = sent.is_none(); sent_read = sent.unwrap_or(sent_read); }
                        Err(error) => break error,
                    }
                }
            };
            alive.store(false, Ordering::Release);
            {
                let mut activity = activity.lock().unwrap_or_else(|p| p.into_inner());
                activity.state = ActivityState::Unknown;
                activity.pending = false;
            }
            let _=visibility_stop.try_send(());
            for (_, reply) in std::mem::take(&mut *pending.lock().unwrap_or_else(|p| p.into_inner())) {
                let _ = reply.reply.try_send(Err(failure.clone()));
            }
            for (_,reply) in std::mem::take(&mut *file_pending.lock().unwrap_or_else(|p|p.into_inner())) {
                let _=reply.try_send(Err(failure.clone()));
            }
            for (_,reply) in std::mem::take(&mut *text_copy_pending.lock().unwrap_or_else(|p|p.into_inner())) {
                let _=reply.try_send(Err(failure.clone()));
            }
            wake(Wake::Disconnected(failure));
        }).map_err(|e| e.to_string())?;
        *inner.reader.lock().unwrap_or_else(|p| p.into_inner()) = Some(worker);
        Ok(Self { inner })
    }
    /// The durable descriptor for this chat, with the conversation the
    /// provider has confirmed.
    ///
    /// A conversation the provider has not named yet is not evidence that the
    /// saved one is gone: a start that is still loading reports nothing, and
    /// so does one that failed to resume. Overwriting the saved identifier
    /// with that absence would leave the card with nothing to resume on its
    /// next attempt, which is how a single failed restore used to turn a
    /// recoverable conversation into a permanently blank one.
    pub fn session(&self) -> Session {
        let mut session = self.inner.session.clone();
        let snapshot = self.snapshot();
        if let Some(id) = snapshot.session_id.clone() {
            session.launch.session_id = Some(id);
        }
        session.learn_native_continuity(&snapshot.native_continuity);
        session
    }
    /// The identity negotiated by ChatAttached, before snapshot projection.
    /// A read-only reader cannot move to another conversation after attachment.
    pub fn attached_session_id(&self) -> Option<&str> {
        self.inner.session.launch.session_id.as_deref()
    }
    pub fn upgrade_recovery(&self) -> Option<&(Session, Session)> {
        self.inner
            .upgrade_recovery
            .as_ref()
            .map(|recovery| &recovery.sessions)
    }
    /// The saved card whose stranded retained owner this successor replaces,
    /// when the daemon proved that owner gone and prepared a resume.
    pub fn resumed_from(&self) -> Option<&Session> {
        self.inner.resumed_from.as_ref()
    }
    pub fn authenticated_upgrade_recovery(&self) -> Option<&UpgradeRecovery> {
        self.inner.upgrade_recovery.as_ref()
    }
    pub fn is_connected(&self) -> bool {
        self.inner.alive.load(Ordering::Acquire)
    }

    pub fn snapshot(&self) -> Arc<Snapshot> {
        self.inner.publication.current.load().snapshot.clone()
    }
    pub(crate) fn snapshot_with_clock(&self) -> (Arc<Snapshot>, Option<SourceClock>) {
        let state = self.inner.publication.current.load();
        (state.snapshot.clone(), state.clock)
    }

    /// Control metadata for a hidden runtime, without borrowing its body.
    /// A disconnected client's final publication must remain readable instead.
    pub fn hidden_snapshot_with_clock(&self) -> Option<(Arc<Snapshot>, Option<SourceClock>)> {
        let (mut snapshot, clock) = self.snapshot_with_clock();
        if !self.inner.alive.load(Ordering::Acquire) {
            return None;
        }
        Arc::make_mut(&mut snapshot).evict_transcript();
        Some((snapshot, clock))
    }

    pub fn has_observed_user_item(&self) -> bool {
        self.inner.user_item_seen.load(Ordering::Acquire)
    }

    /// A prompt receipt can precede its first transcript publication. Either
    /// authenticated observation forbids offering that prompt as an unsent draft.
    pub fn has_accepted_user_input(&self) -> bool {
        self.has_observed_user_item() || self.inner.prompt_accepted.load(Ordering::Acquire)
    }

    /// A renderable publication, absent until a hidden client's full history
    /// has been rehydrated. Metadata-only generations must never clear a
    /// runtime's scroll/disclosure state as if they were empty conversations.
    pub fn resident_snapshot_with_clock(&self) -> Option<(Arc<Snapshot>, Option<SourceClock>)> {
        let state = self.inner.publication.current.load();
        state
            .resident
            .then(|| (state.snapshot.clone(), state.clock))
    }

    #[cfg(any(test, feature = "test-support"))]
    fn install_snapshot_for_test(&self, update: Snapshot) {
        self.inner.user_item_seen.fetch_or(
            update
                .items
                .iter()
                .any(|item| item.role == super::Role::User),
            Ordering::AcqRel,
        );
        let (base, revision) = self.inner.publication.capture();
        let full = Publication::next(&base, update, true, append::Lineage::default());
        let metadata = full.hidden.as_ref().unwrap().clone();
        self.inner.publication.publish(
            &base,
            revision,
            &self.inner.body_requested,
            Some(full),
            metadata,
            false,
        );
    }

    /// Drains one coalesced activity edge without touching presentation state.
    pub fn take_activity(&self) -> ActivityState {
        let mut activity = self
            .inner
            .activity
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        activity.take()
    }
    /// How the provider addresses this conversation's subagents, by address,
    /// with the ID its snapshot lists each under.
    pub fn agent_addresses(&self) -> AgentAddresses {
        self.inner
            .agent_addresses
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }
    /// The usage limit that stops this conversation, as the daemon last
    /// judged it.
    pub fn limit(&self) -> Option<crate::agent_execution::directory::UsageLimit> {
        self.inner
            .limit
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }
    /// Drains what the provider reported about the account this chat runs
    /// on, oldest first.
    pub fn take_account_usage(&self) -> Vec<(LiveReport, u64)> {
        self.inner
            .account_usage
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .reports
            .drain(..)
            .collect()
    }
    pub fn take_observed(&self) -> bool {
        self.inner
            .observed
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take()
    }
    pub fn proof(&self) -> Option<HostSessionProof> {
        Some(self.inner.proof.clone())
    }
    fn request_response(
        &self,
        request: ClientRequest,
    ) -> Result<Option<crate::model::TerminalSession>, String> {
        if !self.inner.alive.load(Ordering::Acquire) {
            return Err("Chat is disconnected; exact reattachment is required".into());
        }
        let id = self.inner.next.fetch_add(1, Ordering::Relaxed);
        let (reply, response) = crossbeam_channel::bounded(1);
        {
            let mut pending = self.inner.pending.lock().unwrap_or_else(|p| p.into_inner());
            if pending.len() >= 64 {
                return Err("Chat client command capacity reached".into());
            }
            pending.insert(
                id,
                PendingCommand {
                    prompt: matches!(
                        &request,
                        ClientRequest::ChatCommand {
                            command: Command::Prompt { .. },
                            ..
                        }
                    ),
                    reply,
                },
            );
        }
        let result = transmit(&self.inner.wire, &self.inner.proof, id, request).and_then(|()| {
            response
                .recv_timeout(Duration::from_secs(40))
                .map_err(|_| {
                    "Chat request acknowledgement timed out; do not retry blindly".to_string()
                })?
        });
        self.inner
            .pending
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&id);
        result
    }
    fn request(&self, request: ClientRequest) -> Result<(), String> {
        self.request_response(request).map(|_| ())
    }

    /// The daemon retains and replays the resulting terminal across lost replies.
    /// Credentials stay entirely within the host's native/PTY execution context.
    pub fn return_terminal(&self) -> Result<crate::model::TerminalSession, String> {
        let terminal = self
            .request_response(ClientRequest::ChatReturnTerminal {
                chat_id: self.inner.id,
            })?
            .ok_or("Daemon did not return the exact replacement terminal")?;
        if terminal.continuity.live_proof().as_ref() != Some(&self.inner.proof) {
            return Err("Replacement terminal has no matching daemon ownership proof".into());
        }
        Ok(terminal)
    }
    /// Read immutable cached edit data on this connection's own reply channel.
    /// Call off the UI thread; no watch publication or provider state is mutated.
    pub fn file_changes(&self, command: Command) -> Result<super::FileChangeReply, String> {
        if !matches!(
            &command,
            Command::LoadFileChanges { .. } | Command::LoadFileChangePatches { .. }
        ) {
            return Err("Expected a file changes cache query".into());
        }
        if !self.inner.alive.load(Ordering::Acquire) {
            return Err("Chat is disconnected; retained file pages remain readable".into());
        }
        let id = self.inner.next.fetch_add(1, Ordering::Relaxed);
        let (reply, response) = crossbeam_channel::bounded(1);
        {
            let mut pending = self
                .inner
                .file_pending
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            if pending.len() >= 64 {
                return Err("File changes request capacity reached".into());
            }
            pending.insert(id, reply);
        }
        let result = transmit(
            &self.inner.wire,
            &self.inner.proof,
            id,
            ClientRequest::ChatCommand {
                chat_id: self.inner.id,
                command,
            },
        )
        .and_then(|()| {
            response
                .recv_timeout(Duration::from_secs(40))
                .map_err(|_| "File changes request timed out".to_string())?
        });
        self.inner
            .file_pending
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&id);
        result
    }
    /// Assemble one immutable logical body over this connection's own replies.
    /// Call off the UI thread: all pages share one total deadline and never
    /// publish snapshots or dispatch commands to the provider.
    pub fn copy_text(&self, source: super::TextCopySource) -> Result<String, String> {
        let deadline = Instant::now() + TEXT_COPY_TIMEOUT;
        if source.bytes > super::TEXT_COPY_LIMIT_BYTES as u64 {
            return Err("Text copy exceeds the supported byte limit".into());
        }
        self.inner
            .text_copies
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                (count < 2).then_some(count + 1)
            })
            .map_err(|_| "Text copy request capacity reached".to_string())?;
        struct Reservation<'a>(&'a AtomicUsize);
        impl Drop for Reservation<'_> {
            fn drop(&mut self) {
                self.0.fetch_sub(1, Ordering::AcqRel);
            }
        }
        let _reservation = Reservation(&self.inner.text_copies);
        let mut text = String::new();
        text.try_reserve_exact(source.bytes as usize)
            .map_err(|_| "Could not allocate text copy buffer".to_string())?;
        loop {
            if !self.inner.alive.load(Ordering::Acquire) {
                return Err("Chat is disconnected; text copy is unavailable".into());
            }
            text_copy_remaining(deadline)?;
            let offset = text.len() as u64;
            let id = self.inner.next.fetch_add(1, Ordering::Relaxed);
            let (reply, response) = crossbeam_channel::bounded(1);
            {
                let mut pending = self
                    .inner
                    .text_copy_pending
                    .lock()
                    .unwrap_or_else(|p| p.into_inner());
                // Disconnect sets alive before draining this same map. Recheck
                // under its lock so a request cannot be stranded after the drain.
                if !self.inner.alive.load(Ordering::Acquire) {
                    return Err("Chat is disconnected; text copy is unavailable".into());
                }
                pending.insert(id, reply);
            }
            let result = transmit_text_copy(
                &self.inner.wire,
                &self.inner.proof,
                id,
                ClientRequest::ChatCommand {
                    chat_id: self.inner.id,
                    command: Command::ReadTextCopy {
                        source: source.clone(),
                        offset,
                    },
                },
                deadline,
            )
            .and_then(|()| {
                response
                    .recv_timeout(text_copy_remaining(deadline)?)
                    .map_err(|_| "Text copy request timed out or disconnected".to_string())?
            });
            self.inner
                .text_copy_pending
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .remove(&id);
            let page = result?;
            text_copy_remaining(deadline)?;
            append_text_copy_page(&source, &mut text, page)?;
            if text.len() as u64 == source.bytes {
                return Ok(text);
            }
        }
    }
    pub fn send(&self, command: Command) -> Result<(), String> {
        self.request(ClientRequest::ChatCommand {
            chat_id: self.inner.id,
            command,
        })
    }
    pub fn known_append(&self, previous: &Arc<super::Item>, current: &Arc<super::Item>) -> bool {
        self.inner
            .publication
            .current
            .load()
            .lineage
            .proves(previous, current)
    }

    pub fn set_visible(&self, visible: bool) -> Result<(), String> {
        self.set_interest(visible, visible)
    }

    /// Keep control notifications flowing for queued commands without keeping
    /// renderable history resident in an offscreen UI card.
    pub fn set_interest(&self, interested: bool, retain_body: bool) -> Result<(), String> {
        self.update_interest(
            interested,
            retain_body,
            retain_body.then_some(BodyOwner::Client),
        )
    }

    /// The caller now owns an already projected, readable source window. Release
    /// this client's raw body even if its reader died before the UI saw its wake.
    /// A late final frame still supplies identity/control state, but cannot take
    /// body ownership back. A later live body request restores normal ownership.
    pub fn hand_off_source_body(&self) -> Result<(), String> {
        self.update_interest(false, false, Some(BodyOwner::FrozenProjection))
    }

    fn update_interest(
        &self,
        interested: bool,
        retain_body: bool,
        body_owner: Option<BodyOwner>,
    ) -> Result<(), String> {
        let handoff = body_owner == Some(BodyOwner::FrozenProjection);
        if !handoff && !self.inner.alive.load(Ordering::Acquire) {
            return Err("Chat is disconnected".into());
        }
        let changed = self.inner.visible.swap(interested, Ordering::AcqRel) != interested;
        let (old, current, result) = {
            let mut revision = self
                .inner
                .publication
                .commit
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            let current = self.inner.publication.current.load_full();
            if !handoff && !self.inner.alive.load(Ordering::Acquire) {
                (None, current, Err("Chat is disconnected"))
            } else {
                let owner_changed = body_owner.is_some_and(|owner| {
                    let changed = revision.body_owner != owner;
                    revision.body_owner = owner;
                    changed
                });
                let body_changed = self
                    .inner
                    .body_requested
                    .swap(retain_body, Ordering::AcqRel)
                    != retain_body
                    || owner_changed;
                if body_changed {
                    revision.revision = revision
                        .revision
                        .checked_add(1)
                        .expect("chat body-policy revision exhausted");
                }
                let (old, published_resident, published_generation) = if !retain_body {
                    match current.hidden.as_ref() {
                        Some(metadata) => (
                            Some(self.inner.publication.current.swap(metadata.clone())),
                            metadata.resident,
                            metadata.snapshot.generation,
                        ),
                        None => (None, current.resident, current.snapshot.generation),
                    }
                } else {
                    (None, current.resident, current.snapshot.generation)
                };
                let observation = (
                    revision.revision,
                    published_resident,
                    published_generation,
                    old.is_some(),
                );
                (old, current, Ok((body_changed, observation)))
            }
        };
        // No retry, allocation, Snapshot/lineage cloning or destruction under
        // the gate. Show keeps the body-free node until fresh hydration arrives.
        drop(old);
        drop(current);
        let (
            body_changed,
            (revision, published_resident, published_generation, metadata_installed),
        ) = result.map_err(str::to_owned)?;
        if changed || body_changed {
            // This is the publication installed by this policy transaction,
            // not a receipt for other aliases or retirement-queue completion.
            tracing::trace!(target: "cagents::memory", process_id = std::process::id(),
                owner = "chat_client_policy", chat_id = %self.inner.id,
                requested_interested = interested, requested_retain_body = retain_body,
                policy_revision = revision, published_generation, published_resident,
                metadata_installed);
        }
        if !self.inner.alive.load(Ordering::Acquire) {
            return Err("Chat is disconnected".into());
        }
        if interested && (changed || body_changed) {
            // Controls can use retained metadata immediately. The visibility
            // notification triggers an unconditional read for evicted history,
            // even if the idle daemon's generation has not changed.
            (self.inner.wake)(Wake::Changed);
        }
        // Visibility is replaceable state, not an ordered provider frame. Coalesce
        // transitions without socket IO or acknowledgement waits on the UI thread.
        match self.inner.visibility_tx.try_send(()) {
            Ok(()) | Err(crossbeam_channel::TrySendError::Full(())) => Ok(()),
            Err(crossbeam_channel::TrySendError::Disconnected(())) => {
                Err("Chat visibility worker disconnected".into())
            }
        }
    }
    /// Fence pending wakes and wire frames synchronously, before scheduling IO.
    /// Neither acknowledgement phase changes visibility or reads presentation.
    pub fn begin_acknowledgement(&self) -> u64 {
        self.inner
            .observed
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .begin_acknowledgement()
    }
    pub fn acknowledge_updates(&self, epoch: u64) -> Result<(), String> {
        self.request(ClientRequest::ChatAcknowledge {
            chat_id: self.inner.id,
            epoch,
        })
    }
    /// Explicitly interrupts active work, waits for shutdown, then closes the owned provider pipe.
    /// Dropping the handle instead only detaches and leaves provider work running.
    /// On success, `snapshot()` contains the final provider identity/state before
    /// the daemon frees this explicitly closed chat's transcript cache.
    pub fn close(&self) -> Result<(), String> {
        self.request(ClientRequest::ChatClose {
            chat_id: self.inner.id,
        })
    }
}

/// Retire replay evidence only after every durable reference owned by this GUI
/// has been replaced and saved. This never starts a daemon or a provider process.
/// Retrying a lost acknowledgement is harmless.
/// Results correspond to the input order. A rejected terminal does not block
/// other receipts, and a missing daemon has no in-memory receipts to retire.
pub fn acknowledge_terminal_handoffs(
    terminals: &[crate::model::TerminalSession],
) -> Vec<Result<(), String>> {
    if terminals.is_empty() {
        return Vec::new();
    }
    let socket = match ipc::daemon_socket_path() {
        Ok(socket) => socket,
        Err(error) => return vec![Err(error.to_string()); terminals.len()],
    };
    acknowledge_terminal_handoffs_on(crate::control::ControlClient::connect(&socket), terminals)
}

fn acknowledge_terminal_handoffs_on(
    connection: Result<crate::control::ControlClient, crate::control::ControlError>,
    terminals: &[crate::model::TerminalSession],
) -> Vec<Result<(), String>> {
    use crate::control::ControlError;

    let mut client = match connection {
        Ok(client) => client,
        Err(ControlError::Connect(error))
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
            ) =>
        {
            return vec![Ok(()); terminals.len()];
        }
        Err(error) => return vec![Err(error.to_string()); terminals.len()],
    };
    let mut connection_failure: Option<String> = None;
    terminals
        .iter()
        .map(|terminal| {
            // Exited descriptors have no live proof; their saved UUID retires only
            // a matching receipt and never authorizes execution.
            if let Some(expected) = terminal.continuity.live_proof() {
                if !expected.is_complete() {
                    return Err(
                        "Terminal handoff acknowledgement has an incomplete daemon proof".into(),
                    );
                }
                // A different, proven daemon never held this in-memory receipt.
                if &expected != client.proof() {
                    return Ok(());
                }
            }
            if let Some(error) = &connection_failure {
                return Err(error.clone());
            }
            match client.commit_terminal_handoff(terminal.id) {
                Ok(()) => Ok(()),
                Err(error) => {
                    // A correlated refusal leaves the stream in step. Transport
                    // and protocol failures may not; never reuse that connection.
                    if !matches!(error, ControlError::Remote { .. }) {
                        connection_failure = Some(error.to_string());
                    }
                    Err(error.to_string())
                }
            }
        })
        .collect()
}

/// Running and starting daemon chats, including detached sessions. Never starts a daemon.
pub fn inventory() -> Result<Vec<Session>, String> {
    let socket = ipc::daemon_socket_path().map_err(|e| e.to_string())?;
    let mut client = match crate::control::ControlClient::connect(&socket) {
        Ok(client) => client,
        Err(crate::control::ControlError::Connect(error))
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
            ) =>
        {
            return Ok(Vec::new());
        }
        Err(error) => return Err(error.to_string()),
    };
    client.list_chats().map_err(|e| e.to_string())
}
impl Drop for Inner {
    fn drop(&mut self) {
        let Some(retired) = self.owned.take() else {
            return;
        };
        // Socket EOF unregisters the subscription, never stops the daemon's actor.
        retired.payload.alive.store(false, Ordering::Release);
        let _ = retired.payload.visibility_tx.try_send(());
        let _ = retired.payload.cancel.shutdown(Shutdown::Both);
        self.retirement.enqueue(RetirementJob::Client(retired));
    }
}

/// Daemon handshakes for chat-client tests here and in the UI crate.
#[cfg(any(test, feature = "test-support"))]
#[doc(hidden)]
pub mod fixture {
    use super::*;

    pub fn host_proof(incarnation: &str) -> HostSessionProof {
        use crate::model::{BootId, HostIncarnation, HostSessionId};
        HostSessionProof {
            host_session_id: HostSessionId::new("session"),
            boot_id: BootId::new("boot"),
            host_incarnation: HostIncarnation::new(incarnation),
        }
    }

    pub fn restore_with_lineage(
        session: Session,
        resolved: Option<Session>,
        answer: impl FnOnce(ClientRequest) -> ServerMessage + Send + 'static,
    ) -> Result<ClientHandle, StartFailure> {
        use crate::terminal_ipc::{ProtocolError, ProtocolErrorCode};
        let live = resolved
            .as_ref()
            .and_then(|session| session.proof.clone())
            .unwrap_or_else(|| host_proof("live"));
        let (client, mut server) = UnixStream::pair().unwrap();
        let proof = live.clone();
        let worker = thread::spawn(move || {
            server
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            server
                .set_write_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut codec = FrameCodec::new(ipc::MAX_FRAME_BYTES);
            let respond = |codec: &mut FrameCodec, server: &mut UnixStream, id, message| {
                codec
                    .write(
                        server,
                        &ServerEnvelope {
                            version: PROTOCOL_VERSION,
                            request_id: Some(id),
                            proof: proof.clone(),
                            message,
                        },
                    )
                    .unwrap();
            };
            let connect: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
            respond(
                &mut codec,
                &mut server,
                connect.request_id,
                ServerMessage::Response {
                    response: ServerResponse::Connected {
                        proof: proof.clone(),
                        resumed: Vec::new(),
                    },
                },
            );
            let lineage: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
            respond(
                &mut codec,
                &mut server,
                lineage.request_id,
                resolved.map_or_else(
                    || {
                        ServerMessage::Error(ProtocolError::new(
                            ProtocolErrorCode::TerminalUnavailable,
                            "No durable upgrade replacement exists for this native chat",
                            false,
                        ))
                    },
                    |session| ServerMessage::Response {
                        response: ServerResponse::UpgradeChatResolved { session },
                    },
                ),
            );
            let start: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
            respond(
                &mut codec,
                &mut server,
                start.request_id,
                answer(start.request),
            );
        });
        let result = ClientHandle::connect_on(
            client,
            session,
            Intent::Restore,
            None,
            false,
            None,
            None,
            None,
            |_| {},
        );
        worker.join().unwrap();
        result
    }
}

#[cfg(test)]
mod tests;
