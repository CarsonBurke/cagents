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

fn start_failure(error: ipc::ProtocolError) -> StartFailure {
    StartFailure {
        message: error.message,
        recovery: match error.code {
            ipc::ProtocolErrorCode::AccountRemoved => super::Recovery::Account,
            ipc::ProtocolErrorCode::ChatCapacity => super::Recovery::Capacity,
            _ => super::Recovery::Retry,
        },
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

fn frame_read_error(error: ipc::FrameError, timeout: Option<Duration>) -> String {
    match &error {
        ipc::FrameError::Io(io_error)
            if matches!(
                io_error.kind(),
                io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
            ) =>
        {
            timeout.map_or_else(
                || "Timed out waiting for the daemon response".into(),
                |timeout| {
                    format!(
                        "the daemon did not answer within {} s",
                        timeout.as_secs_f64()
                    )
                },
            )
        }
        _ => format!("Cannot decode matched daemon protocol: {error}"),
    }
}

#[derive(Debug, Clone)]
pub enum Wake {
    Changed,
    Observed,
    /// Consume the newest state with `take_activity`; never fetches a transcript.
    Activity,
    /// Consume the reports with `take_account_usage`.
    AccountUsage,
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
}
type Pending = Arc<
    Mutex<
        BTreeMap<
            u64,
            crossbeam_channel::Sender<Result<Option<crate::model::TerminalSession>, String>>,
        >,
    >,
>;
type FilePending =
    Arc<Mutex<BTreeMap<u64, crossbeam_channel::Sender<Result<super::FileChangeReply, String>>>>>;
type TextCopyPending =
    Arc<Mutex<BTreeMap<u64, crossbeam_channel::Sender<Result<super::TextCopyPage, String>>>>>;
const TEXT_COPY_TIMEOUT: Duration = Duration::from_secs(40);

/// A daemon timestamp paired with local receipt time. It survives delayed UI
/// hydration and advances while the provider is quiet without relying on the
/// display machine's wall clock.
#[derive(Clone, Copy, Debug)]
pub(crate) struct SourceClock {
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

    pub(crate) fn now(self) -> u64 {
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
pub(crate) struct UpgradeRecovery {
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
    pub(crate) fn is_candidate(source: &Session, replacement: &Session) -> bool {
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

    pub(crate) fn source(&self) -> &Session {
        &self.sessions.0
    }
    pub(crate) fn replacement(&self) -> &Session {
        &self.sessions.1
    }

    pub(crate) fn validate(&self) -> bool {
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
    #[cfg(test)]
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
        #[cfg(test)]
        let complete = self
            .payload
            .retirement_complete
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take();
        // Snapshot/control vectors, weak lineage and ready metadata ownership
        // all belong to this same node, regardless of the last Arc's thread.
        drop(self);
        #[cfg(test)]
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
                    #[cfg(test)]
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
        snapshot: Snapshot,
        resident: bool,
        lineage: append::Lineage,
    ) -> Arc<Self> {
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

struct Publications {
    current: ArcSwap<Publication>,
    // Only writers use this gate. All body derivation and destruction happens
    // outside it; UI presentation readers never acquire a producer lock.
    commit: Mutex<u64>,
}

impl Publications {
    fn new(snapshot: Snapshot) -> Self {
        Self {
            current: ArcSwap::from(Publication::initial(snapshot)),
            commit: Mutex::new(0),
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
        (self.current.load_full(), *revision)
    }

    fn replace(
        &self,
        snapshot: Snapshot,
        body: &AtomicBool,
        sample_clock: bool,
        final_body: bool,
    ) -> Option<bool> {
        let (base, revision) = self.capture();
        let mut clock = base.clock;
        if sample_clock {
            SourceClock::observe(&mut clock, snapshot.sampled_at, Instant::now());
        }
        let full = Publication::resident(
            snapshot,
            clock,
            append::Lineage::default(),
            base.source_thread.clone(),
        );
        let metadata = full
            .hidden
            .as_ref()
            .expect("resident publication has ready metadata")
            .clone();
        #[cfg(test)]
        tests::pause_before_replacement(self as *const Self as usize);
        self.publish(&base, revision, body, Some(full), metadata, final_body)
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
                let resident = full.is_some()
                    && (final_body
                        || (*current_revision == revision
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
    proof: HostSessionProof,
    wire: Arc<Mutex<Wire>>,
    cancel: UnixStream,
    publication: Arc<Publications>,
    body_requested: Arc<AtomicBool>,
    // First-prompt receipt evidence survives body eviction, without retaining
    // user text. This is scoped to this exact authenticated connection.
    user_item_seen: Arc<AtomicBool>,
    activity: Arc<Mutex<ActivityObservation>>,
    observed: Arc<Mutex<OutputObservation>>,
    account_usage: Arc<Mutex<AccountUsage>>,
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
    #[cfg(test)]
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
        #[cfg(test)]
        let completed = self.payload.retirement_complete.take();
        // Actual publication, codec scratch, pending replies, session metadata
        // and callback ownership all leave the event thread together.
        drop(self);
        tracing::trace!(target: "cagents::memory", process_id = std::process::id(), owner = "chat_client", chat_id = %id,
            released = true, resident_bytes = 0usize);
        #[cfg(test)]
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
    let hidden = matches!(
        &request,
        ClientRequest::ChatVisibility { visible: false, .. }
    );
    let mut wire = wire.lock().unwrap_or_else(|p| p.into_inner());
    let Wire {
        stream,
        codec,
        body_requested,
    } = &mut *wire;
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

fn receive(
    stream: &mut UnixStream,
    codec: &mut FrameCodec,
    request_id: u64,
    proof: Option<&HostSessionProof>,
) -> Result<ServerResponse, String> {
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
        .map_err(|error| frame_read_error(error, stream.read_timeout().ok().flatten()))?
        .ok_or("Daemon disconnected")?;
    if envelope.version != PROTOCOL_VERSION {
        #[cfg(feature = "ui")]
        crate::daemon_health::incompatible_stream(stream, envelope.version, health_request);
        return Err("Daemon version mismatch; use matched cagents executables".into());
    }
    if !envelope.proof.is_complete() || proof.is_some_and(|proof| proof != &envelope.proof) {
        return Err("Daemon proof changed".into());
    }
    if envelope.request_id != Some(request_id) {
        return Err("Unexpected daemon response ordering".into());
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
            Err(error.message)
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
    session
        .launch
        .recipe
        .remove_unset_environment(&mut environment);
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
    retry_explicit_start: bool,
) -> Result<ClientRequest, StartFailure> {
    if session.proof.as_ref() == Some(proof) && !retry_explicit_start {
        return Ok(ClientRequest::ChatAttach { session });
    }
    let mut refusal = None;
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
        let sessions = match receive(stream, codec, request_id, Some(proof))? {
            ServerResponse::ChatListed { sessions } => sessions,
            _ => return Err("Daemon did not return its native chat inventory".into()),
        };
        if let Some(hosted) = sessions.into_iter().find(|hosted| hosted.id == session.id) {
            if hosted.proof.as_ref() != Some(proof)
                || hosted.launch.provider != session.launch.provider
                || hosted.launch.recipe != session.launch.recipe
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
                let prepared = match receive(stream, codec, request_id, Some(proof))? {
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
        match receive(stream, codec, request_id, Some(proof)) {
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
                refusal = Some(
                    "The daemon answered this chat's upgrade lineage with a different conversation."
                        .to_owned(),
                );
            }
            // A daemon an installer stopped and restarted never wrote an
            // upgrade checkpoint, which is the ordinary case after an
            // application update rather than a reason to give up.
            Err(message) => refusal = Some(message),
        }
    }
    if session.native_continuity.retains_provider() {
        return Err(StartFailure::from(format!(
            "The saved native conversation could not be reattached. Its uncertain work was not restarted or resubmitted. {}",
            refusal.unwrap_or_else(|| "No authenticated native holder was available.".into())
        )));
    }
    if session.launch.provider == crate::agent::Provider::Claude && !retry_explicit_start {
        return Err(StartFailure::from(format!(
            "Saved Claude history could not be opened without starting a provider. Its exact conversation and draft are retained. {}",
            refusal.unwrap_or_else(|| "No authenticated history attachment was available.".into())
        )));
    }
    if session.launch.session_id.is_none() {
        return Err(StartFailure::fresh(format!(
            "The daemon that was hosting this conversation is no longer running, and this card never recorded a conversation to resume. {}",
            refusal.unwrap_or_else(|| "No saved daemon proof was available either.".to_owned())
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
impl ClientHandle {
    /// A handle whose snapshot the UI can render without a daemon.
    ///
    /// The peer sockets are kept open for the process lifetime so dropping the
    /// handle does not look like a protocol failure to a test that only draws.
    #[cfg(test)]
    pub(crate) fn fixture(session: Session, snapshot: Snapshot) -> Self {
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
                    proof,
                    wire: Arc::new(Mutex::new(Wire {
                        stream,
                        codec: FrameCodec::new(ipc::MAX_FRAME_BYTES),
                        body_requested: body_requested.clone(),
                    })),
                    cancel,
                    publication: Arc::new(Publications::new(snapshot)),
                    body_requested,
                    user_item_seen: Arc::new(AtomicBool::new(user_item_seen)),
                    activity: Arc::new(Mutex::new(ActivityObservation {
                        generation: 0,
                        state: ActivityState::Idle,
                        pending: false,
                    })),
                    observed: Arc::new(Mutex::new(OutputObservation::default())),
                    account_usage: Arc::default(),
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

    #[cfg(test)]
    pub(crate) fn fixture_upgrade(source: Session, session: Session, snapshot: Snapshot) -> Self {
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

    /// Drives the production connect/lineage/attach path for UI writer tests.
    #[cfg(test)]
    pub(crate) fn fixture_upgrade_handshake(
        source: Session,
        session: Session,
        snapshot: Snapshot,
    ) -> Result<Self, StartFailure> {
        tests::restore_with_lineage(source, Some(session), move |request| {
            let ClientRequest::ChatAttach { session } = request else {
                panic!("expected exact upgrade attach");
            };
            ServerMessage::Response {
                response: ServerResponse::ChatAttached { session, snapshot },
            }
        })
    }

    /// Publishes `snapshot` as the reader thread would on a presentation
    /// frame, so a test can stream a transcript into a surface.
    #[cfg(test)]
    pub(crate) fn publish_snapshot_for_test(&self, snapshot: Snapshot) {
        self.install_snapshot_for_test(snapshot);
    }

    /// Apply a fixture publication through the production delta validator and
    /// append-lineage recorder. A full-snapshot fixture intentionally resets
    /// that lineage and cannot stand in for a streaming delta benchmark.
    #[cfg(test)]
    pub(crate) fn publish_presentation_for_test(&self, update: super::presentation::Presentation) {
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
    #[cfg(test)]
    pub(crate) fn publish_delta_snapshot_for_test(&self, mut next: Snapshot) {
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
    #[cfg(test)]
    pub(crate) fn push_account_usage_for_test(&self, report: LiveReport, observed_at: u64) -> bool {
        self.inner
            .account_usage
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(report, observed_at)
    }

    /// Records an activity edge as the reader thread would, and says whether
    /// it would have woken the surface.
    #[cfg(test)]
    pub(crate) fn observe_activity_for_test(&self, state: ActivityState) -> bool {
        let mut activity = self
            .inner
            .activity
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let generation = activity.generation;
        activity.observe(generation, state)
    }

    /// Whether the handle has asked its daemon to keep the snapshot current.
    #[cfg(test)]
    pub(crate) fn is_visible_for_test(&self) -> bool {
        self.inner.visible.load(Ordering::Acquire)
    }

    /// Marks the connection lost, as the reader thread does when its daemon
    /// goes away.
    #[cfg(test)]
    pub(crate) fn disconnect_for_test(&self) {
        self.inner.alive.store(false, Ordering::Release);
    }

    #[cfg(test)]
    pub(crate) fn replace_snapshot_for_test(&self, snapshot: Snapshot) {
        self.install_snapshot_for_test(snapshot);
    }

    /// Observe disposal of this exact publication, independently of the UI's
    /// presentation reclaimer. The signal follows destruction of its payload.
    #[cfg(test)]
    pub(crate) fn observe_snapshot_retirement_for_test(
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
        let socket = crate::terminal::ensure_daemon_ready().map_err(|e| e.to_string())?;
        let stream = connect_daemon(&socket)
            .map_err(|e| format!("Cannot connect to the proven chat daemon: {e}"))?;
        Self::connect_on(
            stream,
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

    /// Attach only the exact canonical replacement authenticated from this
    /// saved source. Unlike ordinary restore, this never starts a provider.
    pub fn connect_upgrade(
        source: Session,
        expected: Session,
        wake: impl Fn(Wake) + Send + Sync + 'static,
    ) -> Result<Self, StartFailure> {
        if UpgradeRecovery::candidate(&source, &expected).is_none() {
            return Err("The canonical chat is not an exact upgrade of the saved source".into());
        }
        let socket = crate::terminal::ensure_daemon_ready().map_err(|error| error.to_string())?;
        let stream = connect_daemon(&socket).map_err(|error| error.to_string())?;
        Self::connect_on(
            stream,
            source,
            Intent::Restore,
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
        let socket = crate::terminal::ensure_daemon_ready().map_err(|error| error.to_string())?;
        let stream = connect_daemon(&socket).map_err(|error| error.to_string())?;
        Self::connect_on(
            stream,
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
            return Err("Account switch changed the saved conversation".into());
        }
        let socket = crate::terminal::ensure_daemon_ready().map_err(|error| error.to_string())?;
        let stream = connect_daemon(&socket).map_err(|error| error.to_string())?;
        Self::connect_on(
            stream,
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

    fn connect_on(
        mut stream: UnixStream,
        session: Session,
        intent: Intent,
        source: Option<HostSessionProof>,
        terminate_source: bool,
        selection: Option<String>,
        account_recovery: Option<(Session, Option<u64>)>,
        upgrade_expected: Option<Session>,
        wake: impl Fn(Wake) + Send + Sync + 'static,
    ) -> Result<Self, StartFailure> {
        let retirement = ClientRetirement::for_client(session.id)?;
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
        if let Some(expected) = &upgrade_expected
            && (intent != Intent::Restore
                || source.is_some()
                || terminate_source
                || selection.is_some()
                || account_recovery.is_some()
                || UpgradeRecovery::candidate(&session, expected).is_none())
        {
            return Err("The canonical chat is not an exact upgrade of the saved source".into());
        }
        let wake: Arc<dyn Fn(Wake) + Send + Sync> = Arc::new(wake);
        stream
            .set_read_timeout(Some(CHAT_ATTACH_TIMEOUT))
            .map_err(|e| e.to_string())?;
        stream
            .set_write_timeout(Some(Duration::from_secs(5)))
            .map_err(|e| e.to_string())?;
        let mut codec = FrameCodec::new(ipc::MAX_FRAME_BYTES);
        let expected = upgrade_expected
            .as_ref()
            .and_then(|session| session.proof.clone())
            .or_else(|| source.clone());
        codec
            .write(
                &mut stream,
                &ClientEnvelope {
                    version: PROTOCOL_VERSION,
                    request_id: 1,
                    proof: None,
                    request: ClientRequest::Connect {
                        expected_host: expected.clone(),
                        resume: Vec::new(),
                    },
                },
            )
            .map_err(|e| e.to_string())?;
        let proof = match receive(&mut stream, &mut codec, 1, expected.as_ref())? {
            ServerResponse::Connected { proof, .. } => proof,
            _ => return Err("Daemon did not complete native chat handshake".into()),
        };
        if !proof.is_complete() {
            return Err("Daemon returned an incomplete native chat ownership proof".into());
        }
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
        let id = session.id;
        let mut attach_request = 2;
        let mut upgrade_recovery = None;
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
            match receive(&mut stream, &mut codec, request_id, Some(&proof))? {
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
        } else if intent == Intent::Start {
            let execution_environment = native_environment(&session, source.is_none());
            ClientRequest::ChatStart {
                session,
                source,
                terminate_source,
                execution_environment,
            }
        } else {
            restore(
                &mut stream,
                &mut codec,
                &mut attach_request,
                &proof,
                session,
                &mut upgrade_recovery,
                intent == Intent::RetryStart,
            )?
        };
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
        let (session, initial) = loop {
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
                ServerMessage::ChatAccountUsage {
                    chat_id,
                    report,
                    observed_at,
                } if chat_id == id && envelope.request_id.is_none() => {
                    early_usage.push(report, observed_at);
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
                    return Err(start_failure(error));
                }
                _ => return Err("Daemon did not attach the exact native chat".into()),
            }
        };
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
                proof: proof.clone(),
                wire: wire.clone(),
                cancel,
                publication: publication.clone(),
                body_requested: body_requested.clone(),
                user_item_seen: user_item_seen.clone(),
                activity: activity.clone(),
                observed: observed.clone(),
                account_usage: account_usage.clone(),
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
                #[cfg(test)]
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
                            if visible.load(Ordering::Acquire) && !read_pending {
                                if let Err(error) = transmit(&wire, &proof, next.fetch_add(1, Ordering::Relaxed), ClientRequest::ChatRead { chat_id: id, after_generation: None }) { break error; }
                                read_pending = true;
                            } else { read_again = true; }
                        }
                        if let Some(reply) = envelope.request_id.and_then(|id| pending.lock().unwrap_or_else(|p|p.into_inner()).remove(&id)) {
                            let _ = reply.try_send(Ok(terminal));
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
                        let _=reply.try_send(if Some(request)==envelope.request_id { Ok(terminal.clone()) } else { Err("Chat was explicitly closed".into()) });
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
                    let result = match &envelope.message { ServerMessage::Response { response: ServerResponse::Accepted | ServerResponse::Detached } => Ok(None), ServerMessage::Error(error) => Err(error.message.clone()), _ => Err("Unexpected chat command response".into()) };
                    let _ = reply.try_send(result);
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
                    ServerMessage::ChatAccountUsage { chat_id, report, observed_at } if chat_id == id => {
                        let first = account_usage.lock().unwrap_or_else(|p| p.into_inner())
                            .push(report, observed_at);
                        if first { wake(Wake::AccountUsage); }
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
                                let old = if *current_revision == revision && Arc::ptr_eq(&base, &current) { Some(publication.current.swap(proposed.take().unwrap())) } else { None };
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
                    read_again = false;
                    let after_generation = {
                        let state = publication.current.load();
                        (!resync && (state.resident || !body_requested.load(Ordering::Acquire))).then_some(state.snapshot.generation)
                    };
                    if let Err(error) = transmit(&wire, &proof, next.fetch_add(1, Ordering::Relaxed), ClientRequest::ChatRead { chat_id: id, after_generation }) { break error; }
                    read_pending = true;
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
                let _ = reply.try_send(Err(failure.clone()));
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
    pub(crate) fn upgrade_recovery(&self) -> Option<&(Session, Session)> {
        self.inner
            .upgrade_recovery
            .as_ref()
            .map(|recovery| &recovery.sessions)
    }
    pub(crate) fn authenticated_upgrade_recovery(&self) -> Option<&UpgradeRecovery> {
        self.inner.upgrade_recovery.as_ref()
    }
    pub(crate) fn is_connected(&self) -> bool {
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
    pub(crate) fn hidden_snapshot_with_clock(
        &self,
    ) -> Option<(Arc<Snapshot>, Option<SourceClock>)> {
        let (mut snapshot, clock) = self.snapshot_with_clock();
        if !self.inner.alive.load(Ordering::Acquire) {
            return None;
        }
        Arc::make_mut(&mut snapshot).evict_transcript();
        Some((snapshot, clock))
    }

    pub(crate) fn has_observed_user_item(&self) -> bool {
        self.inner.user_item_seen.load(Ordering::Acquire)
    }

    /// A renderable publication, absent until a hidden client's full history
    /// has been rehydrated. Metadata-only generations must never clear a
    /// runtime's scroll/disclosure state as if they were empty conversations.
    pub(crate) fn resident_snapshot_with_clock(
        &self,
    ) -> Option<(Arc<Snapshot>, Option<SourceClock>)> {
        let state = self.inner.publication.current.load();
        state
            .resident
            .then(|| (state.snapshot.clone(), state.clock))
    }

    #[cfg(test)]
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
            pending.insert(id, reply);
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
    pub(crate) fn known_append(
        &self,
        previous: &Arc<super::Item>,
        current: &Arc<super::Item>,
    ) -> bool {
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
    pub(crate) fn set_interest(&self, interested: bool, retain_body: bool) -> Result<(), String> {
        if !self.inner.alive.load(Ordering::Acquire) {
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
            if !self.inner.alive.load(Ordering::Acquire) {
                (None, current, Err("Chat is disconnected"))
            } else {
                let body_changed = self
                    .inner
                    .body_requested
                    .swap(retain_body, Ordering::AcqRel)
                    != retain_body;
                if body_changed {
                    *revision = revision
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
                    *revision,
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

#[cfg(test)]
mod tests {
    use super::*;

    static REPLACEMENT_GATES: Mutex<
        BTreeMap<usize, (std::sync::mpsc::Sender<()>, std::sync::mpsc::Receiver<()>)>,
    > = Mutex::new(BTreeMap::new());

    pub(super) fn pause_before_replacement(owner: usize) {
        let gate = REPLACEMENT_GATES
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&owner);
        if let Some((paused, resume)) = gate {
            paused.send(()).unwrap();
            resume.recv_timeout(Duration::from_secs(10)).unwrap();
        }
    }

    static PUBLICATION_GATES: Mutex<
        BTreeMap<TerminalId, (std::sync::mpsc::Sender<()>, std::sync::mpsc::Receiver<()>)>,
    > = Mutex::new(BTreeMap::new());
    pub(super) fn pause_before_publication(chat: TerminalId) {
        let gate = PUBLICATION_GATES
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&chat);
        if let Some((paused, resume)) = gate {
            paused.send(()).unwrap();
            resume.recv_timeout(Duration::from_secs(10)).unwrap();
        }
    }

    #[test]
    fn admission_capacity_is_typed_and_keeps_daemon_detail() {
        for (code, recovery) in [
            (
                ipc::ProtocolErrorCode::ChatCapacity,
                super::super::Recovery::Capacity,
            ),
            (
                ipc::ProtocolErrorCode::AccountRemoved,
                super::super::Recovery::Account,
            ),
            (
                ipc::ProtocolErrorCode::TerminalUnavailable,
                super::super::Recovery::Retry,
            ),
        ] {
            let failure = start_failure(ipc::ProtocolError::new(
                code,
                "128 resident; limit 128",
                true,
            ));
            assert_eq!(failure.recovery, recovery);
            assert_eq!(failure.message, "128 resident; limit 128");
        }
        // Text alone must never turn a provider failure into capacity recovery.
        assert_eq!(
            start_failure(ipc::ProtocolError::new(
                ipc::ProtocolErrorCode::TerminalUnavailable,
                "Daemon native chat capacity reached",
                true,
            ))
            .recovery,
            super::super::Recovery::Retry,
        );
    }

    #[test]
    fn source_clock_advances_in_daemon_time_despite_delayed_hydration() {
        let received = Instant::now();
        // The daemon can be arbitrarily ahead of or behind the display
        // machine. Neither its epoch nor receipt time comes from UI refresh.
        for sampled_at in [1_000, u64::MAX - 60_000] {
            let clock = SourceClock::from_sample(Some(sampled_at), received).unwrap();
            assert_eq!(clock.at(received), sampled_at);
            assert_eq!(
                clock.at(received + Duration::from_secs(12)),
                sampled_at + 12_000
            );
        }
        assert!(SourceClock::from_sample(None, received).is_none());
    }

    #[test]
    fn source_clock_never_rewinds_when_new_publications_arrive() {
        let received = Instant::now();
        let mut clock = SourceClock::from_sample(Some(10_000), received);
        SourceClock::observe(&mut clock, Some(9_000), received + Duration::from_secs(2));
        assert_eq!(clock.unwrap().at(received + Duration::from_secs(3)), 13_000);
        SourceClock::observe(&mut clock, Some(14_000), received + Duration::from_secs(3));
        assert_eq!(clock.unwrap().at(received + Duration::from_secs(4)), 15_000);
    }

    fn handoff_terminal() -> crate::model::TerminalSession {
        use crate::model::{LaunchRecipe, TerminalContinuity, TerminalSession};
        let recipe = LaunchRecipe {
            unset_environment: Default::default(),
            program: "unused".into(),
            arguments: Vec::new(),
            working_directory: "/".into(),
            environment: BTreeMap::new(),
        };
        TerminalSession {
            owned_codex_listener: None,
            recovery_origin: None,
            predecessor: None,
            id: TerminalId::new(),
            launch: recipe.clone(),
            continuity: TerminalContinuity::RelaunchAvailable {
                recipe,
                last_exit: None,
            },
            resume_hint: None,
            transcript_path: None,
        }
    }

    fn handoff_proof() -> HostSessionProof {
        HostSessionProof {
            host_session_id: crate::model::HostSessionId::new("session"),
            boot_id: crate::model::BootId::new("boot"),
            host_incarnation: crate::model::HostIncarnation::new("incarnation"),
        }
    }

    fn handoff_reply(
        server: &mut UnixStream,
        codec: &mut FrameCodec,
        request_id: Option<u64>,
        proof: &HostSessionProof,
        message: ServerMessage,
    ) {
        codec
            .write(
                server,
                &ServerEnvelope {
                    version: PROTOCOL_VERSION,
                    request_id,
                    proof: proof.clone(),
                    message,
                },
            )
            .unwrap();
    }

    fn handoff_handshake(server: &mut UnixStream, codec: &mut FrameCodec) {
        server
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let connect: ClientEnvelope = codec.read(server).unwrap().unwrap();
        assert!(matches!(
            connect.request,
            ClientRequest::Connect {
                expected_host: None,
                ..
            }
        ));
        let proof = handoff_proof();
        handoff_reply(
            server,
            codec,
            Some(connect.request_id),
            &proof,
            ServerMessage::Response {
                response: ServerResponse::Connected {
                    proof: proof.clone(),
                    resumed: Vec::new(),
                },
            },
        );
    }

    #[test]
    fn handoff_batch_continues_after_a_refusal_and_an_incomplete_saved_proof() {
        use crate::terminal_ipc::{ProtocolError, ProtocolErrorCode};
        let mut terminals = vec![
            handoff_terminal(),
            handoff_terminal(),
            handoff_terminal(),
            handoff_terminal(),
        ];
        terminals[1].continuity = crate::model::TerminalContinuity::DetachedSameBoot {
            host_session_id: crate::model::HostSessionId::new(""),
            boot_id: handoff_proof().boot_id,
            host_incarnation: handoff_proof().host_incarnation,
        };
        let expected = [terminals[0].id, terminals[2].id, terminals[3].id];
        let (client, mut server) = UnixStream::pair().unwrap();
        let worker = thread::spawn(move || {
            let mut codec = FrameCodec::new(ipc::MAX_FRAME_BYTES);
            handoff_handshake(&mut server, &mut codec);
            for (index, terminal_id) in expected.into_iter().enumerate() {
                let request: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
                assert_eq!(request.request_id, index as u64 + 2);
                assert!(
                    matches!(request.request, ClientRequest::ChatCommitTerminal { terminal_id: actual } if actual == terminal_id)
                );
                assert_eq!(request.proof, Some(handoff_proof()));
                let message = if index == 0 {
                    ServerMessage::Error(ProtocolError::new(
                        ProtocolErrorCode::InvalidRequest,
                        "receipt refused",
                        false,
                    ))
                } else {
                    ServerMessage::Response {
                        response: ServerResponse::Accepted,
                    }
                };
                handoff_reply(
                    &mut server,
                    &mut codec,
                    Some(request.request_id),
                    &handoff_proof(),
                    message,
                );
            }
            assert!(codec.read::<ClientEnvelope>(&mut server).unwrap().is_none());
        });
        let results = acknowledge_terminal_handoffs_on(
            crate::control::ControlClient::from_stream(client),
            &terminals,
        );
        worker.join().unwrap();
        assert!(results[0].as_ref().unwrap_err().contains("receipt refused"));
        assert!(
            results[1]
                .as_ref()
                .unwrap_err()
                .contains("incomplete daemon proof")
        );
        assert_eq!(&results[2..], &[Ok(()), Ok(())]);
    }

    #[test]
    fn handoff_batch_preserves_success_but_stops_using_a_failed_connection() {
        use crate::terminal_ipc::{ProtocolError, ProtocolErrorCode};
        // EOF, proof changes, uncorrelated drop notices, and mismatched request
        // IDs or unexpected response types must not cause any more requests
        // on a compromised connection.
        for failure in 0..5 {
            let mut terminals = vec![
                handoff_terminal(),
                handoff_terminal(),
                handoff_terminal(),
                handoff_terminal(),
            ];
            terminals[3].continuity = crate::model::TerminalContinuity::DetachedSameBoot {
                host_session_id: handoff_proof().host_session_id,
                boot_id: handoff_proof().boot_id,
                host_incarnation: crate::model::HostIncarnation::new("previous"),
            };
            let (client, mut server) = UnixStream::pair().unwrap();
            let worker = thread::spawn(move || {
                let mut codec = FrameCodec::new(ipc::MAX_FRAME_BYTES);
                handoff_handshake(&mut server, &mut codec);
                let first: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
                handoff_reply(
                    &mut server,
                    &mut codec,
                    Some(first.request_id),
                    &handoff_proof(),
                    ServerMessage::Response {
                        response: ServerResponse::Accepted,
                    },
                );
                let second: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
                if failure == 0 {
                    return;
                }
                let mut proof = handoff_proof();
                if failure == 1 {
                    proof.host_incarnation = crate::model::HostIncarnation::new("changed");
                }
                let (id, message) = if failure == 2 {
                    (
                        None,
                        ServerMessage::Error(ProtocolError::new(
                            ProtocolErrorCode::QueueFull,
                            "dropping connection",
                            true,
                        )),
                    )
                } else {
                    (
                        Some(second.request_id + u64::from(failure == 3)),
                        ServerMessage::Response {
                            response: if failure == 4 {
                                ServerResponse::Listed {
                                    terminals: Vec::new(),
                                }
                            } else {
                                ServerResponse::Accepted
                            },
                        },
                    )
                };
                handoff_reply(&mut server, &mut codec, id, &proof, message);
                assert!(codec.read::<ClientEnvelope>(&mut server).unwrap().is_none());
            });
            let results = acknowledge_terminal_handoffs_on(
                crate::control::ControlClient::from_stream(client),
                &terminals,
            );
            worker.join().unwrap();
            assert_eq!(results[0], Ok(()));
            assert!(results[1].is_err());
            assert_eq!(results[1], results[2]);
            assert_eq!(results[3], Ok(()));
        }
    }

    #[test]
    fn only_missing_or_refused_socket_connections_retire_the_whole_handoff_batch() {
        use crate::control::{ControlClient, ControlError};
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("daemon.sock");
        let terminals = [handoff_terminal(), handoff_terminal()];
        assert_eq!(
            acknowledge_terminal_handoffs_on(ControlClient::connect(&socket), &terminals),
            vec![Ok(()), Ok(())]
        );
        // Inject refusal explicitly: parallel subprocesses can briefly inherit a
        // listener before exec, so dropping it does not guarantee immediate refusal.
        assert_eq!(
            acknowledge_terminal_handoffs_on(
                Err(ControlError::Connect(io::Error::from(
                    io::ErrorKind::ConnectionRefused
                ))),
                &terminals,
            ),
            vec![Ok(()), Ok(())]
        );
        for error in [
            ControlError::Connect(io::Error::from(io::ErrorKind::PermissionDenied)),
            ControlError::Connect(io::Error::from(io::ErrorKind::ConnectionReset)),
            ControlError::Configure(io::Error::from(io::ErrorKind::NotFound)),
            ControlError::ProofMismatch,
            ControlError::ConnectionClosed,
        ] {
            assert!(
                acknowledge_terminal_handoffs_on(Err(error), &terminals)
                    .iter()
                    .all(Result::is_err)
            );
        }
    }

    /// The conversation the live installation's card was showing: a UUIDv7,
    /// which every identifier gate on the restore path has to accept.
    const CONVERSATION: &str = "01a0ab66-21d6-7065-9fe5-95f302b669e1";

    #[test]
    fn exited_terminal_commit_discovers_host_but_pins_the_acknowledgement() {
        use crate::model::{
            BootId, HostIncarnation, HostSessionId, LaunchRecipe, TerminalContinuity,
            TerminalSession,
        };
        let proof = HostSessionProof {
            host_session_id: HostSessionId::new("session"),
            boot_id: BootId::new("boot"),
            host_incarnation: HostIncarnation::new("incarnation"),
        };
        let recipe = LaunchRecipe {
            unset_environment: Default::default(),
            program: "unused".into(),
            arguments: Vec::new(),
            working_directory: "/".into(),
            environment: BTreeMap::new(),
        };
        let terminal = TerminalSession {
            owned_codex_listener: None,
            recovery_origin: None,
            predecessor: None,
            id: TerminalId::new(),
            launch: recipe.clone(),
            continuity: TerminalContinuity::RelaunchAvailable {
                recipe,
                last_exit: None,
            },
            resume_hint: None,
            transcript_path: None,
        };
        // A persisted descriptor with no live proof still commits on the
        // discovered host, as one naming that host does. An incarnation change
        // after discovery must fail.
        for (live, changed) in [(false, false), (false, true), (true, false), (true, true)] {
            let mut terminal = terminal.clone();
            if live {
                terminal.continuity = TerminalContinuity::DetachedSameBoot {
                    host_session_id: proof.host_session_id.clone(),
                    boot_id: proof.boot_id.clone(),
                    host_incarnation: proof.host_incarnation.clone(),
                };
            }
            let (client, mut server) = UnixStream::pair().unwrap();
            let proof = proof.clone();
            let worker = thread::spawn(move || {
                server
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                server
                    .set_write_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut codec = FrameCodec::new(ipc::MAX_FRAME_BYTES);
                let connect: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
                codec
                    .write(
                        &mut server,
                        &ServerEnvelope {
                            version: PROTOCOL_VERSION,
                            request_id: Some(connect.request_id),
                            proof: proof.clone(),
                            message: ServerMessage::Response {
                                response: ServerResponse::Connected {
                                    proof: proof.clone(),
                                    resumed: Vec::new(),
                                },
                            },
                        },
                    )
                    .unwrap();
                let commit: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
                assert!(matches!(
                    commit.request,
                    ClientRequest::ChatCommitTerminal { .. }
                ));
                let response_proof = if changed {
                    HostSessionProof {
                        host_incarnation: HostIncarnation::new("replacement"),
                        ..proof
                    }
                } else {
                    proof
                };
                codec
                    .write(
                        &mut server,
                        &ServerEnvelope {
                            version: PROTOCOL_VERSION,
                            request_id: Some(commit.request_id),
                            proof: response_proof,
                            message: ServerMessage::Response {
                                response: ServerResponse::Accepted,
                            },
                        },
                    )
                    .unwrap();
            });
            let result = acknowledge_terminal_handoffs_on(
                crate::control::ControlClient::from_stream(client),
                &[terminal],
            );
            worker.join().unwrap();
            assert_eq!(result[0].is_err(), changed);
        }
    }

    #[test]
    fn a_replacement_daemon_retires_the_previous_daemons_handoff() {
        use crate::model::{
            BootId, HostIncarnation, HostSessionId, LaunchRecipe, TerminalContinuity,
            TerminalSession,
        };
        let previous = HostSessionProof {
            host_session_id: HostSessionId::new("session"),
            boot_id: BootId::new("boot"),
            host_incarnation: HostIncarnation::new("previous"),
        };
        let replacement = HostSessionProof {
            host_incarnation: HostIncarnation::new("replacement"),
            ..previous.clone()
        };
        // A closed card saved before its daemon was replaced still carries the
        // previous daemon's identity; its receipt went away with that daemon.
        let terminal = TerminalSession {
            owned_codex_listener: None,
            recovery_origin: None,
            predecessor: None,
            id: TerminalId::new(),
            launch: LaunchRecipe {
                unset_environment: Default::default(),
                program: "unused".into(),
                arguments: Vec::new(),
                working_directory: "/".into(),
                environment: BTreeMap::new(),
            },
            continuity: TerminalContinuity::DetachedSameBoot {
                host_session_id: previous.host_session_id.clone(),
                boot_id: previous.boot_id.clone(),
                host_incarnation: previous.host_incarnation.clone(),
            },
            resume_hint: None,
            transcript_path: None,
        };
        let (client, mut server) = UnixStream::pair().unwrap();
        let worker = thread::spawn(move || {
            server
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut codec = FrameCodec::new(ipc::MAX_FRAME_BYTES);
            let connect: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
            // Pinning the previous identity would only draw a refusal.
            assert!(matches!(
                connect.request,
                ClientRequest::Connect {
                    expected_host: None,
                    ..
                }
            ));
            codec
                .write(
                    &mut server,
                    &ServerEnvelope {
                        version: PROTOCOL_VERSION,
                        request_id: Some(connect.request_id),
                        proof: replacement.clone(),
                        message: ServerMessage::Response {
                            response: ServerResponse::Connected {
                                proof: replacement,
                                resumed: Vec::new(),
                            },
                        },
                    },
                )
                .unwrap();
            // Nothing more is asked of a daemon that never held the receipt.
            codec.read::<ClientEnvelope>(&mut server).unwrap()
        });
        assert_eq!(
            acknowledge_terminal_handoffs_on(
                crate::control::ControlClient::from_stream(client),
                &[terminal],
            ),
            vec![Ok(())]
        );
        assert!(worker.join().unwrap().is_none());
    }

    #[test]
    fn selecting_then_leaving_fences_queued_wakes_and_old_wire_observations() {
        let mut observed = OutputObservation::default();
        let (wake, queued) = crossbeam_channel::unbounded();
        // Hidden output queues a wake, but selection acknowledges before it runs.
        assert!(observed.observe(0));
        wake.send(Wake::Observed).unwrap();
        let epoch = observed.begin_acknowledgement();
        assert_eq!(epoch, 1);
        // The worktree is hidden again when the pre-selection wake executes.
        assert!(matches!(queued.recv().unwrap(), Wake::Observed));
        assert!(
            !observed.take(),
            "selection must consume the old unread edge"
        );
        // An already-written daemon frame can arrive after the queued wake.
        assert!(!observed.observe(0));
        assert!(
            !observed.take(),
            "old wire output must not resurrect unread"
        );
        assert!(observed.observe(epoch));
        assert!(
            !observed.observe(epoch),
            "fresh output coalesces until drained"
        );
        assert!(!observed.observe(0));
        assert!(
            observed.take(),
            "rejecting old output must preserve fresh output"
        );
        assert!(!observed.take());
        // A stale queued wake may consume fresh output; its matching wake must
        // then be harmless, rather than produce a second unread transition.
        assert!(observed.observe(epoch));
        wake.send(Wake::Observed).unwrap();
        let next_epoch = observed.begin_acknowledgement();
        assert!(next_epoch > epoch);
        assert!(observed.observe(next_epoch));
        wake.send(Wake::Observed).unwrap();
        assert!(matches!(queued.recv().unwrap(), Wake::Observed));
        assert!(observed.take());
        assert!(matches!(queued.recv().unwrap(), Wake::Observed));
        assert!(!observed.take());
    }

    #[test]
    fn activity_wake_coalesces_until_consumed_and_fences_stale_attach_edges() {
        let mut activity = ActivityObservation {
            generation: 5,
            state: ActivityState::Working,
            pending: false,
        };
        assert!(!activity.observe(4, ActivityState::Idle));
        assert_eq!(activity.take(), ActivityState::Working);
        assert!(activity.observe(6, ActivityState::Waiting));
        assert!(!activity.observe(7, ActivityState::Idle));
        assert_eq!(activity.take(), ActivityState::Idle);
        assert!(!activity.observe(8, ActivityState::Idle));
        assert!(activity.observe(9, ActivityState::Working));
        assert_eq!(activity.take(), ActivityState::Working);
    }

    fn host_proof(incarnation: &str) -> HostSessionProof {
        use crate::model::{BootId, HostIncarnation, HostSessionId};
        HostSessionProof {
            host_session_id: HostSessionId::new("session"),
            boot_id: BootId::new("boot"),
            host_incarnation: HostIncarnation::new(incarnation),
        }
    }

    /// The live installation's own card: a native OMP conversation whose
    /// persisted launch names its session directory and nothing else.
    fn omp_card(conversation: Option<&str>, proof: Option<HostSessionProof>) -> Session {
        Session {
            native_continuity: Default::default(),
            owned_codex_listener: None,
            id: TerminalId::new(),
            launch: crate::chat::Launch {
                provider: crate::agent::Provider::Omp,
                recipe: crate::model::LaunchRecipe {
                    unset_environment: Default::default(),
                    program: "omp".into(),
                    arguments: vec![
                        "--session-dir".into(),
                        "/home/user/.omp/agent/sessions/native".into(),
                    ],
                    working_directory: "/".into(),
                    environment: BTreeMap::new(),
                },
                session_id: conversation.map(str::to_owned),
            },
            proof,
            terminal: None,
        }
    }

    /// Runs the restore negotiation against a daemon that answers the upgrade
    /// lineage question with `answer`, and returns what it was asked.
    fn negotiate(
        live: &HostSessionProof,
        session: Session,
        answer: ServerMessage,
    ) -> (Result<ClientRequest, StartFailure>, ClientRequest) {
        let (outcome, request, _) = negotiate_with_recovery(live, session, answer);
        (outcome, request)
    }

    #[test]
    fn proofless_restore_attaches_the_exact_hosted_writer() {
        let live = host_proof("live");
        for durable in [false, true] {
            for named in [false, true] {
                let mut saved = omp_card(named.then_some(CONVERSATION), None);
                if durable {
                    saved.native_continuity = super::super::NativeContinuity::Durable {
                        provider_version: "test".into(),
                    };
                }
                let mut hosted = saved.clone();
                hosted.proof = Some(live.clone());
                hosted.launch.session_id = Some(CONVERSATION.into());
                let (outcome, asked) = negotiate(
                    &live,
                    saved,
                    ServerMessage::Response {
                        response: ServerResponse::ChatListed {
                            sessions: vec![hosted.clone()],
                        },
                    },
                );
                assert!(matches!(asked, ClientRequest::ChatList));
                assert!(
                    matches!(outcome, Ok(ClientRequest::ChatAttach { session }) if session == hosted)
                );
            }
        }
    }

    #[test]
    fn proofless_restore_refuses_a_conflicting_hosted_identity() {
        let live = host_proof("live");
        let saved = omp_card(Some(CONVERSATION), None);
        let mut hosted = saved.clone();
        hosted.proof = Some(live.clone());
        for conflict in 0..4 {
            let mut other = hosted.clone();
            match conflict {
                0 => other.proof = Some(host_proof("stale")),
                1 => other.launch.provider = crate::agent::Provider::Claude,
                2 => other.launch.recipe.working_directory = "/other".into(),
                _ => other.launch.session_id = Some("other-conversation".into()),
            }
            let (outcome, _) = negotiate(
                &live,
                saved.clone(),
                ServerMessage::Response {
                    response: ServerResponse::ChatListed {
                        sessions: vec![other],
                    },
                },
            );
            assert!(
                outcome.is_err(),
                "identity conflict must not start or attach"
            );
        }
    }

    #[test]
    fn proofless_restore_without_a_hosted_writer_preserves_restart_policy() {
        let live = host_proof("live");
        for durable in [false, true] {
            let mut saved = omp_card(Some(CONVERSATION), None);
            if durable {
                saved.native_continuity = super::super::NativeContinuity::Durable {
                    provider_version: "test".into(),
                };
            }
            let (outcome, _) = negotiate(
                &live,
                saved,
                ServerMessage::Response {
                    response: ServerResponse::ChatListed { sessions: vec![] },
                },
            );
            if durable {
                assert!(outcome.is_err(), "uncertain durable work must not restart");
            } else {
                assert!(matches!(outcome, Ok(ClientRequest::ChatStart { .. })));
            }
        }
    }

    #[test]
    fn failed_saved_claude_restore_sends_no_start_request() {
        let (stream, mut peer) = UnixStream::pair().unwrap();
        let live = host_proof("current");
        let mut source = omp_card(Some(CONVERSATION), Some(host_proof("old")));
        source.launch.provider = crate::agent::Provider::Claude;
        source.launch.recipe.program = "must-not-run".into();
        source.launch.recipe.arguments.clear();
        let expected = source.clone();
        let worker = thread::spawn(move || {
            peer.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            for step in 0..2 {
                let request: ClientEnvelope = ipc::read_frame(&mut peer).unwrap().unwrap();
                let message = match (step, request.request) {
                    (0, ClientRequest::Connect { .. }) => ServerMessage::Response {
                        response: ServerResponse::Connected {
                            proof: live.clone(),
                            resumed: Vec::new(),
                        },
                    },
                    (1, ClientRequest::ResolveUpgradeChat { source }) if source == expected => {
                        ServerMessage::Error(ipc::ProtocolError::new(
                            ipc::ProtocolErrorCode::TerminalUnavailable,
                            "Exact history is unavailable",
                            false,
                        ))
                    }
                    other => panic!("Unexpected saved history request: {other:?}"),
                };
                ipc::write_frame(
                    &mut peer,
                    &ServerEnvelope {
                        version: PROTOCOL_VERSION,
                        request_id: Some(request.request_id),
                        proof: live.clone(),
                        message,
                    },
                )
                .unwrap();
            }
            let extra: Option<ClientEnvelope> = ipc::read_frame(&mut peer).unwrap();
            assert!(extra.is_none(), "a failed restore must not send ChatStart");
        });
        let result = ClientHandle::connect_on(
            stream,
            source,
            Intent::Restore,
            None,
            false,
            None,
            None,
            None,
            |_| {},
        );
        assert!(result.is_err());
        worker.join().unwrap();
    }

    #[test]
    fn explicit_retry_classifies_exact_history_reader_and_live_successor() {
        for state in 0..5 {
            let proof = host_proof("current");
            let mut source = omp_card(Some(CONVERSATION), Some(proof.clone()));
            source.launch.provider = crate::agent::Provider::Claude;
            source.launch.recipe.program = "claude".into();
            source.launch.recipe.arguments.clear();
            let mut hosted = source.clone();
            if state == 1 {
                hosted.native_continuity =
                    super::super::NativeContinuity::RetainedClaudeActor { protocol: 2 };
            } else if state == 2 || state == 4 {
                source.proof = Some(host_proof("previous"));
            }
            let expected = hosted.clone();
            let expected_source = source.clone();
            let (mut client, mut server) = UnixStream::pair().unwrap();
            let server_proof = proof.clone();
            let worker = thread::spawn(move || {
                server
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut codec = FrameCodec::new(ipc::MAX_FRAME_BYTES);
                for step in 0..2 {
                    let request: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
                    let response = match (step, request.request) {
                        (0, ClientRequest::ChatList) => ServerResponse::ChatListed {
                            sessions: if state < 3 {
                                vec![expected.clone()]
                            } else {
                                vec![]
                            },
                        },
                        (1, ClientRequest::ChatPrepareResume { source })
                            if state < 3 && source == expected =>
                        {
                            ServerResponse::ChatResumePrepared {
                                session: expected.clone(),
                            }
                        }
                        (1, ClientRequest::ResolveUpgradeChat { source })
                            if state >= 3 && source == expected_source =>
                        {
                            ServerResponse::UpgradeChatResolved {
                                session: expected.clone(),
                            }
                        }
                        other => panic!("unexpected explicit retry request: {other:?}"),
                    };
                    codec
                        .write(
                            &mut server,
                            &ServerEnvelope {
                                version: PROTOCOL_VERSION,
                                request_id: Some(request.request_id),
                                proof: server_proof.clone(),
                                message: ServerMessage::Response { response },
                            },
                        )
                        .unwrap();
                }
            });
            let result = restore(
                &mut client,
                &mut FrameCodec::new(ipc::MAX_FRAME_BYTES),
                &mut 1,
                &proof,
                source,
                &mut None,
                true,
            );
            worker.join().unwrap();
            match state {
                0 | 3 => assert!(
                    matches!(result, Ok(ClientRequest::ChatStart { session, source: None, terminate_source: false, .. }) if session == hosted)
                ),
                1 => assert!(
                    matches!(result, Ok(ClientRequest::ChatAttach { session }) if session == hosted)
                ),
                2 | 4 => assert!(
                    result
                        .unwrap_err()
                        .message
                        .contains("changed the explicit retry request")
                ),
                _ => unreachable!(),
            }
        }
    }

    fn negotiate_with_recovery(
        live: &HostSessionProof,
        session: Session,
        answer: ServerMessage,
    ) -> (
        Result<ClientRequest, StartFailure>,
        ClientRequest,
        Option<(Session, Session)>,
    ) {
        let (mut client, mut server) = UnixStream::pair().unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let proof = live.clone();
        let worker = thread::spawn(move || {
            let mut codec = FrameCodec::new(ipc::MAX_FRAME_BYTES);
            let envelope: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
            codec
                .write(
                    &mut server,
                    &ServerEnvelope {
                        version: PROTOCOL_VERSION,
                        request_id: Some(envelope.request_id),
                        proof,
                        message: answer,
                    },
                )
                .unwrap();
            envelope.request
        });
        let mut codec = FrameCodec::new(ipc::MAX_FRAME_BYTES);
        let mut request_id = 2;
        let mut recovery = None;
        let outcome = restore(
            &mut client,
            &mut codec,
            &mut request_id,
            live,
            session,
            &mut recovery,
            false,
        );
        (outcome, worker.join().unwrap(), recovery)
    }

    #[test]
    fn same_thread_codex_resume_normalization_requires_explicit_rebind() {
        let previous = host_proof("previous");
        let live = host_proof("live");
        let mut source = omp_card(None, Some(previous.clone()));
        source.launch.provider = crate::agent::Provider::Codex;
        source.launch.recipe.program = "codex".into();
        source.launch.recipe.arguments = vec!["--remote".into(), "unix://".into()];
        assert!(
            source.claim_owned_codex_listener(std::path::Path::new("/run/test-native"), &previous)
        );
        source.launch.session_id = Some(CONVERSATION.into());
        source.native_continuity = super::super::NativeContinuity::Durable {
            provider_version: "0.100.0".into(),
        };
        let mut replacement = source.clone();
        replacement.proof = Some(live.clone());
        replacement
            .launch
            .recipe
            .arguments
            .splice(0..0, ["resume".into(), CONVERSATION.into()]);
        let (outcome, _, recovery) = negotiate_with_recovery(
            &live,
            source.clone(),
            ServerMessage::Response {
                response: ServerResponse::UpgradeChatResolved {
                    session: replacement.clone(),
                },
            },
        );
        assert!(
            matches!(outcome, Ok(ClientRequest::ChatAttach { session }) if session == replacement)
        );
        assert_eq!(recovery, Some((source.clone(), replacement.clone())));
        for alteration in 0..5 {
            let mut invalid = replacement.clone();
            match alteration {
                0 => invalid.launch.recipe.arguments[1] = "other-thread".into(),
                1 => invalid.launch.recipe.arguments.push("--different".into()),
                2 => {
                    invalid
                        .launch
                        .recipe
                        .environment
                        .insert("ACCOUNT_HOME".into(), "/other".into());
                }
                3 => invalid.launch.recipe.program = "other-provider".into(),
                _ => invalid.owned_codex_listener = None,
            }
            let (outcome, _, recovery) = negotiate_with_recovery(
                &live,
                source.clone(),
                ServerMessage::Response {
                    response: ServerResponse::UpgradeChatResolved { session: invalid },
                },
            );
            assert!(outcome.is_err(), "alteration {alteration}");
            assert!(recovery.is_none());
        }
    }

    #[test]
    fn authenticated_upgrade_rotation_preserves_provenance_and_known_thread_identity() {
        let live = host_proof("live");
        for previous in [None, Some(CONVERSATION), Some("previous-conversation")] {
            let source = omp_card(previous, Some(host_proof("previous")));
            let mut resolved = source.clone();
            resolved.proof = Some(live.clone());
            resolved.launch.session_id = Some(CONVERSATION.into());
            let (outcome, _, recovery) = negotiate_with_recovery(
                &live,
                source.clone(),
                ServerMessage::Response {
                    response: ServerResponse::UpgradeChatResolved {
                        session: resolved.clone(),
                    },
                },
            );
            if previous.is_some_and(|id| id != CONVERSATION) {
                assert!(outcome.is_err(), "a known provider thread cannot rotate");
                assert!(recovery.is_none());
            } else {
                assert!(
                    matches!(outcome, Ok(ClientRequest::ChatAttach { session }) if session == resolved)
                );
                assert_eq!(recovery, Some((source, resolved)));
            }
        }
        let source = omp_card(Some(CONVERSATION), Some(host_proof("previous")));
        let mut resolved = source.clone();
        resolved.proof = Some(live.clone());
        resolved.launch.session_id = Some(CONVERSATION.into());
        resolved
            .launch
            .recipe
            .environment
            .insert("ACCOUNT_HOME".into(), "/different".into());
        let (outcome, _, recovery) = negotiate_with_recovery(
            &live,
            source,
            ServerMessage::Response {
                response: ServerResponse::UpgradeChatResolved { session: resolved },
            },
        );
        assert!(outcome.is_err());
        assert!(recovery.is_none());
    }

    /// An application update replaces the executables and restarts the daemon,
    /// so every saved chat proof is stale afterwards and the daemon holds no
    /// upgrade checkpoint for a restart it did not perform itself. A card that
    /// treated that as an exact-attach failure never started the provider that
    /// would have read its conversation back.
    #[test]
    fn a_restarted_daemon_still_resumes_the_conversation_a_card_was_showing() {
        let live = host_proof("live");
        let stale = host_proof("before-the-update");

        // The hosting daemon is still the live one, so its writer is there to
        // attach to and nothing needs to be asked first.
        let (mut client, _server) = UnixStream::pair().unwrap();
        let mut codec = FrameCodec::new(ipc::MAX_FRAME_BYTES);
        let mut request_id = 2;
        let attach = restore(
            &mut client,
            &mut codec,
            &mut request_id,
            &live,
            omp_card(Some(CONVERSATION), Some(live.clone())),
            &mut None,
            false,
        )
        .expect("a live proof attaches");
        assert!(matches!(attach, ClientRequest::ChatAttach { .. }));
        assert_eq!(request_id, 2, "an attach asks the daemon nothing first");

        // The saved proof is stale and the restart left no lineage, which is
        // every card after an installer-driven update.
        let (outcome, asked) = negotiate(
            &live,
            omp_card(Some(CONVERSATION), Some(stale.clone())),
            ServerMessage::Error(crate::terminal_ipc::ProtocolError::new(
                crate::terminal_ipc::ProtocolErrorCode::TerminalUnavailable,
                "No durable upgrade replacement exists for this native chat",
                false,
            )),
        );
        assert!(
            matches!(asked, ClientRequest::ResolveUpgradeChat { .. }),
            "a stale proof asks the daemon for its upgrade lineage before giving up on attaching"
        );
        let ClientRequest::ChatStart {
            session,
            source,
            terminate_source,
            ..
        } = outcome.expect("a refused lineage resumes rather than failing")
        else {
            panic!("a card with a saved conversation must start the provider on it");
        };
        assert_eq!(
            session.launch.session_id.as_deref(),
            Some(CONVERSATION),
            "the exact saved conversation is what the provider is started on"
        );
        assert_eq!(
            session.proof.as_ref(),
            Some(&live),
            "the start carries the proof the live daemon just gave, not the saved one"
        );
        assert!(source.is_none() && !terminate_source);

        // A conversation converted from a terminal resumes the same way, but
        // must carry no client-chosen execution environment: the daemon
        // refuses one outright for any chat that still names a source
        // terminal, and a refused start is a card that does not come back.
        let mut converted = omp_card(Some(CONVERSATION), Some(stale));
        converted.terminal = Some(crate::model::TerminalSession {
            owned_codex_listener: None,
            recovery_origin: None,
            predecessor: None,
            id: converted.id,
            launch: converted.launch.recipe.clone(),
            continuity: crate::model::TerminalContinuity::RelaunchAvailable {
                recipe: converted.launch.recipe.clone(),
                last_exit: None,
            },
            resume_hint: None,
            transcript_path: None,
        });
        let (outcome, _) = negotiate(
            &live,
            converted,
            ServerMessage::Error(crate::terminal_ipc::ProtocolError::new(
                crate::terminal_ipc::ProtocolErrorCode::TerminalUnavailable,
                "No durable upgrade replacement exists for this native chat",
                false,
            )),
        );
        let ClientRequest::ChatStart {
            execution_environment,
            ..
        } = outcome.expect("a terminal-derived card resumes too")
        else {
            panic!("a card with a saved conversation must start the provider on it");
        };
        assert!(
            execution_environment.0.is_empty(),
            "a chat naming a source terminal takes the daemon's environment, never a client's"
        );
    }

    /// A daemon that did upgrade itself can still map the saved descriptor onto
    /// the conversation it restarted under, and that attachment wins over
    /// starting a second provider on the same conversation.
    /// A pane saved showing an agent's chat, paired with the agent's
    /// console, rejoins the running actor after the app or the daemon
    /// restarts. It never starts the provider again beside the agent.
    #[test]
    fn an_agent_chat_saved_with_its_console_restores_by_attaching() {
        let live = host_proof("live");
        let mut saved = omp_card(Some(CONVERSATION), Some(live.clone()));
        saved.terminal = Some(crate::model::TerminalSession {
            id: saved.id,
            owned_codex_listener: None,
            recovery_origin: None,
            predecessor: None,
            launch: crate::model::LaunchRecipe {
                unset_environment: Default::default(),
                program: "cagents".into(),
                arguments: vec![
                    "agent".into(),
                    "console".into(),
                    crate::model::AgentId::new().to_string(),
                ],
                working_directory: "/".into(),
                environment: BTreeMap::new(),
            },
            transcript_path: None,
            resume_hint: None,
            continuity: crate::model::TerminalContinuity::Live {
                host_session_id: live.host_session_id.clone(),
                boot_id: live.boot_id.clone(),
                host_incarnation: live.host_incarnation.clone(),
            },
        });
        // The same daemon still runs it.
        let (mut client, _peer) = UnixStream::pair().unwrap();
        let outcome = restore(
            &mut client,
            &mut FrameCodec::new(ipc::MAX_FRAME_BYTES),
            &mut 2,
            &live,
            saved.clone(),
            &mut None,
            false,
        );
        assert!(
            matches!(&outcome, Ok(ClientRequest::ChatAttach { session }) if *session == saved),
            "{outcome:?}"
        );
        // A restarted daemon proves the agent's actor is the saved one.
        let mut before = saved.clone();
        before.proof = Some(host_proof("before-the-restart"));
        let (outcome, _) = negotiate(
            &live,
            before,
            ServerMessage::Response {
                response: ServerResponse::UpgradeChatResolved {
                    session: saved.clone(),
                },
            },
        );
        assert!(
            matches!(&outcome, Ok(ClientRequest::ChatAttach { session }) if *session == saved),
            "{outcome:?}"
        );
    }

    #[test]
    fn a_proven_upgrade_lineage_attaches_instead_of_starting_a_second_provider() {
        let live = host_proof("live");
        let saved = omp_card(Some(CONVERSATION), Some(host_proof("before-the-upgrade")));
        let mut restored = saved.clone();
        restored.proof = Some(live.clone());
        let (outcome, _) = negotiate(
            &live,
            saved.clone(),
            ServerMessage::Response {
                response: ServerResponse::UpgradeChatResolved {
                    session: restored.clone(),
                },
            },
        );
        let ClientRequest::ChatAttach { session } = outcome.expect("a proven lineage attaches")
        else {
            panic!("a proven lineage must attach");
        };
        assert_eq!(session, restored);

        let mut wrong_thread = restored.clone();
        wrong_thread.launch.session_id = Some("different-known-thread".into());
        let (outcome, _) = negotiate(
            &live,
            saved.clone(),
            ServerMessage::Response {
                response: ServerResponse::UpgradeChatResolved {
                    session: wrong_thread,
                },
            },
        );
        assert!(
            outcome.is_err(),
            "a confirmed replacement cannot change a saved known thread"
        );

        // A replacement for a different chat proves nothing about this one.
        // A known conversation identifier remains part of its exact source.
        let mut other = restored.clone();
        other.id = TerminalId::new();
        let (outcome, _) = negotiate(
            &live,
            saved,
            ServerMessage::Response {
                response: ServerResponse::UpgradeChatResolved { session: other },
            },
        );
        assert!(
            matches!(outcome, Ok(ClientRequest::ChatStart { .. })),
            "a mismatched lineage falls back to resuming, never to attaching"
        );
    }

    /// A conversation that cannot be resumed and cannot be named is the one
    /// case where retrying is not a real answer, so the card is told to offer
    /// the only thing left instead of being handed a retry that cannot work.
    #[test]
    fn a_card_with_nothing_to_resume_asks_for_a_new_conversation_in_the_daemons_words() {
        let live = host_proof("live");
        let (outcome, _) = negotiate(
            &live,
            omp_card(None, Some(host_proof("before-the-update"))),
            ServerMessage::Error(crate::terminal_ipc::ProtocolError::new(
                crate::terminal_ipc::ProtocolErrorCode::TerminalUnavailable,
                "No durable upgrade replacement exists for this native chat",
                false,
            )),
        );
        let failure = outcome.expect_err("nothing can be resumed here");
        assert_eq!(failure.recovery, crate::chat::Recovery::Fresh);
        assert!(
            failure
                .message
                .contains("No durable upgrade replacement exists for this native chat"),
            "the daemon's own refusal has to reach the card: {}",
            failure.message
        );
    }

    /// Drives a whole restore against a daemon that has just restarted: it
    /// proves a new identity, holds no upgrade checkpoint for a restart it did
    /// not perform, and then answers the resume with `answer`.
    fn restore_against(
        session: Session,
        answer: impl FnOnce(ClientRequest) -> ServerMessage + Send + 'static,
    ) -> Result<ClientHandle, StartFailure> {
        restore_with_lineage(session, None, answer)
    }

    pub(super) fn restore_with_lineage(
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

    #[test]
    fn canonical_upgrade_requires_exact_lineage_before_attach_and_never_starts() {
        // 0: exact success; 1: no lineage; 2: different resolved descriptor;
        // 3: changed live daemon; 4: changed attached descriptor;
        // 5: unconfirmed snapshot; 6: invalid canonical source transition;
        // 7: an unnamed source learns its authenticated identity.
        for case in 0..8 {
            let source = omp_card(
                (case != 7).then_some(CONVERSATION),
                Some(host_proof("prior")),
            );
            let mut expected = source.clone();
            expected.proof = Some(host_proof("live"));
            expected.launch.session_id = Some(CONVERSATION.into());
            if case == 6 {
                expected.launch.recipe.program = "another-program".into();
            }
            let mut live = host_proof("live");
            if case == 3 {
                live = host_proof("unexpected-live");
            }
            let (client, mut server) = UnixStream::pair().unwrap();
            let original = source.clone();
            let canonical = expected.clone();
            let worker = thread::spawn(move || {
                server
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                server
                    .set_write_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut codec = FrameCodec::new(ipc::MAX_FRAME_BYTES);
                let mut requests = Vec::new();
                while let Some(request) = codec.read::<ClientEnvelope>(&mut server).unwrap() {
                    let finished = matches!(&request.request, ClientRequest::ChatAttach { .. });
                    let response = match &request.request {
                        ClientRequest::Connect { expected_host, .. } => {
                            assert_eq!(expected_host, &canonical.proof);
                            ServerMessage::Response {
                                response: ServerResponse::Connected {
                                    proof: live.clone(),
                                    resumed: Vec::new(),
                                },
                            }
                        }
                        ClientRequest::ResolveUpgradeChat { source } => {
                            assert_eq!(source, &original);
                            assert_eq!(request.proof.as_ref(), Some(&live));
                            if case == 1 {
                                ServerMessage::Error(ipc::ProtocolError::new(
                                    ipc::ProtocolErrorCode::TerminalUnavailable,
                                    "No authenticated upgrade lineage",
                                    false,
                                ))
                            } else {
                                let mut resolved = canonical.clone();
                                if case == 2 {
                                    resolved.launch.recipe.arguments.push("--changed".into());
                                }
                                ServerMessage::Response {
                                    response: ServerResponse::UpgradeChatResolved {
                                        session: resolved,
                                    },
                                }
                            }
                        }
                        ClientRequest::ChatAttach { session } => {
                            assert_eq!(session, &canonical);
                            assert_eq!(request.proof.as_ref(), Some(&live));
                            let mut session = session.clone();
                            if case == 4 {
                                session.launch.recipe.arguments.push("--changed".into());
                            }
                            ServerMessage::Response {
                                response: ServerResponse::ChatAttached {
                                    session,
                                    snapshot: Snapshot {
                                        session_id: Some(
                                            if case == 5 {
                                                "other-thread"
                                            } else {
                                                CONVERSATION
                                            }
                                            .into(),
                                        ),
                                        ready: true,
                                        ..Default::default()
                                    },
                                },
                            }
                        }
                        other => panic!("strict upgrade must never issue {other:?}"),
                    };
                    requests.push(request.request);
                    codec
                        .write(
                            &mut server,
                            &ServerEnvelope {
                                version: PROTOCOL_VERSION,
                                request_id: Some(request.request_id),
                                proof: live.clone(),
                                message: response,
                            },
                        )
                        .unwrap();
                    if finished {
                        break;
                    }
                }
                requests
            });
            let result = ClientHandle::connect_on(
                client,
                source.clone(),
                Intent::Restore,
                None,
                false,
                None,
                None,
                Some(expected.clone()),
                |_| {},
            );
            let requests = worker.join().unwrap();
            let count = match case {
                6 => 0,
                3 => 1,
                1 | 2 => 2,
                _ => 3,
            };
            assert_eq!(requests.len(), count, "case {case}");
            if case == 0 || case == 7 {
                let handle = result.unwrap();
                assert_eq!(handle.session(), expected);
                assert_eq!(handle.upgrade_recovery(), Some(&(source, expected)));
            } else {
                assert!(result.is_err(), "case {case} must refuse");
            }
        }
    }

    #[test]
    fn retained_reset_candidates_reject_changed_configuration_before_attach() {
        let live = host_proof("live");
        let mut source = omp_card(Some(CONVERSATION), Some(host_proof("prior")));
        source.launch.provider = crate::agent::Provider::Claude;
        source.launch.recipe.program = "claude".into();
        source.native_continuity =
            super::super::NativeContinuity::RetainedClaudeActor { protocol: 2 };
        let mut resolved = source.clone();
        resolved.proof = Some(live.clone());
        resolved.launch.session_id = Some("22222222-2222-4222-8222-222222222222".into());
        for mutation in 0..9 {
            let mut changed = resolved.clone();
            match mutation {
                0 => {
                    changed
                        .launch
                        .recipe
                        .environment
                        .insert("CLAUDE_CONFIG_DIR".into(), "/other-account".into());
                }
                1 => changed.launch.recipe.working_directory = "/other-project".into(),
                2 => changed.launch.recipe.program = "other-provider".into(),
                3 => changed.launch.recipe.arguments.push("--other-model".into()),
                4 => changed.launch.provider = crate::agent::Provider::Omp,
                5 => {
                    changed.native_continuity =
                        super::super::NativeContinuity::RetainedClaudeActor { protocol: 1 }
                }
                6 => changed.launch.session_id = Some("invalid-uuid".into()),
                7 => changed.proof.as_mut().unwrap().boot_id = "other-boot".into(),
                _ => changed.id = TerminalId::new(),
            }
            let (outcome, _, candidate) = negotiate_with_recovery(
                &live,
                source.clone(),
                ServerMessage::Response {
                    response: ServerResponse::UpgradeChatResolved { session: changed },
                },
            );
            assert!(outcome.is_err(), "mutation {mutation}");
            assert!(
                candidate.is_none(),
                "invalid resolution cannot become a recovery candidate"
            );
        }
    }

    #[test]
    fn retained_reset_witness_requires_exact_attachment_and_snapshot_confirmation() {
        for mismatch in 0..3 {
            let mut source = omp_card(Some(CONVERSATION), Some(host_proof("prior")));
            source.launch.provider = crate::agent::Provider::Claude;
            source.launch.recipe.program = "claude".into();
            source.native_continuity =
                super::super::NativeContinuity::RetainedClaudeActor { protocol: 2 };
            let mut resolved = source.clone();
            resolved.proof = Some(host_proof("live"));
            resolved.launch.session_id = Some("22222222-2222-4222-8222-222222222222".into());
            assert!(!super::super::is_upgrade_recovery(&source, &resolved));
            assert!(!super::super::is_upgrade_identity_learning(
                &source, &resolved
            ));
            let result =
                restore_with_lineage(source.clone(), Some(resolved.clone()), move |request| {
                    let ClientRequest::ChatAttach { mut session } = request else {
                        panic!("resolved recovery must attach without starting");
                    };
                    let mut snapshot = Snapshot {
                        session_id: session.launch.session_id.clone(),
                        ..Default::default()
                    };
                    match mismatch {
                        1 => {
                            session
                                .launch
                                .recipe
                                .environment
                                .insert("CLAUDE_CONFIG_DIR".into(), "/other-account".into());
                        }
                        2 => snapshot.session_id = Some(CONVERSATION.into()),
                        _ => {}
                    }
                    ServerMessage::Response {
                        response: ServerResponse::ChatAttached { session, snapshot },
                    }
                });
            if mismatch == 0 {
                let handle = result.unwrap();
                let witness = handle.authenticated_upgrade_recovery().unwrap();
                assert_eq!(witness.source(), &source);
                assert_eq!(witness.replacement(), &resolved);
                assert!(matches!(
                    witness.kind,
                    UpgradeRecoveryKind::BoundRetainedReset
                ));
            } else {
                assert!(result.is_err(), "mismatch {mismatch}");
            }
        }
    }

    #[test]
    fn upgrade_provenance_requires_the_exact_attached_descriptor_and_confirmation() {
        for mismatch in 0..3 {
            let source = omp_card(Some(CONVERSATION), Some(host_proof("prior")));
            let mut resolved = source.clone();
            resolved.proof = Some(host_proof("live"));
            resolved.launch.session_id = Some(CONVERSATION.into());
            let result =
                restore_with_lineage(source.clone(), Some(resolved.clone()), move |request| {
                    let ClientRequest::ChatAttach { mut session } = request else {
                        panic!("expected exact attach")
                    };
                    let mut snapshot = Snapshot {
                        session_id: session.launch.session_id.clone(),
                        ..Default::default()
                    };
                    match mismatch {
                        1 => {
                            session
                                .launch
                                .recipe
                                .environment
                                .insert("ACCOUNT_HOME".into(), "/other".into());
                        }
                        2 => snapshot.session_id = Some("unconfirmed-other-thread".into()),
                        _ => {}
                    }
                    ServerMessage::Response {
                        response: ServerResponse::ChatAttached { session, snapshot },
                    }
                });
            if mismatch == 0 {
                assert_eq!(
                    result.unwrap().upgrade_recovery(),
                    Some(&(source, resolved))
                );
            } else {
                assert!(result.is_err(), "mismatch {mismatch}");
            }
        }
    }

    /// A card that cannot bring its conversation back has to show what refused
    /// it word for word, because the reader decides what to do next from that
    /// sentence and a generic failure tells them nothing.
    #[test]
    fn a_refused_resume_reports_the_daemons_own_sentence() {
        let failure = restore_against(
            omp_card(Some(CONVERSATION), Some(host_proof("before-the-update"))),
            |_| {
                ServerMessage::Error(crate::terminal_ipc::ProtocolError::new(
                    crate::terminal_ipc::ProtocolErrorCode::InvalidRequest,
                    "Provider executable omp is not executable in the preserved launch environment",
                    false,
                ))
            },
        )
        .err()
        .expect("the daemon refused this resume");
        assert_eq!(
            failure.message,
            "Provider executable omp is not executable in the preserved launch environment"
        );
        assert_eq!(
            failure.recovery,
            crate::chat::Recovery::Retry,
            "a refusal that can pass leaves the card a retry that runs the same resume"
        );
    }

    /// A conversation the provider has not named yet - one still loading, or
    /// one whose resume is about to fail - must not overwrite the identifier
    /// the card needs for its next attempt, or one failed restore would turn a
    /// recoverable conversation into a permanently blank card.
    #[test]
    fn a_resume_that_names_no_conversation_yet_keeps_the_saved_one() {
        let handle = restore_against(
            omp_card(Some(CONVERSATION), Some(host_proof("before-the-update"))),
            |request| {
                let ClientRequest::ChatStart { session, .. } = request else {
                    panic!("a saved conversation is resumed by starting the provider on it");
                };
                ServerMessage::Response {
                    response: ServerResponse::ChatAttached {
                        session,
                        snapshot: Snapshot::default(),
                    },
                }
            },
        )
        .expect("the daemon accepted this resume");
        assert_eq!(
            handle.session().launch.session_id.as_deref(),
            Some(CONVERSATION)
        );
    }
    #[test]
    fn hidden_wire_client_releases_history_and_reveals_at_the_same_generation() {
        let (client, mut server) = UnixStream::pair().unwrap();
        let (hidden_tx, hidden_rx) = std::sync::mpsc::channel();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let worker = thread::spawn(move || {
            server
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            server
                .set_write_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let proof = host_proof("live");
            let mut codec = FrameCodec::new(ipc::MAX_FRAME_BYTES);
            let respond = |codec: &mut FrameCodec, server: &mut UnixStream, request_id, message| {
                codec
                    .write(
                        server,
                        &ServerEnvelope {
                            version: PROTOCOL_VERSION,
                            request_id,
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
                Some(connect.request_id),
                ServerMessage::Response {
                    response: ServerResponse::Connected {
                        proof: proof.clone(),
                        resumed: vec![],
                    },
                },
            );
            let start: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
            let ClientRequest::ChatStart { session, .. } = start.request else {
                panic!("chat start expected");
            };
            let chat_id = session.id;
            let snapshot = || Snapshot {
                generation: 41,
                session_id: Some("exact-session".into()),
                ready: true,
                items: vec![Arc::new(super::super::Item {
                    id: "history".into(),
                    role: super::super::Role::Assistant,
                    content: super::super::ItemContent::Text {
                        text: "body".repeat(64 * 1024),
                        thinking: false,
                        phase: Default::default(),
                    },
                    started_at: None,
                    at: None,
                })],
                ..Default::default()
            };
            respond(
                &mut codec,
                &mut server,
                Some(start.request_id),
                ServerMessage::Response {
                    response: ServerResponse::ChatAttached {
                        session,
                        snapshot: snapshot(),
                    },
                },
            );
            let hidden: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
            assert!(matches!(
                hidden.request,
                ClientRequest::ChatVisibility { visible: false, .. }
            ));
            respond(
                &mut codec,
                &mut server,
                Some(hidden.request_id),
                ServerMessage::Response {
                    response: ServerResponse::Accepted,
                },
            );
            hidden_tx.send(()).unwrap();
            let shown: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
            assert!(matches!(
                shown.request,
                ClientRequest::ChatVisibility { visible: true, .. }
            ));
            respond(
                &mut codec,
                &mut server,
                Some(shown.request_id),
                ServerMessage::Response {
                    response: ServerResponse::Accepted,
                },
            );
            respond(
                &mut codec,
                &mut server,
                None,
                ServerMessage::ChatChanged { chat_id },
            );
            let read: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
            assert!(
                matches!(
                    read.request,
                    ClientRequest::ChatRead {
                        after_generation: None,
                        ..
                    }
                ),
                "the current generation is metadata only and cannot suppress hydration"
            );
            respond(
                &mut codec,
                &mut server,
                Some(read.request_id),
                ServerMessage::Response {
                    response: ServerResponse::ChatSnapshot {
                        chat_id,
                        snapshot: Some(snapshot()),
                    },
                },
            );
            done_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        });
        let (wake_tx, wake_rx) = std::sync::mpsc::channel();
        let handle = ClientHandle::connect_on(
            client,
            omp_card(None, None),
            Intent::Start,
            None,
            false,
            None,
            None,
            None,
            move |wake| {
                let _ = wake_tx.send(wake);
            },
        )
        .unwrap();
        let body = Arc::downgrade(&handle.snapshot().items[0]);
        let installed = handle.inner.publication.current.load_full();
        let (released, retirement) = std::sync::mpsc::channel();
        *installed.retirement_complete.lock().unwrap() = Some(released);
        drop(installed);
        handle.set_visible(false).unwrap();
        hidden_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        // The actual wire hide edge no longer implies that the independent
        // local disposal owner has completed. Wait for that exact payload,
        // then retain the same real Weak-body release/residency oracles.
        retirement.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(handle.snapshot().generation, 41);
        assert_eq!(
            handle.snapshot().session_id.as_deref(),
            Some("exact-session")
        );
        assert!(body.upgrade().is_none());
        assert!(handle.resident_snapshot_with_clock().is_none());
        handle.set_visible(true).unwrap();
        while handle.resident_snapshot_with_clock().is_none() {
            wake_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        }
        assert_eq!(handle.snapshot().generation, 41);
        assert_eq!(handle.snapshot().transcript_bytes(), 256 * 1024);
        done_tx.send(()).unwrap();
        worker.join().unwrap();
        while !matches!(
            wake_rx.recv_timeout(Duration::from_secs(5)).unwrap(),
            Wake::Disconnected(_)
        ) {}
        assert!(handle.set_visible(false).is_err());
        assert_eq!(
            handle.snapshot().transcript_bytes(),
            256 * 1024,
            "an unreachable daemon cannot rehydrate the last readable publication yet"
        );
    }

    #[test]
    fn hidden_control_subscribers_advance_metadata_without_rehydration_loops() {
        use super::super::{
            Item, ItemContent, Role,
            presentation::{ItemChange, Presentation},
        };
        let (client, mut server) = UnixStream::pair().unwrap();
        let (advance_tx, advance_rx) = std::sync::mpsc::channel();
        let (stage_tx, stage_rx) = std::sync::mpsc::channel();
        let worker = thread::spawn(move || {
            server
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            server
                .set_write_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let proof = host_proof("control-reader");
            let mut codec = FrameCodec::new(ipc::MAX_FRAME_BYTES);
            let respond = |codec: &mut FrameCodec, server: &mut UnixStream, request_id, message| {
                codec
                    .write(
                        server,
                        &ServerEnvelope {
                            version: PROTOCOL_VERSION,
                            request_id,
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
                Some(connect.request_id),
                ServerMessage::Response {
                    response: ServerResponse::Connected {
                        proof: proof.clone(),
                        resumed: vec![],
                    },
                },
            );
            let start: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
            let ClientRequest::ChatStart { session, .. } = start.request else {
                panic!("chat start expected");
            };
            let chat_id = session.id;
            respond(
                &mut codec,
                &mut server,
                Some(start.request_id),
                ServerMessage::Response {
                    response: ServerResponse::ChatAttached {
                        session,
                        snapshot: Snapshot {
                            generation: 1,
                            ..Default::default()
                        },
                    },
                },
            );
            let interested: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
            assert!(matches!(
                interested.request,
                ClientRequest::ChatVisibility { visible: true, .. }
            ));
            respond(
                &mut codec,
                &mut server,
                Some(interested.request_id),
                ServerMessage::Response {
                    response: ServerResponse::Accepted,
                },
            );
            respond(
                &mut codec,
                &mut server,
                None,
                ServerMessage::ChatChanged { chat_id },
            );
            let read: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
            assert!(matches!(
                read.request,
                ClientRequest::ChatRead {
                    after_generation: Some(1),
                    ..
                }
            ));
            let prompt = Item {
                id: "received-prompt".into(),
                role: Role::User,
                content: ItemContent::Text {
                    text: "prompt".repeat(64 * 1024),
                    thinking: false,
                    phase: Default::default(),
                },
                started_at: None,
                at: None,
            };
            respond(
                &mut codec,
                &mut server,
                Some(read.request_id),
                ServerMessage::Response {
                    response: ServerResponse::ChatUpdate {
                        chat_id,
                        update: Presentation {
                            base_generation: 99,
                            file_changes_unchanged: true,
                            metadata: Snapshot {
                                generation: 100,
                                ready: true,
                                ..Default::default()
                            },
                            order: vec![],
                            changes: vec![],
                            children: vec![],
                        },
                    },
                },
            );
            let resync: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
            assert!(
                matches!(
                    resync.request,
                    ClientRequest::ChatRead {
                        after_generation: None,
                        ..
                    }
                ),
                "hidden controls must reject an update with a mismatched metadata base"
            );
            respond(
                &mut codec,
                &mut server,
                Some(resync.request_id),
                ServerMessage::Response {
                    response: ServerResponse::ChatSnapshot {
                        chat_id,
                        snapshot: Some(Snapshot {
                            generation: 1,
                            ..Default::default()
                        }),
                    },
                },
            );
            respond(
                &mut codec,
                &mut server,
                None,
                ServerMessage::ChatChanged { chat_id },
            );
            let read: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
            assert!(matches!(
                read.request,
                ClientRequest::ChatRead {
                    after_generation: Some(1),
                    ..
                }
            ));
            respond(
                &mut codec,
                &mut server,
                Some(read.request_id),
                ServerMessage::Response {
                    response: ServerResponse::ChatUpdate {
                        chat_id,
                        update: Presentation {
                            base_generation: 1,
                            file_changes_unchanged: true,
                            metadata: Snapshot {
                                generation: 2,
                                ready: true,
                                ..Default::default()
                            },
                            order: vec![prompt.id.clone()],
                            changes: vec![ItemChange::Replace(prompt.clone())],
                            children: vec![],
                        },
                    },
                },
            );
            let command: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
            assert!(matches!(&command.request, ClientRequest::ChatCommand {
                command: Command::Prompt { text, .. }, ..
            } if text.len() > 1024 * 1024));
            respond(
                &mut codec,
                &mut server,
                Some(command.request_id),
                ServerMessage::Response {
                    response: ServerResponse::Accepted,
                },
            );
            advance_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            respond(
                &mut codec,
                &mut server,
                None,
                ServerMessage::ChatChanged { chat_id },
            );
            let unchanged: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
            assert!(
                matches!(
                    unchanged.request,
                    ClientRequest::ChatRead {
                        after_generation: Some(2),
                        ..
                    }
                ),
                "discarded bodies must not cause hidden control resync loops"
            );
            respond(
                &mut codec,
                &mut server,
                Some(unchanged.request_id),
                ServerMessage::Response {
                    response: ServerResponse::ChatSnapshot {
                        chat_id,
                        snapshot: None,
                    },
                },
            );
            // This ordered control edge lets the client prove it consumed the
            // unchanged response while still hidden before requesting reveal.
            respond(
                &mut codec,
                &mut server,
                None,
                ServerMessage::ChatActivity {
                    chat_id,
                    state: ActivityState::Working,
                    generation: 2,
                },
            );
            stage_tx.send(()).unwrap();
            // The next frame must be reveal, not another automatic full read.
            let shown: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
            assert!(matches!(
                shown.request,
                ClientRequest::ChatVisibility { visible: true, .. }
            ));
            respond(
                &mut codec,
                &mut server,
                Some(shown.request_id),
                ServerMessage::Response {
                    response: ServerResponse::Accepted,
                },
            );
            respond(
                &mut codec,
                &mut server,
                None,
                ServerMessage::ChatChanged { chat_id },
            );
            let read: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
            assert!(matches!(
                read.request,
                ClientRequest::ChatRead {
                    after_generation: None,
                    ..
                }
            ));
            respond(
                &mut codec,
                &mut server,
                Some(read.request_id),
                ServerMessage::Response {
                    response: ServerResponse::ChatSnapshot {
                        chat_id,
                        snapshot: Some(Snapshot {
                            generation: 2,
                            ready: true,
                            items: vec![Arc::new(prompt)],
                            ..Default::default()
                        }),
                    },
                },
            );
            advance_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        });
        let (wake_tx, wake_rx) = std::sync::mpsc::channel();
        let handle = ClientHandle::connect_on(
            client,
            omp_card(None, None),
            Intent::Start,
            None,
            false,
            None,
            None,
            None,
            move |wake| {
                let _ = wake_tx.send(wake);
            },
        )
        .unwrap();
        {
            let mut wire = handle.inner.wire.lock().unwrap();
            wire.codec
                .write(&mut std::io::sink(), &"large prompt".repeat(128 * 1024))
                .unwrap();
            assert!(wire.codec.buffer_capacities_for_test().1 > 1024 * 1024);
        }
        handle.set_interest(true, false).unwrap();
        while handle.snapshot().generation != 2 {
            wake_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        while handle
            .inner
            .wire
            .lock()
            .unwrap()
            .codec
            .buffer_capacities_for_test()
            .1
            > 256 * 1024
        {
            assert!(
                Instant::now() < deadline,
                "hidden interested transport retained its large prompt buffer"
            );
            thread::sleep(Duration::from_millis(1));
        }
        assert!(handle.snapshot().ready);
        assert_eq!(handle.snapshot().transcript_bytes(), 0);
        assert!(handle.has_observed_user_item());
        assert!(handle.resident_snapshot_with_clock().is_none());
        handle
            .send(Command::Prompt {
                text: "large prompt".repeat(128 * 1024),
                attachments: vec![],
                mode: super::super::PromptMode::Send,
            })
            .unwrap();
        assert!(
            handle
                .inner
                .wire
                .lock()
                .unwrap()
                .codec
                .buffer_capacities_for_test()
                .1
                <= 256 * 1024,
            "new large hidden commands release scratch without another visibility transition"
        );
        advance_tx.send(()).unwrap();
        stage_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        while !matches!(
            wake_rx.recv_timeout(Duration::from_secs(5)).unwrap(),
            Wake::Activity
        ) {}
        handle.set_visible(true).unwrap();
        while handle.resident_snapshot_with_clock().is_none() {
            wake_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        }
        assert_eq!(handle.snapshot().generation, 2);
        assert_eq!(handle.snapshot().transcript_bytes(), 6 * 64 * 1024);
        advance_tx.send(()).unwrap();
        worker.join().unwrap();
    }

    fn paused_wire_publication(visibility: Option<bool>) {
        use crate::chat::{
            Item, ItemContent, Role,
            presentation::{ItemChange, Presentation},
        };
        let text = |id: &str, body: &str| Item {
            id: id.into(),
            role: Role::Assistant,
            started_at: None,
            at: None,
            content: ItemContent::Text {
                text: body.into(),
                thinking: false,
                phase: Default::default(),
            },
        };
        let initial = Snapshot {
            generation: 1,
            sampled_at: Some(1_000),
            items: vec![Arc::new(text("live", "hello"))],
            ..Default::default()
        };
        let update = Presentation {
            file_changes_unchanged: false,
            base_generation: 1,
            metadata: Snapshot {
                sampled_at: Some(50_000),
                generation: 2,
                ..Default::default()
            },
            order: vec!["live".into()],
            changes: vec![ItemChange::between(
                Some(&initial.items[0]),
                &text("live", "hello tail"),
            )],
            children: vec![],
        };
        let card = omp_card(None, None);
        let (paused_tx, paused_rx) = std::sync::mpsc::channel();
        let (resume_tx, resume_rx) = std::sync::mpsc::channel();
        PUBLICATION_GATES
            .lock()
            .unwrap()
            .insert(card.id, (paused_tx, resume_rx));
        let (client, mut server) = UnixStream::pair().unwrap();
        let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
        let (resync_tx, resync_rx) = std::sync::mpsc::channel();
        let (hydrate_tx, hydrate_rx) = std::sync::mpsc::channel();
        let worker = thread::spawn(move || {
            server
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let proof = host_proof("live");
            let mut codec = FrameCodec::new(ipc::MAX_FRAME_BYTES);
            let respond = |codec: &mut FrameCodec, server: &mut UnixStream, request_id, message| {
                codec
                    .write(
                        server,
                        &ServerEnvelope {
                            version: PROTOCOL_VERSION,
                            request_id,
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
                Some(connect.request_id),
                ServerMessage::Response {
                    response: ServerResponse::Connected {
                        proof: proof.clone(),
                        resumed: vec![],
                    },
                },
            );
            let start: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
            let ClientRequest::ChatStart { session, .. } = start.request else {
                panic!("expected new chat")
            };
            let chat_id = session.id;
            respond(
                &mut codec,
                &mut server,
                Some(start.request_id),
                ServerMessage::Response {
                    response: ServerResponse::ChatAttached {
                        session,
                        snapshot: initial,
                    },
                },
            );
            let visible: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
            assert!(matches!(
                visible.request,
                ClientRequest::ChatVisibility { visible: true, .. }
            ));
            respond(
                &mut codec,
                &mut server,
                None,
                ServerMessage::Response {
                    response: ServerResponse::ChatUpdate { chat_id, update },
                },
            );
            if visibility == Some(true) {
                loop {
                    let request: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
                    match request.request {
                        ClientRequest::ChatVisibility { .. } => {}
                        ClientRequest::ChatRead {
                            after_generation, ..
                        } => {
                            assert_eq!(
                                after_generation, None,
                                "a hide/reveal collision requires unconditional body hydration"
                            );
                            resync_tx.send(()).unwrap();
                            hydrate_rx.recv_timeout(Duration::from_secs(5)).unwrap();
                            respond(
                                &mut codec,
                                &mut server,
                                Some(request.request_id),
                                ServerMessage::Response {
                                    response: ServerResponse::ChatSnapshot {
                                        chat_id,
                                        snapshot: Some(Snapshot {
                                            generation: 2,
                                            sampled_at: Some(50_000),
                                            items: vec![Arc::new(text("live", "hello tail"))],
                                            ..Default::default()
                                        }),
                                    },
                                },
                            );
                            break;
                        }
                        other => panic!("unexpected request during body resync: {other:?}"),
                    }
                }
            }
            done_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        });
        let (wake_tx, wake_rx) = std::sync::mpsc::channel();
        let handle = ClientHandle::connect_on(
            client,
            card,
            Intent::Start,
            None,
            false,
            None,
            None,
            None,
            move |wake| {
                let _ = wake_tx.send(wake);
            },
        )
        .unwrap();
        let installed = handle.snapshot_with_clock();
        handle.set_visible(true).unwrap();
        paused_rx.recv_timeout(Duration::from_secs(5)).unwrap();

        // The reader now holds a derived publication it has not installed.
        let (read_tx, read_rx) = std::sync::mpsc::channel();
        let ui = handle.clone();
        let reading = thread::spawn(move || {
            let (snapshot, clock) = ui.snapshot_with_clock();
            let session = ui.session();
            let resident = ui.resident_snapshot_with_clock().unwrap();
            assert!(!ui.known_append(&snapshot.items[0], &snapshot.items[0]));
            let _ = read_tx.send((snapshot, clock, session, ui.snapshot(), resident));
        });
        let (snapshot, clock, session, plain, resident) = read_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("presentation reads must not wait for the connection reader");
        reading.join().unwrap();
        assert!(Arc::ptr_eq(&snapshot, &installed.0));
        assert!(Arc::ptr_eq(&plain, &installed.0));
        assert_eq!(snapshot.generation, 1);
        assert_eq!(clock.unwrap().sampled_at, 1_000);
        assert_eq!(session.id, handle.inner.id);
        assert!(Arc::ptr_eq(&resident.0, &installed.0));
        assert_eq!(resident.1.unwrap().sampled_at, 1_000);
        if let Some(reveal) = visibility {
            // Hide must finish while the source reader is still paused.
            let (hidden_tx, hidden_rx) = std::sync::mpsc::channel();
            let hidden = handle.clone();
            let hiding = thread::spawn(move || {
                hidden.set_visible(false).unwrap();
                if reveal {
                    hidden.set_visible(true).unwrap();
                }
                hidden_tx.send(()).unwrap();
            });
            hidden_rx
                .recv_timeout(Duration::from_secs(5))
                .expect("hide must not wait for body derivation");
            hiding.join().unwrap();
            assert_eq!(handle.snapshot().transcript_bytes(), 0);
            assert_eq!(handle.snapshot_with_clock().1.unwrap().sampled_at, 1_000);
            assert!(handle.resident_snapshot_with_clock().is_none());
        }
        resume_tx.send(()).unwrap();
        let until = Instant::now() + Duration::from_secs(5);
        while handle.snapshot().generation != 2 {
            assert!(
                Instant::now() < until,
                "derived publication was not committed"
            );
            thread::sleep(Duration::from_millis(1));
        }
        let mut metadata_clock = None;
        if visibility.is_some() {
            let state = handle.inner.publication.current.load_full();
            assert!(
                !state.resident,
                "a pre-hide derived body must not be installed after hide or hide/reveal"
            );
            assert_eq!(state.snapshot.transcript_bytes(), 0);
            assert!(
                state.lineage.is_empty(),
                "hidden metadata cannot authorize an append across eviction"
            );
            assert_eq!(state.clock.unwrap().sampled_at, 50_000);
            metadata_clock = state.clock;
            if visibility == Some(true) {
                resync_rx.recv_timeout(Duration::from_secs(5)).unwrap();
                assert!(handle.resident_snapshot_with_clock().is_none());
                hydrate_tx.send(()).unwrap();
                while handle.resident_snapshot_with_clock().is_none() {
                    wake_rx.recv_timeout(Duration::from_secs(5)).unwrap();
                }
            }
        }
        let (published, clock) = handle.snapshot_with_clock();
        if visibility != Some(false) {
            assert!(
                matches!(&published.items[0].content, ItemContent::Text { text, .. } if text == "hello tail")
            );
            assert_eq!(
                handle.known_append(&installed.0.items[0], &published.items[0]),
                visibility.is_none()
            );
        }
        assert_eq!(published.sampled_at, Some(50_000));
        let clock = clock.unwrap();
        if visibility == Some(true) {
            let metadata_clock = metadata_clock.unwrap();
            assert!(clock.received_at >= metadata_clock.received_at);
            assert!(clock.at(clock.received_at) >= metadata_clock.at(clock.received_at));
        } else {
            assert_eq!(clock.sampled_at, 50_000);
        }
        done_tx.send(()).unwrap();
        worker.join().unwrap();
    }
    #[test]
    fn readers_keep_the_installed_publication_while_the_next_is_built() {
        paused_wire_publication(None);
    }

    #[test]
    fn hide_while_body_is_derived_commits_only_metadata() {
        paused_wire_publication(Some(false));
    }

    #[test]
    fn hide_then_reveal_while_body_is_derived_requires_fresh_hydration() {
        paused_wire_publication(Some(true));
    }

    #[test]
    fn second_hide_reveal_fences_full_hydration_derived_from_metadata() {
        use crate::chat::{Item, ItemContent, Role};
        let text = |body: &str| {
            Arc::new(Item {
                id: "live".into(),
                role: Role::Assistant,
                started_at: None,
                at: None,
                content: ItemContent::Text {
                    text: body.into(),
                    thinking: false,
                    phase: Default::default(),
                },
            })
        };
        let card = omp_card(None, None);
        let (client, mut server) = UnixStream::pair().unwrap();
        let (first_read_tx, first_read_rx) = std::sync::mpsc::channel();
        let (send_full_tx, send_full_rx) = std::sync::mpsc::channel();
        let (fresh_read_tx, fresh_read_rx) = std::sync::mpsc::channel();
        let (hydrate_tx, hydrate_rx) = std::sync::mpsc::channel();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let worker = thread::spawn(move || {
            server
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let proof = host_proof("live");
            let mut codec = FrameCodec::new(ipc::MAX_FRAME_BYTES);
            let respond = |codec: &mut FrameCodec, server: &mut UnixStream, request_id, message| {
                codec
                    .write(
                        server,
                        &ServerEnvelope {
                            version: PROTOCOL_VERSION,
                            request_id,
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
                Some(connect.request_id),
                ServerMessage::Response {
                    response: ServerResponse::Connected {
                        proof: proof.clone(),
                        resumed: vec![],
                    },
                },
            );
            let start: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
            let ClientRequest::ChatStart { session, .. } = start.request else {
                panic!("expected start")
            };
            let chat_id = session.id;
            respond(
                &mut codec,
                &mut server,
                Some(start.request_id),
                ServerMessage::Response {
                    response: ServerResponse::ChatAttached {
                        session,
                        snapshot: Snapshot {
                            generation: 1,
                            sampled_at: Some(1_000),
                            items: vec![text("initial")],
                            ..Default::default()
                        },
                    },
                },
            );
            // The first reveal requests a full body from nonresident metadata.
            loop {
                let request: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
                match request.request {
                    ClientRequest::ChatVisibility { visible: true, .. } => {
                        respond(
                            &mut codec,
                            &mut server,
                            None,
                            ServerMessage::ChatChanged { chat_id },
                        );
                    }
                    ClientRequest::ChatVisibility { visible: false, .. } => {}
                    ClientRequest::ChatRead {
                        after_generation, ..
                    } => {
                        assert_eq!(after_generation, None);
                        first_read_tx.send(()).unwrap();
                        send_full_rx.recv_timeout(Duration::from_secs(5)).unwrap();
                        respond(
                            &mut codec,
                            &mut server,
                            Some(request.request_id),
                            ServerMessage::Response {
                                response: ServerResponse::ChatSnapshot {
                                    chat_id,
                                    snapshot: Some(Snapshot {
                                        generation: 2,
                                        sampled_at: Some(50_000),
                                        items: vec![text("stale pre-hide body")],
                                        ..Default::default()
                                    }),
                                },
                            },
                        );
                        break;
                    }
                    other => panic!("unexpected first hydration request: {other:?}"),
                }
            }
            loop {
                let request: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
                match request.request {
                    ClientRequest::ChatVisibility { .. } => {}
                    ClientRequest::ChatRead {
                        after_generation, ..
                    } => {
                        assert_eq!(
                            after_generation, None,
                            "second hide/show must invalidate the full proposal even when metadata identity is unchanged"
                        );
                        fresh_read_tx.send(()).unwrap();
                        hydrate_rx.recv_timeout(Duration::from_secs(5)).unwrap();
                        respond(
                            &mut codec,
                            &mut server,
                            Some(request.request_id),
                            ServerMessage::Response {
                                response: ServerResponse::ChatSnapshot {
                                    chat_id,
                                    snapshot: Some(Snapshot {
                                        generation: 2,
                                        sampled_at: Some(60_000),
                                        items: vec![text("fresh authorized body")],
                                        ..Default::default()
                                    }),
                                },
                            },
                        );
                        break;
                    }
                    other => panic!("unexpected fresh hydration request: {other:?}"),
                }
            }
            done_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        });
        let (wake_tx, wake_rx) = std::sync::mpsc::channel();
        let handle = ClientHandle::connect_on(
            client,
            card,
            Intent::Start,
            None,
            false,
            None,
            None,
            None,
            move |wake| {
                let _ = wake_tx.send(wake);
            },
        )
        .unwrap();
        let old = handle.snapshot();
        handle.set_visible(false).unwrap();
        assert!(handle.resident_snapshot_with_clock().is_none());
        handle.set_visible(true).unwrap();
        first_read_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let (paused_tx, paused_rx) = std::sync::mpsc::channel();
        let (resume_tx, resume_rx) = std::sync::mpsc::channel();
        REPLACEMENT_GATES.lock().unwrap().insert(
            Arc::as_ptr(&handle.inner.publication) as usize,
            (paused_tx, resume_rx),
        );
        send_full_tx.send(()).unwrap();
        paused_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let metadata = handle.inner.publication.current.load_full();
        assert!(!metadata.resident && metadata.hidden.is_none());
        // This is the ABA case: second hide/show changes only body policy,
        // retaining the identical nonresident Publication pointer.
        let (finished_tx, finished_rx) = std::sync::mpsc::channel();
        let hidden = handle.clone();
        let hiding = thread::spawn(move || {
            hidden.set_visible(false).unwrap();
            hidden.set_visible(true).unwrap();
            finished_tx.send(()).unwrap();
        });
        finished_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("visibility must not wait for full hydration derivation");
        hiding.join().unwrap();
        assert!(Arc::ptr_eq(
            &metadata,
            &handle.inner.publication.current.load_full()
        ));
        assert!(handle.resident_snapshot_with_clock().is_none());
        assert_eq!(handle.snapshot_with_clock().1.unwrap().sampled_at, 1_000);
        resume_tx.send(()).unwrap();
        let until = Instant::now() + Duration::from_secs(5);
        while handle.snapshot().generation != 2 {
            assert!(
                Instant::now() < until,
                "full response proposal did not commit"
            );
            thread::sleep(Duration::from_millis(1));
        }
        let fallback = handle.inner.publication.current.load_full();
        assert!(
            !fallback.resident,
            "a second hide/show must reject the pre-hide full hydration proposal"
        );
        assert_eq!(fallback.snapshot.generation, 2);
        assert_eq!(fallback.clock.unwrap().sampled_at, 50_000);
        assert_eq!(fallback.snapshot.transcript_bytes(), 0);
        assert!(fallback.lineage.is_empty());
        fresh_read_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(
            handle.resident_snapshot_with_clock().is_none(),
            "pre-hide full response was never admitted"
        );
        hydrate_tx.send(()).unwrap();
        while handle.resident_snapshot_with_clock().is_none() {
            wake_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        }
        let (snapshot, clock) = handle.snapshot_with_clock();
        assert_eq!(snapshot.generation, 2);
        assert_eq!(clock.unwrap().sampled_at, 60_000);
        assert!(
            matches!(&snapshot.items[0].content, ItemContent::Text { text, .. } if text == "fresh authorized body")
        );
        assert!(!handle.known_append(&old.items[0], &snapshot.items[0]));
        done_tx.send(()).unwrap();
        worker.join().unwrap();
    }

    #[test]
    fn wire_updates_share_retained_items_and_request_full_resync_on_invalid_patch() {
        use crate::chat::{
            Item, ItemContent, Role,
            presentation::{ItemChange, Presentation},
        };
        let text = |id: &str, body: &str| Item {
            id: id.into(),
            role: Role::Assistant,
            started_at: None,
            at: None,
            content: ItemContent::Text {
                text: body.into(),
                thinking: false,
                phase: Default::default(),
            },
        };
        let initial = Snapshot {
            sampled_at: Some(1000),
            generation: 1,
            items: vec![
                Arc::new(text("fixed", &"x".repeat(128 * 1024))),
                Arc::new(text("live", "hello")),
            ],
            ..Default::default()
        };
        let update = Presentation {
            file_changes_unchanged: false,
            base_generation: 1,
            metadata: Snapshot {
                sampled_at: Some(2000),
                generation: 2,
                ..Default::default()
            },
            order: vec!["fixed".into(), "live".into()],
            changes: vec![ItemChange::between(
                Some(&initial.items[1]),
                &text("live", "hello tail"),
            )],
            children: vec![],
        };
        let full = Snapshot {
            generation: 4,
            items: vec![Arc::new(text("live", "resynchronized"))],
            ..Default::default()
        };
        let (client, mut server) = UnixStream::pair().unwrap();
        let (advance_tx, advance_rx) = std::sync::mpsc::channel();
        let (resync_tx, resync_rx) = std::sync::mpsc::channel();
        let worker = thread::spawn(move || {
            server
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            server
                .set_write_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let proof = host_proof("live");
            let mut codec = FrameCodec::new(ipc::MAX_FRAME_BYTES);
            let respond = |codec: &mut FrameCodec, server: &mut UnixStream, request_id, message| {
                codec
                    .write(
                        server,
                        &ServerEnvelope {
                            version: PROTOCOL_VERSION,
                            request_id,
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
                Some(connect.request_id),
                ServerMessage::Response {
                    response: ServerResponse::Connected {
                        proof: proof.clone(),
                        resumed: vec![],
                    },
                },
            );
            let start: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
            let ClientRequest::ChatStart { session, .. } = start.request else {
                panic!("expected new chat")
            };
            let chat_id = session.id;
            respond(
                &mut codec,
                &mut server,
                Some(start.request_id),
                ServerMessage::Response {
                    response: ServerResponse::ChatAttached {
                        session,
                        snapshot: initial,
                    },
                },
            );
            let visible: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
            assert!(matches!(
                visible.request,
                ClientRequest::ChatVisibility { visible: true, .. }
            ));
            respond(
                &mut codec,
                &mut server,
                None,
                ServerMessage::Response {
                    response: ServerResponse::ChatUpdate { chat_id, update },
                },
            );
            advance_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            let malformed = Presentation {
                file_changes_unchanged: false,
                base_generation: 2,
                metadata: Snapshot {
                    sampled_at: Some(999_999),
                    generation: 3,
                    subagents: vec![super::super::Subagent {
                        model: None,
                        waiting: false,
                        detail: String::new(),
                        id: "missing-child-patch".into(),
                        kind: "review".into(),
                        description: String::new(),
                        status: super::super::ToolStatus::Running,
                        background: false,
                        items: vec![],
                    }],
                    ..Default::default()
                },
                order: vec!["fixed".into(), "live".into()],
                changes: vec![ItemChange::between(
                    Some(&text("live", "hello tail")),
                    &text("live", "hello tail corrupt"),
                )],
                // Parent work is valid, then child validation rejects the
                // entire publication; no partial parent or clock may escape.
                children: vec![],
            };
            respond(
                &mut codec,
                &mut server,
                None,
                ServerMessage::Response {
                    response: ServerResponse::ChatUpdate {
                        chat_id,
                        update: malformed,
                    },
                },
            );
            let resync: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
            assert!(
                matches!(
                    resync.request,
                    ClientRequest::ChatRead {
                        after_generation: None,
                        ..
                    }
                ),
                "invalid patch must request an unconditional snapshot"
            );
            resync_tx.send(()).unwrap();
            advance_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            respond(
                &mut codec,
                &mut server,
                Some(resync.request_id),
                ServerMessage::Response {
                    response: ServerResponse::ChatSnapshot {
                        chat_id,
                        snapshot: Some(full),
                    },
                },
            );
            advance_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        });
        let (wake_tx, wake_rx) = std::sync::mpsc::channel();
        let handle = ClientHandle::connect_on(
            client,
            omp_card(None, None),
            Intent::Start,
            None,
            false,
            None,
            None,
            None,
            move |wake| {
                let _ = wake_tx.send(wake);
            },
        )
        .unwrap();
        let retained = handle.snapshot();
        handle.set_visible(true).unwrap();
        while handle.snapshot().generation != 2 {
            wake_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        }
        let updated = handle.snapshot();
        let clock = handle.snapshot_with_clock().1.unwrap();
        assert!(Arc::ptr_eq(&retained.items[0], &updated.items[0]));
        assert!(
            matches!(&retained.items[1].content, ItemContent::Text { text, .. } if text == "hello")
        );
        assert!(
            matches!(&updated.items[1].content, ItemContent::Text { text, .. } if text == "hello tail")
        );
        advance_tx.send(()).unwrap();
        resync_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let (unchanged, unchanged_clock) = handle.snapshot_with_clock();
        assert!(
            Arc::ptr_eq(&updated, &unchanged),
            "rejected update must preserve installed publication identity"
        );
        assert_eq!(unchanged.generation, 2);
        assert_eq!(unchanged_clock.unwrap().sampled_at, clock.sampled_at);
        assert_eq!(unchanged_clock.unwrap().received_at, clock.received_at);
        advance_tx.send(()).unwrap();
        while handle.snapshot().generation != 4 {
            wake_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        }
        assert!(
            matches!(&handle.snapshot().items[0].content, ItemContent::Text { text, .. } if text == "resynchronized")
        );
        advance_tx.send(()).unwrap();
        worker.join().unwrap();
    }

    fn text_copy_source(token: u64, bytes: u64) -> super::super::TextCopySource {
        super::super::TextCopySource {
            cache: uuid::Uuid::new_v4(),
            scope: super::super::TextCopyScope::Subagent { id: "child".into() },
            token,
            revision: 7,
            bytes,
        }
    }

    #[test]
    fn text_copy_pages_require_exact_identity_contiguous_bytes_and_completion() {
        let source = text_copy_source(1, 6);
        let first = super::super::TextCopyPage {
            source: source.clone(),
            offset: 0,
            text: "猫".into(),
            done: false,
        };
        let mut text = String::new();
        append_text_copy_page(&source, &mut text, first.clone()).unwrap();
        assert_eq!(text, "猫");
        let last = super::super::TextCopyPage {
            source: source.clone(),
            offset: 3,
            text: "犬".into(),
            done: true,
        };
        let mut bad_pages = vec![first]; // Duplicate offset.
        let mut changed = last.clone();
        changed.offset = 4;
        bad_pages.push(changed);
        let mut changed = last.clone();
        changed.source.cache = uuid::Uuid::new_v4();
        bad_pages.push(changed);
        let mut changed = last.clone();
        changed.source.scope = super::super::TextCopyScope::Conversation;
        bad_pages.push(changed);
        let mut changed = last.clone();
        changed.source.token += 1;
        bad_pages.push(changed);
        let mut changed = last.clone();
        changed.source.revision += 1;
        bad_pages.push(changed);
        let mut changed = last.clone();
        changed.source.bytes += 1;
        bad_pages.push(changed);
        let mut changed = last.clone();
        changed.done = false;
        bad_pages.push(changed);
        let mut changed = last.clone();
        changed.text.clear();
        changed.done = false;
        bad_pages.push(changed);
        let mut changed = last.clone();
        changed.text = "longer".into();
        bad_pages.push(changed);
        let mut changed = last.clone();
        changed.text = "x".into();
        bad_pages.push(changed);
        for page in bad_pages {
            assert!(append_text_copy_page(&source, &mut text, page).is_err());
            assert_eq!(text, "猫", "invalid pages must not publish partial data");
        }
        append_text_copy_page(&source, &mut text, last).unwrap();
        assert_eq!(text, "猫犬");

        let large = text_copy_source(2, (super::super::TEXT_COPY_PAGE_BYTES + 1) as u64);
        assert!(
            append_text_copy_page(
                &large,
                &mut String::new(),
                super::super::TextCopyPage {
                    source: large.clone(),
                    offset: 0,
                    text: "x".repeat(super::super::TEXT_COPY_PAGE_BYTES + 1),
                    done: true,
                }
            )
            .is_err()
        );
        let empty = text_copy_source(3, 0);
        append_text_copy_page(
            &empty,
            &mut String::new(),
            super::super::TextCopyPage {
                source: empty.clone(),
                offset: 0,
                text: String::new(),
                done: true,
            },
        )
        .unwrap();
        let over_limit = text_copy_source(4, super::super::TEXT_COPY_LIMIT_BYTES as u64 + 1);
        assert!(
            append_text_copy_page(
                &over_limit,
                &mut String::new(),
                super::super::TextCopyPage {
                    source: over_limit.clone(),
                    offset: 0,
                    text: String::new(),
                    done: false,
                }
            )
            .is_err()
        );
        assert!(text_copy_remaining(Instant::now()).is_err());
    }

    #[test]
    fn text_copy_replies_refuse_other_chat_and_other_response_types() {
        let chat_id = TerminalId::new();
        let source = text_copy_source(1, 0);
        let message = ServerMessage::Response {
            response: ServerResponse::ChatTextCopy {
                chat_id: TerminalId::new(),
                page: super::super::TextCopyPage {
                    source,
                    offset: 0,
                    text: String::new(),
                    done: true,
                },
            },
        };
        assert!(text_copy_response(chat_id, message).is_err());
        assert!(
            text_copy_response(
                chat_id,
                ServerMessage::Response {
                    response: ServerResponse::Accepted
                }
            )
            .is_err()
        );
    }

    #[test]
    fn text_copy_pending_requests_fail_on_disconnect_and_explicit_close() {
        for explicit_close in [false, true] {
            let (client, mut server) = UnixStream::pair().unwrap();
            let worker = thread::spawn(move || {
                server
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                server
                    .set_write_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let proof = host_proof("text-copy-cleanup-host");
                let mut codec = FrameCodec::new(ipc::MAX_FRAME_BYTES);
                let respond =
                    |codec: &mut FrameCodec, server: &mut UnixStream, request_id, response| {
                        codec
                            .write(
                                server,
                                &ServerEnvelope {
                                    version: PROTOCOL_VERSION,
                                    request_id,
                                    proof: proof.clone(),
                                    message: ServerMessage::Response { response },
                                },
                            )
                            .unwrap();
                    };
                let connect: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
                respond(
                    &mut codec,
                    &mut server,
                    Some(connect.request_id),
                    ServerResponse::Connected {
                        proof: proof.clone(),
                        resumed: vec![],
                    },
                );
                let start: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
                let ClientRequest::ChatStart { session, .. } = start.request else {
                    panic!("start expected")
                };
                let chat_id = session.id;
                respond(
                    &mut codec,
                    &mut server,
                    Some(start.request_id),
                    ServerResponse::ChatAttached {
                        session,
                        snapshot: Snapshot::default(),
                    },
                );
                let copy: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
                assert!(matches!(
                    copy.request,
                    ClientRequest::ChatCommand {
                        command: Command::ReadTextCopy { .. },
                        ..
                    }
                ));
                if explicit_close {
                    respond(
                        &mut codec,
                        &mut server,
                        None,
                        ServerResponse::ChatClosed {
                            chat_id,
                            snapshot: Snapshot {
                                exited: true,
                                ..Default::default()
                            },
                        },
                    );
                }
                // EOF otherwise exercises the reader's disconnect drain.
            });
            let client = ClientHandle::connect_on(
                client,
                omp_card(None, None),
                Intent::Start,
                None,
                false,
                None,
                None,
                None,
                |_| {},
            )
            .unwrap();
            let error = client.copy_text(text_copy_source(1, 100)).unwrap_err();
            assert!(
                error.contains(if explicit_close {
                    "explicitly closed"
                } else {
                    "disconnected"
                }),
                "{error}"
            );
            assert!(client.inner.text_copy_pending.lock().unwrap().is_empty());
            assert_eq!(client.inner.text_copies.load(Ordering::Acquire), 0);
            worker.join().unwrap();
        }
    }

    #[test]
    fn text_copy_expired_deadline_does_not_wait_for_writer_or_send_a_frame() {
        let (stream, _server) = UnixStream::pair().unwrap();
        let wire = Mutex::new(Wire {
            stream,
            codec: FrameCodec::new(ipc::MAX_CLIENT_FRAME_BYTES),
            body_requested: Arc::new(AtomicBool::new(true)),
        });
        let _held = wire.lock().unwrap();
        let error = transmit_text_copy(
            &wire,
            &host_proof("expired-text-copy"),
            1,
            ClientRequest::ChatCommand {
                chat_id: TerminalId::new(),
                command: Command::ReadTextCopy {
                    source: text_copy_source(1, 0),
                    offset: 0,
                },
            },
            Instant::now(),
        )
        .unwrap_err();
        assert!(error.contains("timed out"));
    }

    #[test]
    fn text_copy_wire_is_request_specific_bounded_and_pages_exact_raw_utf8() {
        let mut clients = Vec::new();
        let mut servers = Vec::new();
        let mut ready = Vec::new();
        let mut release = Vec::new();
        let mut finish = Vec::new();
        for client_index in 0..2_u64 {
            let (client, mut server) = UnixStream::pair().unwrap();
            let (ready_tx, ready_rx) = crossbeam_channel::bounded(1);
            let (release_tx, release_rx) = crossbeam_channel::bounded(1);
            let (finish_tx, finish_rx) = crossbeam_channel::bounded(1);
            ready.push(ready_rx);
            release.push(release_tx);
            finish.push(finish_tx);
            servers.push(thread::spawn(move || {
                server
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                server
                    .set_write_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let proof = host_proof("text-copy-host");
                let mut codec = FrameCodec::new(ipc::MAX_FRAME_BYTES);
                let respond =
                    |codec: &mut FrameCodec, server: &mut UnixStream, request_id, response| {
                        codec
                            .write(
                                server,
                                &ServerEnvelope {
                                    version: PROTOCOL_VERSION,
                                    request_id,
                                    proof: proof.clone(),
                                    message: ServerMessage::Response { response },
                                },
                            )
                            .unwrap();
                    };
                let connect: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
                respond(
                    &mut codec,
                    &mut server,
                    Some(connect.request_id),
                    ServerResponse::Connected {
                        proof: proof.clone(),
                        resumed: vec![],
                    },
                );
                let start: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
                let ClientRequest::ChatStart { session, .. } = start.request else {
                    panic!("start expected")
                };
                let chat_id = session.id;
                respond(
                    &mut codec,
                    &mut server,
                    Some(start.request_id),
                    ServerResponse::ChatAttached {
                        session,
                        snapshot: Snapshot {
                            generation: 1,
                            ..Default::default()
                        },
                    },
                );
                for round in 0..2 {
                    let first: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
                    let second: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
                    if round == 0 {
                        ready_tx.send(()).unwrap();
                        release_rx.recv_timeout(Duration::from_secs(5)).unwrap();
                    }
                    // Coalesced view updates cannot consume requester-specific pages.
                    respond(
                        &mut codec,
                        &mut server,
                        None,
                        ServerResponse::ChatSnapshot {
                            chat_id,
                            snapshot: Some(Snapshot {
                                generation: round + 2,
                                ..Default::default()
                            }),
                        },
                    );
                    for request in [second, first] {
                        let ClientRequest::ChatCommand {
                            chat_id: requested_chat,
                            command: Command::ReadTextCopy { source, offset },
                        } = request.request
                        else {
                            panic!("copy page expected")
                        };
                        assert_eq!(requested_chat, chat_id);
                        assert_eq!(source.token / 10, client_index);
                        let text = if round == 0 {
                            assert_eq!(offset, 0);
                            if source.token % 10 == 1 { "a" } else { "b" }
                                .repeat(super::super::TEXT_COPY_PAGE_BYTES)
                        } else {
                            assert_eq!(offset, super::super::TEXT_COPY_PAGE_BYTES as u64);
                            if source.token % 10 == 1 { "一" } else { "二" }.into()
                        };
                        respond(
                            &mut codec,
                            &mut server,
                            Some(request.request_id),
                            ServerResponse::ChatTextCopy {
                                chat_id,
                                page: super::super::TextCopyPage {
                                    source,
                                    offset,
                                    text,
                                    done: round == 1,
                                },
                            },
                        );
                    }
                }
                finish_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            }));
            clients.push(
                ClientHandle::connect_on(
                    client,
                    omp_card(None, None),
                    Intent::Start,
                    None,
                    false,
                    None,
                    None,
                    None,
                    |_| {},
                )
                .unwrap(),
            );
        }
        let mut requests = Vec::new();
        for (index, client) in clients.iter().enumerate() {
            for token in [1, 2] {
                let client = client.clone();
                requests.push((
                    token,
                    thread::spawn(move || {
                        client.copy_text(text_copy_source(
                            index as u64 * 10 + token,
                            super::super::TEXT_COPY_PAGE_BYTES as u64 + 3,
                        ))
                    }),
                ));
            }
        }
        for event in ready {
            event.recv_timeout(Duration::from_secs(5)).unwrap();
        }
        for client in &clients {
            assert!(
                client
                    .copy_text(text_copy_source(99, 0))
                    .unwrap_err()
                    .contains("capacity")
            );
            assert_eq!(client.inner.text_copy_pending.lock().unwrap().len(), 2);
        }
        for event in release {
            event.send(()).unwrap();
        }
        for (token, request) in requests {
            let expected = format!(
                "{}{}",
                if token == 1 { "a" } else { "b" }.repeat(super::super::TEXT_COPY_PAGE_BYTES),
                if token == 1 { "一" } else { "二" }
            );
            assert_eq!(request.join().unwrap().unwrap(), expected);
        }
        for client in &clients {
            assert!(client.inner.text_copy_pending.lock().unwrap().is_empty());
            assert_eq!(client.inner.text_copies.load(Ordering::Acquire), 0);
        }
        for event in finish {
            event.send(()).unwrap();
        }
        for server in servers {
            server.join().unwrap();
        }
    }

    #[test]
    fn file_changes_replies_are_request_specific_across_clients_and_coalesced_updates() {
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let mut releases = Vec::new();
        let mut handles = Vec::new();
        let mut servers = Vec::new();
        let mut finish = Vec::new();
        for client_index in 0..2_u64 {
            let (client, mut server) = UnixStream::pair().unwrap();
            let ready_tx = ready_tx.clone();
            let (release_tx, release_rx) = std::sync::mpsc::channel();
            releases.push(release_tx);
            let (done_tx, done_rx) = std::sync::mpsc::channel();
            finish.push(done_tx);
            servers.push(thread::spawn(move || {
                server
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                server
                    .set_write_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let proof = host_proof("file-query-host");
                let mut codec = FrameCodec::new(ipc::MAX_FRAME_BYTES);
                let respond =
                    |codec: &mut FrameCodec, server: &mut UnixStream, request_id, response| {
                        codec
                            .write(
                                server,
                                &ServerEnvelope {
                                    version: PROTOCOL_VERSION,
                                    request_id,
                                    proof: proof.clone(),
                                    message: ServerMessage::Response { response },
                                },
                            )
                            .unwrap()
                    };
                let connect: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
                respond(
                    &mut codec,
                    &mut server,
                    Some(connect.request_id),
                    ServerResponse::Connected {
                        proof: proof.clone(),
                        resumed: vec![],
                    },
                );
                let start: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
                let ClientRequest::ChatStart { session, .. } = start.request else {
                    panic!("start expected")
                };
                let chat_id = session.id;
                respond(
                    &mut codec,
                    &mut server,
                    Some(start.request_id),
                    ServerResponse::ChatAttached {
                        session,
                        snapshot: Snapshot {
                            generation: 1,
                            busy: true,
                            read_only_reason: Some("cached fixture".into()),
                            ..Default::default()
                        },
                    },
                );
                let first: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
                let second: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
                ready_tx.send(()).unwrap();
                release_rx.recv_timeout(Duration::from_secs(5)).unwrap();
                // Watch publications intentionally coalesce before either query
                // reply; immutable results still return through their request IDs.
                for generation in [2, 3] {
                    respond(
                        &mut codec,
                        &mut server,
                        None,
                        ServerResponse::ChatSnapshot {
                            chat_id,
                            snapshot: Some(Snapshot {
                                generation,
                                ..Default::default()
                            }),
                        },
                    );
                }
                for request in [second, first] {
                    let ClientRequest::ChatCommand { command, .. } = request.request else {
                        panic!("cache query expected")
                    };
                    let response = match command {
                        Command::LoadFileChanges { request_id, .. } => {
                            ServerResponse::ChatFileChanges {
                                chat_id,
                                summary: super::super::FileChangesSummary {
                                    request_id,
                                    total_files: client_index as usize + 1,
                                    ..Default::default()
                                },
                            }
                        }
                        Command::LoadFileChangePatches {
                            request_id,
                            key,
                            cursor,
                            ..
                        } => ServerResponse::ChatFileChangePatches {
                            chat_id,
                            page: super::super::FileChangePage {
                                request_id,
                                key,
                                revision: 4,
                                patches: vec![],
                                next: cursor,
                                error: None,
                            },
                        },
                        _ => panic!("unexpected provider command"),
                    };
                    respond(&mut codec, &mut server, Some(request.request_id), response);
                }
                done_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            }));
            handles.push(
                ClientHandle::connect_on(
                    client,
                    omp_card(None, None),
                    Intent::Start,
                    None,
                    false,
                    None,
                    None,
                    None,
                    |_| {},
                )
                .unwrap(),
            );
        }
        for handle in &handles {
            assert!(
                handle
                    .file_changes(Command::Interrupt)
                    .unwrap_err()
                    .contains("Expected a file changes cache query")
            );
        }
        let mut queries = Vec::new();
        for (client_index, handle) in handles.iter().enumerate() {
            let a = handle.clone();
            queries.push(thread::spawn(move || {
                match a
                    .file_changes(Command::LoadFileChanges {
                        request_id: 100 + client_index as u64,
                        after: None,
                    })
                    .unwrap()
                {
                    super::super::FileChangeReply::Files(summary) => {
                        assert_eq!(summary.request_id, 100 + client_index as u64);
                        assert_eq!(summary.total_files, client_index + 1);
                    }
                    _ => panic!("wrong query reply"),
                }
            }));
            let b = handle.clone();
            queries.push(thread::spawn(move || {
                let key = super::super::FileChangeKey {
                    source: super::super::FileChangeSource::Conversation,
                    path: format!("file{client_index}.rs"),
                };
                let cursor = super::super::FilePatchCursor {
                    ordinal: 9,
                    file_index: 0,
                    offset: client_index + 1,
                };
                match b
                    .file_changes(Command::LoadFileChangePatches {
                        request_id: 200 + client_index as u64,
                        key: key.clone(),
                        cursor: Some(cursor.clone()),
                        revision: 4,
                    })
                    .unwrap()
                {
                    super::super::FileChangeReply::Patches(page) => {
                        assert_eq!(page.request_id, 200 + client_index as u64);
                        assert_eq!(page.key, key);
                        assert_eq!(page.next, Some(cursor));
                    }
                    _ => panic!("wrong query reply"),
                }
            }));
        }
        for _ in 0..2 {
            ready_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        }
        for release in releases {
            release.send(()).unwrap();
        }
        for query in queries {
            query.join().unwrap();
        }
        for done in finish {
            done.send(()).unwrap();
        }
        for server in servers {
            server.join().unwrap();
        }
    }
    #[test]
    fn retained_claude_pre_id_restore_records_authenticated_writer_lineage() {
        for learned in [false, true] {
            let mut source = omp_card(None, Some(host_proof("prior")));
            source.launch.provider = crate::agent::Provider::Claude;
            source.launch.recipe.program = "claude".into();
            source.native_continuity =
                super::super::NativeContinuity::RetainedClaudeActor { protocol: 2 };
            let mut resolved = source.clone();
            resolved.proof = Some(host_proof("live"));
            resolved.launch.session_id = learned.then(|| CONVERSATION.into());
            let handle = restore_with_lineage(source.clone(), Some(resolved.clone()), |request| {
                let ClientRequest::ChatAttach { session } = request else {
                    panic!("retained recovery must attach without starting");
                };
                let snapshot = Snapshot {
                    session_id: session.launch.session_id.clone(),
                    ..Default::default()
                };
                ServerMessage::Response {
                    response: ServerResponse::ChatAttached { session, snapshot },
                }
            })
            .unwrap();
            assert_eq!(handle.upgrade_recovery(), Some(&(source, resolved)));
        }
    }
    #[test]
    fn unavailable_presentation_disables_readiness_but_preserves_body_and_copy_authority() {
        let (client, mut server) = UnixStream::pair().unwrap();
        let (released, release) = std::sync::mpsc::channel();
        let (changed, changes) = std::sync::mpsc::channel();
        let session = omp_card(Some(CONVERSATION), Some(host_proof("presentation")));
        let source = session.clone();
        let body = Arc::new(super::super::Item {
            id: "retained-body".into(),
            role: super::super::Role::Assistant,
            content: super::super::ItemContent::Text {
                text: "Preserved private answer λ".into(),
                thinking: false,
                phase: Default::default(),
            },
            started_at: None,
            at: None,
        });
        let initial = Snapshot {
            generation: 7,
            ready: true,
            session_id: source.launch.session_id.clone(),
            items: vec![body.clone()],
            text_copy_sources: super::super::TextCopySources {
                conversation_revision: 3,
                conversation: vec![Some(text_copy_source(11, 28))],
                ..Default::default()
            },
            ..Default::default()
        };
        let before = initial.clone();
        let worker = thread::spawn(move || {
            server
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            server
                .set_write_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let proof = host_proof("presentation");
            let mut codec = FrameCodec::new(ipc::MAX_FRAME_BYTES);
            let respond = |codec: &mut FrameCodec, server: &mut UnixStream, request_id, message| {
                codec
                    .write(
                        server,
                        &ServerEnvelope {
                            version: PROTOCOL_VERSION,
                            request_id,
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
                Some(connect.request_id),
                ServerMessage::Response {
                    response: ServerResponse::Connected {
                        proof: proof.clone(),
                        resumed: vec![],
                    },
                },
            );
            let start: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
            assert!(matches!(start.request, ClientRequest::ChatStart { .. }));
            respond(
                &mut codec,
                &mut server,
                Some(start.request_id),
                ServerMessage::Response {
                    response: ServerResponse::ChatAttached {
                        session,
                        snapshot: initial,
                    },
                },
            );
            let command: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
            assert!(matches!(command.request, ClientRequest::ChatCommand { .. }));
            respond(
                &mut codec,
                &mut server,
                Some(command.request_id),
                ServerMessage::Error(ipc::ProtocolError::new(
                    ipc::ProtocolErrorCode::TerminalUnavailable,
                    "one command was refused",
                    true,
                )),
            );
            let visible: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
            assert!(matches!(
                visible.request,
                ClientRequest::ChatVisibility { visible: true, .. }
            ));
            respond(
                &mut codec,
                &mut server,
                Some(visible.request_id),
                ServerMessage::Response {
                    response: ServerResponse::Accepted,
                },
            );
            respond(
                &mut codec,
                &mut server,
                None,
                ServerMessage::ChatChanged { chat_id: source.id },
            );
            let read: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
            assert!(matches!(
                read.request,
                ClientRequest::ChatRead {
                    after_generation: Some(7),
                    ..
                }
            ));
            respond(
                &mut codec,
                &mut server,
                Some(read.request_id),
                ServerMessage::Error(ipc::ProtocolError::new(
                    ipc::ProtocolErrorCode::TerminalUnavailable,
                    "retained presentation is unavailable",
                    true,
                )),
            );
            release.recv_timeout(Duration::from_secs(5)).unwrap();
        });
        let handle = ClientHandle::connect_on(
            client,
            source.clone(),
            Intent::Start,
            None,
            false,
            None,
            None,
            None,
            move |wake| {
                if matches!(wake, Wake::Changed) {
                    let _ = changed.send(());
                }
            },
        )
        .unwrap();
        let decoded_body = handle.snapshot().items[0].clone();
        assert_eq!(
            handle.send(Command::Refresh).unwrap_err(),
            "one command was refused"
        );
        assert!(
            handle.snapshot().ready,
            "request-specific refusal must not disable presentation"
        );
        assert_eq!(handle.snapshot().error, None);
        handle.set_visible(true).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let after = loop {
            let snapshot = handle.snapshot();
            if !snapshot.ready
                && snapshot.error.as_deref() == Some("retained presentation is unavailable")
            {
                break snapshot;
            }
            changes
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .unwrap();
        };
        assert!(!after.ready);
        assert_eq!(
            after.error.as_deref(),
            Some("retained presentation is unavailable")
        );
        assert_eq!(after.generation, before.generation);
        assert_eq!(after.items, before.items);
        assert!(Arc::ptr_eq(&after.items[0], &decoded_body));
        assert_eq!(after.text_copy_sources, before.text_copy_sources);
        assert_eq!(after.session_id, before.session_id);
        assert_eq!(after.exited, before.exited);
        assert_eq!(handle.session(), source);
        assert!(handle.inner.alive.load(Ordering::Acquire));
        released.send(()).unwrap();
        worker.join().unwrap();
    }

    // Test-only injected thread-factory failure, scoped to an exact fresh client.
    // It exercises the real fallible empty-owner startup path before handshake.
    static FAIL_RETIREMENT_START: Mutex<std::collections::BTreeSet<TerminalId>> =
        Mutex::new(std::collections::BTreeSet::new());

    pub(super) fn fail_retirement_start(id: TerminalId) -> bool {
        FAIL_RETIREMENT_START
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&id)
    }

    fn retirement_text(body: String) -> Arc<super::super::Item> {
        Arc::new(super::super::Item {
            id: "retirement-source".into(),
            role: super::super::Role::Assistant,
            started_at: None,
            at: None,
            content: super::super::ItemContent::Text {
                text: body,
                thinking: false,
                phase: Default::default(),
            },
        })
    }

    /// An actual existing FrameCodec/UnixStream connection; no provider fixture,
    /// substitute JoinHandle, fabricated lifecycle or synthetic body ownership.
    fn retirement_wire(
        session: Session,
        initial: Snapshot,
        update: Option<super::super::presentation::Presentation>,
        wake: impl Fn(Wake) + Send + Sync + 'static,
    ) -> (ClientHandle, JoinHandle<()>, std::sync::mpsc::Receiver<()>) {
        let (client, mut server) = UnixStream::pair().unwrap();
        let (eof, observed_eof) = std::sync::mpsc::channel();
        let worker = thread::spawn(move || {
            let mut codec = FrameCodec::new(ipc::MAX_FRAME_BYTES);
            handoff_handshake(&mut server, &mut codec);
            let request: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
            let ClientRequest::ChatStart { session, .. } = request.request else {
                panic!("expected actual ChatStart handshake")
            };
            let chat_id = session.id;
            handoff_reply(
                &mut server,
                &mut codec,
                Some(request.request_id),
                &handoff_proof(),
                ServerMessage::Response {
                    response: ServerResponse::ChatAttached {
                        session,
                        snapshot: initial,
                    },
                },
            );
            if let Some(update) = update {
                let visible: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
                assert!(matches!(
                    visible.request,
                    ClientRequest::ChatVisibility { visible: true, .. }
                ));
                handoff_reply(
                    &mut server,
                    &mut codec,
                    None,
                    &handoff_proof(),
                    ServerMessage::Response {
                        response: ServerResponse::ChatUpdate { chat_id, update },
                    },
                );
                let end: Option<ClientEnvelope> = codec.read(&mut server).unwrap();
                assert!(
                    end.is_none(),
                    "Drop shutdown must close the real connection while its reader is paused"
                );
                eof.send(()).unwrap();
            }
        });
        let client = ClientHandle::connect_on(
            client,
            session,
            Intent::Start,
            None,
            false,
            None,
            None,
            None,
            wake,
        )
        .unwrap();
        (client, worker, observed_eof)
    }

    #[test]
    fn dropping_a_paused_client_hands_off_both_joins_without_blocking_other_retirements() {
        use super::super::presentation::{ItemChange, Presentation};
        let session = omp_card(None, None);
        let (paused, observed_pause) = std::sync::mpsc::channel();
        let (resume, released) = std::sync::mpsc::channel();
        PUBLICATION_GATES
            .lock()
            .unwrap()
            .insert(session.id, (paused, released));
        let before = retirement_text("before".into());
        let after = retirement_text("before tail".into());
        let update = Presentation {
            base_generation: 1,
            metadata: Snapshot {
                generation: 2,
                ..Default::default()
            },
            order: vec![after.id.clone()],
            changes: vec![ItemChange::between(Some(&before), &after)],
            children: vec![],
            file_changes_unchanged: false,
        };
        let (callback, callbacks) = std::sync::mpsc::channel();
        let (mut client, server, eof) = retirement_wire(
            session,
            Snapshot {
                generation: 1,
                items: vec![before],
                ..Default::default()
            },
            Some(update),
            move |wake| {
                let _ = callback.send(wake);
            },
        );
        let (finished, observed_finish) = std::sync::mpsc::channel();
        Arc::get_mut(&mut client.inner).unwrap().retirement_complete = Some(finished);
        let alive = client.inner.alive.clone();
        client.set_visible(true).unwrap();
        observed_pause.recv_timeout(Duration::from_secs(5)).unwrap();
        while callbacks.try_recv().is_ok() {}
        let (dropped, observed_drop) = std::sync::mpsc::channel();
        let dropping = thread::spawn(move || {
            drop(client);
            dropped.send(thread::current().id()).unwrap();
        });
        let returned_while_paused = observed_drop.recv_timeout(Duration::from_secs(1));
        let cancelled_while_paused = eof.recv_timeout(Duration::from_secs(1));
        let unfinished_was_retained = observed_finish.try_recv().is_err();
        let cancelled_before_resume = !alive.load(Ordering::Acquire);
        let no_callback_before_resume = callbacks.try_recv().is_err();
        // A second real owned payload has no connection threads to wait on.
        // It must retire while the first real connection remains gated.
        let mut independent = ClientHandle::fixture(omp_card(None, None), Snapshot::default());
        let (other_finished, other_observed) = std::sync::mpsc::channel();
        Arc::get_mut(&mut independent.inner)
            .unwrap()
            .retirement_complete = Some(other_finished);
        drop(independent);
        let other_retired_while_paused = other_observed.recv_timeout(Duration::from_secs(1));
        // Always release/retire the real held reader before judging results.
        resume.send(()).unwrap();
        dropping.join().unwrap();
        server.join().unwrap();
        let released_on = observed_finish
            .recv_timeout(Duration::from_secs(5))
            .unwrap();
        let late: Vec<_> = callbacks.try_iter().collect();
        assert!(
            cancelled_before_resume,
            "alive must be fenced inline before the held reader resumes"
        );
        assert!(no_callback_before_resume);
        assert!(late.iter().any(|wake| matches!(wake, Wake::Changed)));
        assert!(
            late.iter()
                .any(|wake| matches!(wake, Wake::Disconnected(_))),
            "real queued callbacks may arrive after Drop returns; their UI Target must remain fenced"
        );
        let dropped_on = returned_while_paused
            .expect("dropping the last UI owner must not join its gated reader");
        assert!(
            cancelled_while_paused.is_ok(),
            "actual socket shutdown must remain inline"
        );
        assert!(
            unfinished_was_retained,
            "never claim destruction/join completion while the reader is gated"
        );
        assert_ne!(released_on, dropped_on);
        assert_eq!(
            other_retired_while_paused.unwrap(),
            released_on,
            "one unfinished reader must not head-of-line block another ready payload"
        );
    }

    #[test]
    fn settled_reader_and_failed_join_release_the_actual_large_publication_off_the_dropping_thread()
    {
        let body = "large retained client body\n".repeat(200_000);
        let bytes = body.len();
        let (mut client, server, _) = retirement_wire(
            omp_card(None, None),
            Snapshot {
                generation: 1,
                items: vec![retirement_text(body)],
                ..Default::default()
            },
            None,
            |wake| {
                if matches!(wake, Wake::Disconnected(_)) {
                    panic!("fixture settled reader failure");
                }
            },
        );
        server.join().unwrap();
        let until = Instant::now() + Duration::from_secs(5);
        loop {
            let reader_done = client
                .inner
                .reader
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .is_finished();
            let visibility_done = client
                .inner
                .visibility_worker
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .is_finished();
            if reader_done && visibility_done {
                break;
            }
            assert!(
                Instant::now() < until,
                "actual connection threads must settle before the final-owner test"
            );
            thread::sleep(Duration::from_millis(1));
        }
        let publication = Arc::downgrade(&client.inner.publication);
        let state = client.inner.publication.current.load_full();
        assert_eq!(state.snapshot.transcript_bytes(), bytes);
        let snapshot = Arc::downgrade(&state.snapshot);
        let (body_released, body_completion) = std::sync::mpsc::channel();
        *state.retirement_complete.lock().unwrap() = Some(body_released);
        drop(state);
        let (finished, observed_finish) = std::sync::mpsc::channel();
        Arc::get_mut(&mut client.inner).unwrap().retirement_complete = Some(finished);
        let dropping_thread = thread::current().id();
        drop(client);
        let released_on = observed_finish
            .recv_timeout(Duration::from_secs(5))
            .unwrap();
        assert_ne!(
            released_on, dropping_thread,
            "even a settled connection's large last publication belongs off UI"
        );
        assert!(
            publication.upgrade().is_none(),
            "retirement must release the actual publication owner"
        );
        assert_eq!(
            body_completion
                .recv_timeout(Duration::from_secs(5))
                .unwrap(),
            released_on
        );
        assert!(
            snapshot.upgrade().is_none(),
            "retirement must destroy the final actual snapshot body, not only its handles"
        );
    }

    #[test]
    fn a_panicking_payload_does_not_strand_later_clients() {
        struct CallbackOwner(std::sync::mpsc::Sender<std::thread::ThreadId>);
        impl Drop for CallbackOwner {
            fn drop(&mut self) {
                let _ = self.0.send(thread::current().id());
                panic!("fixture callback owner destructor");
            }
        }
        let (destroyed, destruction) = std::sync::mpsc::channel();
        let owned = CallbackOwner(destroyed);
        let mut first = ClientHandle::fixture(omp_card(None, None), Snapshot::default());
        Arc::get_mut(&mut first.inner).unwrap().wake = Arc::new(move |_| {
            let _ = &owned;
        });
        let dropping_thread = thread::current().id();
        drop(first);
        let failed_on = destruction.recv_timeout(Duration::from_secs(5)).unwrap();
        let mut next = ClientHandle::fixture(omp_card(None, None), Snapshot::default());
        let (finished, completion) = std::sync::mpsc::channel();
        Arc::get_mut(&mut next.inner).unwrap().retirement_complete = Some(finished);
        drop(next);
        assert_ne!(failed_on, dropping_thread);
        assert_eq!(
            completion.recv_timeout(Duration::from_secs(5)).unwrap(),
            failed_on,
            "the same process-owned worker must survive an ordinary payload destructor unwind"
        );
    }

    #[test]
    fn retirement_worker_start_failure_refuses_before_handshake_or_payload_admission() {
        use std::io::Read;
        let session = omp_card(None, None);
        FAIL_RETIREMENT_START.lock().unwrap().insert(session.id);
        let (client, mut server) = UnixStream::pair().unwrap();
        server
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let result = ClientHandle::connect_on(
            client,
            session,
            Intent::Start,
            None,
            false,
            None,
            None,
            None,
            |_| {},
        );
        let Err(error) = result else {
            panic!("thread factory failure must refuse the client");
        };
        assert!(error.message.contains("retirement worker startup failed"));
        let mut byte = [0];
        assert_eq!(
            server.read(&mut byte).unwrap(),
            0,
            "no Connect/ChatStart or body ownership may precede retirement admission"
        );
    }

    fn observed_large_publication() -> (
        Arc<Publication>,
        std::sync::Weak<Snapshot>,
        std::sync::mpsc::Receiver<std::thread::ThreadId>,
    ) {
        let body = "large publication last owner\n".repeat(400_000);
        let publication = Publication::initial(Snapshot {
            generation: 1,
            items: vec![retirement_text(body)],
            ..Default::default()
        });
        let snapshot = Arc::downgrade(&publication.snapshot);
        let (completed, completion) = std::sync::mpsc::channel();
        *publication.retirement_complete.lock().unwrap() = Some(completed);
        (publication, snapshot, completion)
    }

    #[test]
    fn an_idle_hide_reports_the_installed_policy_without_another_source_frame() {
        use tracing::field::{Field, Visit};
        use tracing_subscriber::{Layer, layer::SubscriberExt};

        #[derive(Default)]
        struct Fields(BTreeMap<String, serde_json::Value>);
        impl Visit for Fields {
            fn record_u64(&mut self, field: &Field, value: u64) {
                self.0.insert(field.name().into(), value.into());
            }
            fn record_bool(&mut self, field: &Field, value: bool) {
                self.0.insert(field.name().into(), value.into());
            }
            fn record_str(&mut self, field: &Field, value: &str) {
                self.0.insert(field.name().into(), value.into());
            }
            fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
                self.0
                    .insert(field.name().into(), format!("{value:?}").into());
            }
        }
        struct Observe {
            client: std::sync::Weak<Inner>,
            rows: Arc<Mutex<Vec<BTreeMap<String, serde_json::Value>>>>,
        }
        impl<S: tracing::Subscriber> Layer<S> for Observe {
            fn on_event(
                &self,
                event: &tracing::Event<'_>,
                _: tracing_subscriber::layer::Context<'_, S>,
            ) {
                let mut fields = Fields::default();
                event.record(&mut fields);
                if fields.0.get("owner") != Some(&serde_json::json!("chat_client_policy")) {
                    return;
                }
                let client = self.client.upgrade().expect("caller still owns the client");
                let _gate = client.publication.commit.try_lock().expect(
                    "diagnostic subscriber must run after the publication gate is unlocked",
                );
                self.rows.lock().unwrap().push(fields.0);
            }
        }

        let session = omp_card(None, None);
        let (client, mut server) = UnixStream::pair().unwrap();
        server
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let worker = thread::spawn(move || {
            let mut codec = FrameCodec::new(ipc::MAX_FRAME_BYTES);
            handoff_handshake(&mut server, &mut codec);
            let request: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
            let ClientRequest::ChatStart { session, .. } = request.request else {
                panic!("expected real client attachment")
            };
            handoff_reply(
                &mut server,
                &mut codec,
                Some(request.request_id),
                &handoff_proof(),
                ServerMessage::Response {
                    response: ServerResponse::ChatAttached {
                        session,
                        snapshot: Snapshot {
                            generation: 41,
                            items: vec![retirement_text("idle body".repeat(128 * 1024))],
                            ..Default::default()
                        },
                    },
                },
            );
            // Keep the real reader idle: never send an ACK, snapshot, update or
            // other frame after attachment. Drain visibility only until Drop.
            while codec.read::<ClientEnvelope>(&mut server).unwrap().is_some() {}
        });
        let handle = ClientHandle::connect_on(
            client,
            session,
            Intent::Start,
            None,
            false,
            None,
            None,
            None,
            |_| {},
        )
        .unwrap();
        let body = Arc::downgrade(&handle.snapshot().items[0]);
        let installed = handle.inner.publication.current.load_full();
        let (released, completion) = std::sync::mpsc::channel();
        *installed.retirement_complete.lock().unwrap() = Some(released);
        drop(installed);
        let rows = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::registry().with(Observe {
            client: Arc::downgrade(&handle.inner),
            rows: rows.clone(),
        });
        tracing::subscriber::with_default(subscriber, || {
            handle.set_visible(false).unwrap();
            handle.set_visible(false).unwrap();
            assert!(handle.resident_snapshot_with_clock().is_none());
            assert_eq!(handle.snapshot().generation, 41);
            handle.set_visible(true).unwrap();
            // A show requests hydration; it must not claim the old body became
            // resident merely because interest changed while the source is idle.
            assert!(handle.resident_snapshot_with_clock().is_none());
            handle.set_visible(true).unwrap();
        });
        completion.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(body.upgrade().is_none());
        let observed = rows.lock().unwrap();
        assert_eq!(
            observed.len(),
            2,
            "unchanged calls must not spam policy events"
        );
        for (row, revision, requested, swapped) in [
            (&observed[0], 1, false, true),
            (&observed[1], 2, true, false),
        ] {
            assert_eq!(row["policy_revision"], revision);
            assert_eq!(row["published_generation"], 41);
            assert_eq!(row["published_resident"], false);
            assert_eq!(row["metadata_installed"], swapped);
            assert_eq!(row["requested_interested"], requested);
            assert_eq!(row["requested_retain_body"], requested);
            assert!(!row.contains_key("resident_bytes"));
            assert!(!row.contains_key("released"));
        }
        drop(observed);
        drop(handle);
        worker.join().unwrap();
    }

    #[test]
    fn hiding_retires_the_actual_last_publication_body_off_the_caller() {
        let client = ClientHandle::fixture(omp_card(None, None), Snapshot::default());
        let (publication, body, completion) = observed_large_publication();
        let identity = Arc::downgrade(&publication);
        drop(client.inner.publication.current.swap(publication));
        let caller = thread::current().id();
        client.set_interest(false, false).unwrap();
        assert!(!client.inner.publication.current.load().resident);
        let destroyed_on = completion.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_ne!(destroyed_on, caller);
        assert!(identity.upgrade().is_none());
        assert!(
            body.upgrade().is_none(),
            "the actual large Snapshot must be released, not only a spare wrapper"
        );
    }

    #[test]
    fn a_final_arc_swap_reader_guard_retires_its_body_off_the_reader() {
        let client = ClientHandle::fixture(omp_card(None, None), Snapshot::default());
        let (publication, body, completion) = observed_large_publication();
        drop(client.inner.publication.current.swap(publication));
        let guard = client.inner.publication.current.load();
        let original = Arc::as_ptr(&guard);
        let publications = client.inner.publication.clone();
        let writer = thread::spawn(move || {
            assert_eq!(
                publications.replace(
                    Snapshot {
                        generation: 2,
                        ..Default::default()
                    },
                    &AtomicBool::new(true),
                    false,
                    false
                ),
                Some(true)
            );
        });
        writer.join().unwrap();
        assert_ne!(
            Arc::as_ptr(&client.inner.publication.current.load()),
            original
        );
        assert_eq!(
            guard.snapshot.generation, 1,
            "the real ArcSwap guard still owns the superseded publication"
        );
        assert!(body.upgrade().is_some());
        assert!(completion.try_recv().is_err());
        let reader = thread::current().id();
        drop(guard);
        assert_ne!(
            completion.recv_timeout(Duration::from_secs(5)).unwrap(),
            reader
        );
        assert!(body.upgrade().is_none());
    }

    #[test]
    fn ordinary_reader_replacement_disposes_its_previous_body_on_that_reader() {
        use super::super::presentation::{ItemChange, Presentation};
        let body = "source reader disposal\n".repeat(200_000);
        let before = retirement_text(body.clone());
        let after = retirement_text(body + "tail");
        let update = Presentation {
            base_generation: 1,
            metadata: Snapshot {
                generation: 2,
                ..Default::default()
            },
            order: vec![after.id.clone()],
            changes: vec![ItemChange::between(Some(&before), &after)],
            children: vec![],
            file_changes_unchanged: false,
        };
        let (client, server, eof) = retirement_wire(
            omp_card(None, None),
            Snapshot {
                generation: 1,
                items: vec![before],
                ..Default::default()
            },
            Some(update),
            |_| {},
        );
        let reader = client
            .inner
            .reader
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .thread()
            .id();
        let current = client.inner.publication.current.load_full();
        let previous_body = Arc::downgrade(&current.snapshot);
        let (completed, completion) = std::sync::mpsc::channel();
        *current.retirement_complete.lock().unwrap() = Some(completed);
        drop(current);
        client.set_visible(true).unwrap();
        let disposal = completion.recv_timeout(Duration::from_secs(5));
        // Release/finish the real wire before judging the thread oracle.
        drop(client);
        eof.recv_timeout(Duration::from_secs(5)).unwrap();
        server.join().unwrap();
        assert_eq!(
            disposal.unwrap(),
            reader,
            "ordinary source updates must keep actual prior-body disposal on their producer, preserving backpressure"
        );
        assert!(previous_body.upgrade().is_none());
    }

    #[test]
    fn retirement_initialization_retries_after_a_thread_factory_error() {
        let initializer = ClientRetirementInitializer::new();
        let starts = AtomicUsize::new(0);
        let failure = initializer.get_or_start(|| {
            starts.fetch_add(1, Ordering::Relaxed);
            ClientRetirement::start_with(|_| Err(io::Error::from(io::ErrorKind::WouldBlock)))
        });
        assert!(failure.is_err());
        assert!(
            initializer.owner.get().is_none(),
            "failed startup must not poison later admission"
        );
        let owner = initializer
            .get_or_start(|| {
                starts.fetch_add(1, Ordering::Relaxed);
                ClientRetirement::start_with(|worker| {
                    thread::Builder::new()
                        .name("chat-retire-initializer-fixture".into())
                        .spawn(worker)
                })
            })
            .unwrap();
        let same = initializer
            .get_or_start(|| {
                panic!("a successful owner must be reused without another worker spawn")
            })
            .unwrap();
        assert!(Arc::ptr_eq(&owner, &same));
        assert_eq!(
            starts.load(Ordering::Relaxed),
            2,
            "one failed attempt and one actual successful worker"
        );
        let worker = owner.worker.lock().unwrap().as_ref().unwrap().thread().id();
        // Exercise real payload admission after successful retry, not merely a
        // stored flag. The isolated worker is process-owned, just like shared().
        let mut client = ClientHandle::fixture(omp_card(None, None), Snapshot::default());
        let inner = Arc::get_mut(&mut client.inner).unwrap();
        inner.retirement = owner;
        let (finished, completion) = std::sync::mpsc::channel();
        inner.retirement_complete = Some(finished);
        drop(client);
        assert_eq!(
            completion.recv_timeout(Duration::from_secs(5)).unwrap(),
            worker
        );
    }
}
