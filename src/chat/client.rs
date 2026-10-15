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
use std::{
    collections::{BTreeMap, VecDeque},
    io,
    net::Shutdown,
    os::unix::net::UnixStream,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

// Native holder startup can spend 30 seconds sending its control request and
// another 30 seconds receiving the reply. Leave room for the daemon response
// and a busy desktop scheduler.
const CHAT_ATTACH_TIMEOUT: Duration = Duration::from_secs(70);

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
}
type CommandResult = Result<Option<crate::terminal_ipc::TerminalHandoffReceipt>, String>;
#[derive(Clone)]
enum PendingKind {
    Command,
    Close,
    Handoff(Arc<crate::terminal_ipc::ChatSourceBinding>),
}
struct PendingReply {
    kind: PendingKind,
    sender: crossbeam_channel::Sender<CommandResult>,
}
impl PendingReply {
    fn try_send(&self, result: CommandResult) -> Result<(), crossbeam_channel::TrySendError<CommandResult>> {
        self.sender.try_send(result)
    }
}
type Pending = Arc<Mutex<BTreeMap<u64, PendingReply>>>;
type FilePending =
    Arc<Mutex<BTreeMap<u64, crossbeam_channel::Sender<Result<super::FileChangeReply, String>>>>>;
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

fn valid_source_binding(
    binding: &crate::terminal_ipc::ChatSourceBinding,
    source: &Session,
    instance: uuid::Uuid,
    proof: &HostSessionProof,
) -> bool {
    !instance.is_nil()
        && !binding.backend.is_nil()
        && binding.execution_instance == instance
        && binding.source.id == source.id
        && binding.source.proof.as_ref() == Some(proof)
        && binding.source.launch.provider == source.launch.provider
        && binding.source.launch.recipe == source.launch.recipe
}

fn valid_handoff_receipt(
    receipt: &crate::terminal_ipc::TerminalHandoffReceipt,
    expected: &crate::terminal_ipc::ChatSourceBinding,
    proof: &HostSessionProof,
) -> bool {
    if !proof.is_complete()
        || receipt.operation.is_nil()
        || receipt.terminal_native_epoch.is_nil()
        || receipt.terminal.execution_instance.is_nil()
        || receipt.source_binding != *expected
        || receipt.source_execution_instance != expected.execution_instance
        || expected.source.proof.as_ref() != Some(proof)
        || receipt
            .terminal
            .session
            .continuity
            .live_proof()
            .as_ref()
            .is_some_and(|issued| issued != proof)
    {
        return false;
    }
    let mut source = expected.source.clone();
    if source.launch.session_id.is_some()
        && source.launch.session_id != receipt.source.launch.session_id
    {
        return false;
    }
    source.launch.session_id = receipt.source.launch.session_id.clone();
    source.native_continuity = receipt.source.native_continuity.clone();
    if source != receipt.source {
        return false;
    }
    if let Some(retained) = &receipt.retained_binding {
        if !valid_source_binding(retained, &source, expected.execution_instance, proof)
            || retained.source.launch.session_id != source.launch.session_id
        {
            return false;
        }
        let mut post = source.clone();
        post.native_continuity = retained.source.native_continuity.clone();
        if retained.source != post {
            post.terminal = Some(receipt.terminal.session.clone());
            if retained.source != post {
                return false;
            }
        }
    } else if source.launch.session_id != expected.source.launch.session_id {
        return false; // First learning requires the authenticated canonical publication.
    }
    true
}

fn valid_restart_attachment(
    receipt: &super::RestartReceipt,
    expected: &Session,
    snapshot: &Snapshot,
    instance: uuid::Uuid,
    binding: Option<&ipc::ChatSourceBinding>,
    proof: &HostSessionProof,
) -> bool {
    receipt.is_valid()
        && receipt.source() == expected
        && receipt.replacement().proof.as_ref() == Some(proof)
        && snapshot.session_id == receipt.replacement().launch.session_id
        && binding.is_some_and(|binding|
            valid_source_binding(binding, receipt.replacement(), instance, proof)
                && &binding.source == receipt.replacement())
}

struct Inner {
    id: TerminalId,
    session: Session,
    execution_instance: uuid::Uuid,
    source_binding: Arc<Mutex<Option<crate::terminal_ipc::ChatSourceBinding>>>,
    upgrade_recovery: Option<(Session, Session)>,
    // Immutable receipt from the correlated initial Attach response. Later
    // snapshots/binding refreshes never rewrite its original source lineage.
    restart_receipt: Option<super::RestartReceipt>,
    proof: HostSessionProof,
    // Captured from this authenticated connection, never resolved from a later
    // device selection, daemon discovery or attachment.
    return_endpoint: Option<std::path::PathBuf>,
    return_replay: Mutex<()>,
    wire: Arc<Mutex<Wire>>,
    cancel: UnixStream,
    snapshot: Arc<Mutex<Arc<Snapshot>>>,
    source_clock: Arc<Mutex<Option<SourceClock>>>,
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
    visibility_tx: crossbeam_channel::Sender<()>,
    visibility_worker: Mutex<Option<JoinHandle<()>>>,
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
    outcome: Option<crate::agent::ActivityOutcome>,
    pending: bool,
}

impl ActivityObservation {
    fn observe(&mut self, generation: u64, state: ActivityState) -> bool {
        self.observe_outcome(generation, state, None)
    }

    fn observe_outcome(
        &mut self,
        generation: u64,
        state: ActivityState,
        outcome: Option<crate::agent::ActivityOutcome>,
    ) -> bool {
        if generation < self.generation {
            return false;
        }
        self.generation = generation;
        if self.state == state && self.outcome == outcome {
            return false;
        }
        self.state = state;
        self.outcome = outcome;
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
    let Wire { stream, codec } = &mut *wire;
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
        .map_err(|e| e.to_string())
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

// The same deadline is applied before EVERY partial read/write, so a peer
// dripping one frame cannot renew the retry budget with each socket syscall.
struct ReturnReplayIo { stream: UnixStream, deadline: Instant }
impl ReturnReplayIo {
    fn remaining(&self) -> io::Result<Duration> {
        let remaining = self.deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {Err(io::Error::new(io::ErrorKind::TimedOut, "The original Return retry deadline elapsed"))} else {Ok(remaining)}
    }
}
impl io::Read for ReturnReplayIo {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        self.stream.set_read_timeout(Some(self.remaining()?))?;
        io::Read::read(&mut self.stream, bytes)
    }
}
impl io::Write for ReturnReplayIo {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.stream.set_write_timeout(Some(self.remaining()?))?;
        io::Write::write(&mut self.stream, bytes)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.stream.set_write_timeout(Some(self.remaining()?))?;
        io::Write::flush(&mut self.stream)
    }
}
fn return_replay_response(io: &mut ReturnReplayIo, codec: &mut FrameCodec, request: u64, proof: &HostSessionProof) -> Result<ServerResponse, String> {
    let frame: ServerEnvelope = codec.read(io).map_err(|error| error.to_string())?.ok_or("The original Return daemon disconnected")?;
    io.remaining().map_err(|error| error.to_string())?;
    if frame.version != PROTOCOL_VERSION || frame.proof != *proof || !frame.proof.is_complete() || frame.request_id != Some(request) {
        return Err("The original Return retry response changed its protocol, proof or request".into());
    }
    match frame.message {
        ServerMessage::Response {response} => Ok(response),
        ServerMessage::Error(error) => Err(error.message),
        _ => Err("The original Return retry received an unsolicited payload".into()),
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
) -> Result<ClientRequest, StartFailure> {
    if session.proof.as_ref() == Some(proof) {
        return Ok(ClientRequest::ChatAttach { session });
    }
    let mut refusal = None;
    if !session
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
                if session.launch.session_id.is_some()
                    && (session.launch.session_id != restored.launch.session_id
                        || session.launch.recipe != restored.launch.recipe)
                {
                    if !super::is_upgrade_recovery(&session, &restored) {
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
    if matches!(
        session.native_continuity,
        super::NativeContinuity::Durable { .. }
    ) {
        return Err(StartFailure::from(format!(
            "The saved native conversation could not be reattached. Its uncertain work was not restarted or resubmitted. {}",
            refusal.unwrap_or_else(|| "No authenticated native holder was available.".into())
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
        let activity = ActivityObservation {
            generation: snapshot.generation,
            state: snapshot.activity(),
            outcome: snapshot.outcome.clone(),
            pending: false,
        };
        Self {
            inner: Arc::new(Inner {
                id: session.id,
                session,
                execution_instance: uuid::Uuid::new_v4(),
                source_binding: Arc::new(Mutex::new(None)),
                upgrade_recovery: None,
                restart_receipt: None,
                proof,
                return_endpoint: None,
                return_replay: Mutex::new(()),
                wire: Arc::new(Mutex::new(Wire {
                    stream,
                    codec: FrameCodec::new(ipc::MAX_FRAME_BYTES),
                })),
                cancel,
                source_clock: Arc::new(Mutex::new(SourceClock::from_sample(
                    snapshot.sampled_at,
                    Instant::now(),
                ))),
                snapshot: Arc::new(Mutex::new(Arc::new(snapshot))),
                activity: Arc::new(Mutex::new(activity)),
                observed: Arc::new(Mutex::new(OutputObservation::default())),
                account_usage: Arc::default(),
                next: Arc::new(AtomicU64::new(1)),
                alive: Arc::new(AtomicBool::new(true)),
                visible: Arc::new(AtomicBool::new(true)),
                wake: Arc::new(|_| {}),
                reader: Mutex::new(None),
                pending: Arc::new(Mutex::new(BTreeMap::new())),
                file_pending: Arc::new(Mutex::new(BTreeMap::new())),
                visibility_tx,
                visibility_worker: Mutex::new(None),
            }),
        }
    }

    /// Explicit positive authority for isolated UI fixtures. Ordinary display
    /// fixtures stay unbound and exercise the real unknown-authority refusal.
    /// No running backend, server acknowledgement or native test is implied.
    #[cfg(test)]
    pub(crate) fn fixture_with_source_binding(session: Session, snapshot: Snapshot) -> Self {
        let handle = Self::fixture(session, snapshot);
        let mut source = handle.inner.session.clone();
        source.proof = Some(handle.inner.proof.clone());
        source.launch.session_id = handle.snapshot().session_id.clone();
        let binding = crate::terminal_ipc::ChatSourceBinding {
            source,
            execution_instance: handle.inner.execution_instance,
            backend: uuid::Uuid::new_v4(),
        };
        assert!(valid_source_binding(
            &binding,
            &handle.inner.session,
            handle.inner.execution_instance,
            &handle.inner.proof
        ));
        *handle.inner.source_binding.lock().unwrap() = Some(binding);
        handle
    }

    #[cfg(test)]
    pub(crate) fn fixture_upgrade(source: Session, session: Session, snapshot: Snapshot) -> Self {
        assert!(super::is_upgrade_recovery(&source, &session));
        assert_eq!(snapshot.session_id, session.launch.session_id);
        let proof = session.proof.clone().unwrap();
        let mut handle = Self::fixture(session.clone(), snapshot);
        let inner = Arc::get_mut(&mut handle.inner).unwrap();
        inner.proof = proof;
        inner.upgrade_recovery = Some((source, session));
        handle
    }

    /// Explicit UI-state injection only, never live Host/provider authority.
    /// Wire admission is exercised separately through the actual connect path.
    #[cfg(test)]
    pub(crate) fn fixture_restarted(
        receipt: super::RestartReceipt,
        snapshot: Snapshot,
        upgrade_recovery: Option<(Session, Session)>,
    ) -> Self {
        if let Some((original, resolved)) = &upgrade_recovery {
            assert!(super::is_upgrade_recovery(original, resolved));
            assert_eq!(receipt.source(), resolved);
        }
        let replacement = receipt.replacement().clone();
        let proof = replacement.proof.clone().expect("complete fixture restart proof");
        let mut handle = Self::fixture(replacement.clone(), snapshot);
        let inner = Arc::get_mut(&mut handle.inner).unwrap();
        inner.proof = proof.clone();
        let binding = ipc::ChatSourceBinding {
            source: replacement,
            execution_instance: inner.execution_instance,
            backend: uuid::Uuid::new_v4(),
        };
        assert!(valid_restart_attachment(&receipt, receipt.source(),
            inner.snapshot.lock().unwrap().as_ref(), inner.execution_instance, Some(&binding), &proof));
        *inner.source_binding.lock().unwrap() = Some(binding);
        inner.restart_receipt = Some(receipt);
        inner.upgrade_recovery = upgrade_recovery;
        handle
    }

    /// In-process UI fixtures may exercise the real authenticated Restore
    /// handshake without daemon discovery, a provider or a test-only Ready.
    #[cfg(test)]
    pub(crate) fn restore_on_for_test(
        stream: UnixStream,
        session: Session,
        expected_host: Option<HostSessionProof>,
        wake: impl Fn(Wake) + Send + Sync + 'static,
    ) -> Result<Self, StartFailure> {
        Self::connect_on(stream, session, Intent::Restore, expected_host,
            false, None, None, wake)
    }

    /// Publishes `snapshot` as the reader thread would on a presentation
    /// frame, so a test can stream a transcript into a surface.
    #[cfg(test)]
    pub(crate) fn publish_snapshot_for_test(&self, snapshot: Snapshot) {
        self.install_snapshot_for_test(snapshot);
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

    #[cfg(test)]
    pub(crate) fn replace_snapshot_for_test(&self, snapshot: Snapshot) {
        self.install_snapshot_for_test(snapshot);
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
            wake,
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
        wake: impl Fn(Wake) + Send + Sync + 'static,
    ) -> Result<Self, StartFailure> {
        let wake: Arc<dyn Fn(Wake) + Send + Sync> = Arc::new(wake);
        let return_endpoint = stream.peer_addr().ok().and_then(|address| address.as_pathname().filter(|path| path.is_absolute()).map(std::path::Path::to_path_buf));
        stream
            .set_read_timeout(Some(CHAT_ATTACH_TIMEOUT))
            .map_err(|e| e.to_string())?;
        stream
            .set_write_timeout(Some(Duration::from_secs(5)))
            .map_err(|e| e.to_string())?;
        let mut codec = FrameCodec::new(ipc::MAX_FRAME_BYTES);
        let expected = source.clone();
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
        let request = if let Some((source, idle_generation)) = account_recovery {
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
            )?
        };
        // Preserve the exact submitted source before moving the request into
        // its envelope. Only ChatAttach may return executable restart evidence.
        let restart_source = match &request {
            ClientRequest::ChatAttach { session } => Some(session.clone()),
            _ => None,
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
        let (session, initial, execution_instance, initial_binding, restart_receipt) = loop {
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
                    outcome,
                } if chat_id == id && envelope.request_id.is_none() => {
                    early_activity = Some((generation, state, outcome));
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
                    response:
                        ServerResponse::ChatAttached {
                            session,
                            snapshot,
                            execution_instance,
                            binding,
                        },
                } if session.id == id && envelope.request_id == Some(attach_request) => {
                    #[cfg(feature = "ui")]
                    crate::daemon_health::frame(&stream, None, health_request);
                    if execution_instance.is_nil() {
                        return Err("Native attachment has no exact actor incarnation".into());
                    }
                    if binding.as_ref().is_some_and(|binding| {
                        !valid_source_binding(binding, &session, execution_instance, &proof)
                            || session.launch.session_id.as_ref().is_some_and(|thread| {
                                binding.source.launch.session_id.as_ref() != Some(thread)
                            })
                    }) {
                        return Err("Native attachment has inconsistent backend authority".into());
                    }
                    break (session, snapshot, execution_instance, binding, None);
                }
                ServerMessage::Response { response: ServerResponse::ChatRestarted {
                    receipt, snapshot, execution_instance, binding,
                }} if envelope.request_id == Some(attach_request) => {
                    let expected_source = restart_source.as_ref().ok_or_else(||
                        StartFailure::from("A restarted attachment did not answer an original ChatAttach request"))?;
                    if !valid_restart_attachment(&receipt, expected_source, &snapshot,
                        execution_instance, binding.as_ref(), &proof) {
                        return Err("The restarted attachment did not confirm its exact source, actor and replacement binding".into());
                    }
                    #[cfg(feature = "ui")]
                    crate::daemon_health::frame(&stream, None, health_request);
                    let session = receipt.replacement().clone();
                    break (session, snapshot, execution_instance, binding, Some(receipt));
                }
                ServerMessage::Error(error) if envelope.request_id == Some(attach_request) => {
                    #[cfg(feature = "ui")]
                    crate::daemon_health::frame(&stream, None, health_request);
                    return Err(StartFailure {
                        message: error.message,
                        recovery: if error.code == ipc::ProtocolErrorCode::AccountRemoved {
                            super::Recovery::Account
                        } else {
                            super::Recovery::Retry
                        },
                    });
                }
                _ => return Err("Daemon did not attach the exact native chat".into()),
            }
        };
        if let Some((_, resolved)) = &upgrade_recovery {
            // Upgrade and same-daemon restart are distinct receipts. Keep the
            // original upgrade tuple; the UI must persist this exact two-step
            // chain rather than treating a new paired view as an upgrade alias.
            let confirmed = match restart_receipt.as_ref() {
                Some(receipt) if receipt.source() == resolved => receipt.replacement(),
                Some(_) => return Err("The restart did not continue the exact upgrade replacement".into()),
                None => resolved,
            };
            if &session != confirmed || initial.session_id != confirmed.launch.session_id {
                return Err("The attached conversation did not confirm the daemon's exact upgrade replacement".into());
            }
        }
        stream.set_read_timeout(None).map_err(|e| e.to_string())?;
        let mut reader_stream = stream.try_clone().map_err(|e| e.to_string())?;
        let cancel = stream.try_clone().map_err(|error| error.to_string())?;
        let wire = Arc::new(Mutex::new(Wire {
            stream,
            codec: FrameCodec::new(ipc::MAX_CLIENT_FRAME_BYTES),
        }));
        let mut activity = ActivityObservation {
            generation: initial.generation,
            state: initial.activity(),
            outcome: initial.outcome.clone(),
            pending: false,
        };
        if let Some((generation, state, outcome)) = early_activity {
            activity.observe_outcome(generation, state, outcome);
        }
        let activity = Arc::new(Mutex::new(activity));
        let observed = Arc::new(Mutex::new(early_observed));
        let account_usage = Arc::new(Mutex::new(early_usage));
        let source_clock = Arc::new(Mutex::new(SourceClock::from_sample(
            initial.sampled_at,
            Instant::now(),
        )));
        let snapshot = Arc::new(Mutex::new(Arc::new(initial)));
        let next = Arc::new(AtomicU64::new(attach_request + 1));
        let alive = Arc::new(AtomicBool::new(true));
        let visible = Arc::new(AtomicBool::new(false));
        let source_binding = Arc::new(Mutex::new(initial_binding));
        let pending: Pending = Arc::new(Mutex::new(BTreeMap::new()));
        let file_pending: FilePending = Arc::new(Mutex::new(BTreeMap::new()));
        let (visibility_tx, visibility_rx) = crossbeam_channel::bounded(1);
        let binding_source = session.clone();
        let inner = Arc::new(Inner {
            id,
            session,
            execution_instance,
            source_binding: source_binding.clone(),
            upgrade_recovery,
            restart_receipt,
            proof: proof.clone(),
            return_endpoint,
            return_replay: Mutex::new(()),
            wire: wire.clone(),
            cancel,
            snapshot: snapshot.clone(),
            source_clock: source_clock.clone(),
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
            visibility_tx,
            visibility_worker: Mutex::new(None),
        });
        let visibility_wire = wire.clone();
        let visibility_alive = alive.clone();
        let visibility_value = visible.clone();
        let visibility_proof = proof.clone();
        let visibility_next = next.clone();
        let visibility_worker = thread::Builder::new()
            .name(format!("chat-visibility-{id}"))
            .spawn(move || {
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
            let mut read_pending = false;
            let mut read_again = false;
            let mut resync = false;
            let failure = loop {
                let envelope: ServerEnvelope = match codec.read(&mut reader_stream) {
                    Ok(Some(value)) => value,
                    Ok(None) => break "Chat daemon disconnected; exact session retained for reattachment".into(),
                    Err(error) => break format!("Chat daemon frame failed: {error}"),
                };
                if envelope.version != PROTOCOL_VERSION || envelope.proof != proof { break "Chat daemon identity/version changed; refusing replacement".into(); }
                if matches!(&envelope.message, ServerMessage::Response { response: ServerResponse::ChatClosed { .. } | ServerResponse::ChatTerminal { .. } }) {
                    // Authentication of the daemon alone is not correlation to
                    // this original native intent. A late/cancelled response
                    // has no authority to publish a snapshot or close this actor.
                    let Some(request) = envelope.request_id else { continue; };
                    let Some(kind) = pending.lock().unwrap_or_else(|p|p.into_inner()).get(&request).map(|reply|reply.kind.clone()) else { continue; };
                    let (chat_id, final_snapshot, terminal) = match (envelope.message, kind) {
                        (ServerMessage::Response { response: ServerResponse::ChatClosed { chat_id, snapshot } }, PendingKind::Close) => (chat_id, snapshot, None),
                        (ServerMessage::Response { response: ServerResponse::ChatTerminal { chat_id, session, receipt, snapshot } }, PendingKind::Handoff(expected)) => {
                            if receipt.terminal.session != session || receipt.source.id != chat_id || !valid_handoff_receipt(&receipt, &expected, &proof) {
                                break "Daemon returned inconsistent terminal handoff authority".into();
                            }
                            // A newer positive publication on this same actor
                            // cannot be overwritten by an older operation. None
                            // is normal after exact retirement; the receipt's
                            // own canonical post-pair binding is also explicit.
                            if source_binding.lock().unwrap_or_else(|p|p.into_inner()).as_ref().is_some_and(|current|
                                current != expected.as_ref() && receipt.retained_binding.as_ref() != Some(current)) {
                                break "Native backend changed before terminal handoff publication".into();
                            }
                            (chat_id, snapshot, Some(receipt))
                        },
                        _ => break "Daemon returned a close or handoff for another pending command".into(),
                    };
                    if chat_id!=id { break "Daemon closed a different chat".into(); }
                    // Claim only the same still-pending request after complete
                    // source/backend/actor validation. Timeout may have removed
                    // it while validation ran; then no payload is published.
                    let Some(reply) = pending.lock().unwrap_or_else(|p|p.into_inner()).remove(&request) else { continue; };
                    if terminal.is_some() && !final_snapshot.exited {
                        // A shared-server CLI is another view, not a closed
                        // native actor. Keep this connection and pending work.
                        *snapshot.lock().unwrap_or_else(|p|p.into_inner())=Arc::new(final_snapshot);
                        let _ = reply.try_send(Ok(terminal));
                        wake(Wake::Changed);
                        continue;
                    }
                    *snapshot.lock().unwrap_or_else(|p|p.into_inner())=Arc::new(final_snapshot);
                    {
                        let mut activity = activity.lock().unwrap_or_else(|p| p.into_inner());
                        activity.state = ActivityState::Unknown;
                        activity.pending = false;
                    }
                    alive.store(false,Ordering::Release);
                    let _=visibility_stop.try_send(());
                    let _=reader_stream.shutdown(Shutdown::Both);
                    let _=reply.try_send(Ok(terminal));
                    let replies=std::mem::take(&mut *pending.lock().unwrap_or_else(|p|p.into_inner()));
                    for (_,reply) in replies {
                        let _=reply.try_send(Err("Chat was explicitly closed".into()));
                    }
                    for (_,reply) in std::mem::take(&mut *file_pending.lock().unwrap_or_else(|p|p.into_inner())) {let _=reply.try_send(Err("Chat was explicitly closed".into()));}
                    wake(Wake::Changed);
                    return;
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
                    ServerMessage::ChatActivity { chat_id, state, generation, outcome } if chat_id == id => {
                        let changed = activity.lock().unwrap_or_else(|p| p.into_inner())
                            .observe_outcome(generation, state, outcome);
                        if changed { wake(Wake::Activity); }
                    },
                    ServerMessage::ChatAccountUsage { chat_id, report, observed_at } if chat_id == id => {
                        let first = account_usage.lock().unwrap_or_else(|p| p.into_inner())
                            .push(report, observed_at);
                        if first { wake(Wake::AccountUsage); }
                    },
                    ServerMessage::Response { response: ServerResponse::ChatSnapshot { chat_id, snapshot: update, execution_instance: observed_instance, binding } } if chat_id == id => {
                        if observed_instance != execution_instance || binding.as_ref().is_some_and(|binding|
                            !valid_source_binding(binding, &binding_source, execution_instance, &proof)) {
                            break "Native backend response belongs to a different actor incarnation".into();
                        }
                        *source_binding.lock().unwrap_or_else(|p| p.into_inner()) = binding;
                        read_pending = false;
                        if let Some(update) = update {
                            resync = false;
                            let mut state = snapshot.lock().unwrap_or_else(|p| p.into_inner());
                            SourceClock::observe(&mut source_clock.lock().unwrap_or_else(|p| p.into_inner()), update.sampled_at, Instant::now());
                            *state = Arc::new(update);
                            drop(state);
                            if visible.load(Ordering::Acquire) { wake(Wake::Changed); }
                        }
                    },
                    ServerMessage::Response { response: ServerResponse::ChatUpdate { chat_id, update, execution_instance: observed_instance, binding } } if chat_id == id => {
                        if observed_instance != execution_instance || binding.as_ref().is_some_and(|binding|
                            !valid_source_binding(binding, &binding_source, execution_instance, &proof)) {
                            break "Native backend update belongs to a different actor incarnation".into();
                        }
                        *source_binding.lock().unwrap_or_else(|p| p.into_inner()) = binding;
                        read_pending = false;
                        let mut state = snapshot.lock().unwrap_or_else(|p| p.into_inner());
                        let sampled_at = update.metadata.sampled_at;
                        let applied = match update.apply(&state) {
                            Ok(next) => {
                                *state = Arc::new(next);
                                SourceClock::observe(&mut source_clock.lock().unwrap_or_else(|p| p.into_inner()), sampled_at, Instant::now());
                                true
                            },
                            Err(_) => false,
                        };
                        drop(state);
                        if !applied {
                            resync = true;
                            read_again = true;
                        } else if visible.load(Ordering::Acquire) { wake(Wake::Changed); }
                    },
                    ServerMessage::Error(error) => {
                        read_pending = false;
                        let mut state = snapshot.lock().unwrap_or_else(|p| p.into_inner());
                        Arc::make_mut(&mut state).error = Some(error.message);
                        drop(state); wake(Wake::Changed);
                    },
                    ServerMessage::HostShuttingDown => break "Chat daemon is shutting down".into(),
                    ServerMessage::Response { .. } => {},
                    _ => break "Unexpected native chat protocol frame".into(),
                }
                if read_again && !read_pending && visible.load(Ordering::Acquire) {
                    read_again = false;
                    let generation = snapshot.lock().unwrap_or_else(|p| p.into_inner()).generation;
                    if let Err(error) = transmit(&wire, &proof, next.fetch_add(1, Ordering::Relaxed), ClientRequest::ChatRead { chat_id: id, after_generation: (!resync).then_some(generation) }) { break error; }
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
        if matches!(
            snapshot.native_continuity,
            super::NativeContinuity::Durable { .. }
        ) {
            session.native_continuity = snapshot.native_continuity.clone();
        }
        session
    }
    pub(crate) fn upgrade_recovery(&self) -> Option<&(Session, Session)> {
        self.inner.upgrade_recovery.as_ref()
    }
    /// Original authenticated Attach restart evidence. Persistence callers keep
    /// this receipt separate from the latest live descriptor and upgrade tuple.
    pub(crate) fn restart_receipt(&self) -> Option<&super::RestartReceipt> {
        self.inner.restart_receipt.as_ref()
    }
    pub fn snapshot(&self) -> Arc<Snapshot> {
        self.inner
            .snapshot
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }
    pub(crate) fn snapshot_with_clock(&self) -> (Arc<Snapshot>, Option<SourceClock>) {
        let snapshot = self
            .inner
            .snapshot
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let clock = self
            .inner
            .source_clock
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        (snapshot.clone(), *clock)
    }

    #[cfg(test)]
    fn install_snapshot_for_test(&self, update: Snapshot) {
        let mut state = self
            .inner
            .snapshot
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        SourceClock::observe(
            &mut self
                .inner
                .source_clock
                .lock()
                .unwrap_or_else(|p| p.into_inner()),
            update.sampled_at,
            Instant::now(),
        );
        *state = Arc::new(update);
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
    /// Latest terminal outcome from the small activity stream, including for
    /// hidden chats whose transcript is intentionally not subscribed.
    pub fn activity_outcome(&self) -> Option<crate::agent::ActivityOutcome> {
        self.inner
            .activity
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .outcome
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
    ) -> Result<Option<crate::terminal_ipc::TerminalHandoffReceipt>, String> {
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
            let kind = match &request {
                ClientRequest::ChatReturnTerminal { expected, .. } => PendingKind::Handoff(Arc::new(expected.clone())),
                ClientRequest::ChatClose { .. } | ClientRequest::ChatCloseSession { .. } => PendingKind::Close,
                _ => PendingKind::Command,
            };
            pending.insert(id, PendingReply { kind, sender: reply });
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

    /// Exercise unavailable post-return metadata on the SAME captured handle.
    /// This withdraws authority and never constructs a replacement binding.
    #[cfg(test)]
    pub(crate) fn clear_source_binding_for_test(&self) {
        *self
            .inner
            .source_binding
            .lock()
            .unwrap_or_else(|p| p.into_inner()) = None;
    }

    #[cfg(test)]
    pub(crate) fn publish_source_binding_for_test(&self, binding: crate::terminal_ipc::ChatSourceBinding) {
        assert!(valid_source_binding(&binding, &self.inner.session, self.inner.execution_instance, &self.inner.proof));
        *self.inner.source_binding.lock().unwrap_or_else(|p| p.into_inner()) = Some(binding);
    }

    /// Presentation connection liveness only; this is not native backend or
    /// cleanup authority. A successful control replay does not revive this reader.
    pub fn is_connected(&self) -> bool {
        self.inner.alive.load(Ordering::Acquire)
    }
    #[cfg(test)]
    pub(crate) fn disconnect_reader_for_test(&self) {
        // Negative fixture seam: withdraw liveness, never add a source binding,
        // endpoint, receipt or replacement attachment.
        self.inner.alive.store(false, Ordering::Release);
    }

    /// Latest authenticated live source metadata; absence grants no new authority.
    pub fn source_binding(&self) -> Option<crate::terminal_ipc::ChatSourceBinding> {
        self.inner
            .source_binding
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }
    /// The daemon retains and replays the resulting terminal across lost replies.
    /// Credentials stay entirely within the host's native/PTY execution context.
    pub fn return_terminal(
        &self,
        expected: crate::terminal_ipc::ChatSourceBinding,
    ) -> Result<crate::terminal_ipc::TerminalHandoffReceipt, String> {
        if !valid_source_binding(
            &expected,
            &self.inner.session,
            self.inner.execution_instance,
            &self.inner.proof,
        ) {
            return Err("Return intent has no matching authenticated native backend".into());
        }
        // A later explicit retry may outlive the original reader. It reuses
        // this original handle's endpoint, proof and captured intent; this
        // call never automatically retries an ambiguous live submission.
        if !self.inner.alive.load(Ordering::Acquire) {
            return self.replay_return_terminal(&expected);
        }
        let terminal = self
            .request_response(ClientRequest::ChatReturnTerminal {
                chat_id: self.inner.id,
                expected: expected.clone(),
            })?
            .ok_or("Daemon did not return the exact replacement terminal")?;
        if !valid_handoff_receipt(&terminal, &expected, &self.inner.proof) {
            return Err("Replacement terminal has no matching daemon ownership proof".into());
        }
        Ok(terminal)
    }
    fn replay_return_terminal(
        &self,
        expected: &crate::terminal_ipc::ChatSourceBinding,
    ) -> Result<crate::terminal_ipc::TerminalHandoffReceipt, String> {
        let _replay = match self.inner.return_replay.try_lock() {
            Ok(guard) => guard,
            Err(std::sync::TryLockError::Poisoned(poison)) => poison.into_inner(),
            Err(std::sync::TryLockError::WouldBlock) => return Err("The original Return retry is already in progress".into()),
        };
        let endpoint = self.inner.return_endpoint.as_ref().ok_or("The original authenticated Return endpoint is unavailable; no retry was sent")?;
        let deadline = Instant::now() + Duration::from_secs(40);
        let stream = crate::control::connect_control_socket(endpoint, Duration::from_secs(5)).map_err(|error| error.to_string())?;
        let mut io = ReturnReplayIo { stream, deadline };
        let mut requests = FrameCodec::new(ipc::MAX_CLIENT_FRAME_BYTES);
        let mut responses = FrameCodec::new(ipc::MAX_FRAME_BYTES);
        requests.write(&mut io, &ClientEnvelope {
            version:PROTOCOL_VERSION, request_id:1, proof:None,
            request:ClientRequest::Connect {expected_host:Some(self.inner.proof.clone()), resume:Vec::new()},
        }).map_err(|error| error.to_string())?;
        match return_replay_response(&mut io, &mut responses, 1, &self.inner.proof)? {
            ServerResponse::Connected {proof, resumed} if proof == self.inner.proof && resumed.is_empty() => {},
            _ => return Err("The original Return daemon did not confirm its captured proof".into()),
        }
        requests.write(&mut io, &ClientEnvelope {
            version:PROTOCOL_VERSION, request_id:2, proof:Some(self.inner.proof.clone()),
            request:ClientRequest::ChatReturnTerminal {chat_id:self.inner.id, expected:expected.clone()},
        }).map_err(|error| error.to_string())?;
        // Control replay returns only the receipt. Final snapshots never
        // replace the old attachment's cached state or mark it alive again.
        let receipt = match return_replay_response(&mut io, &mut responses, 2, &self.inner.proof)? {
            ServerResponse::ChatTerminal {chat_id, session, receipt, ..}
                if chat_id == self.inner.id && session == receipt.terminal.session
                    && valid_handoff_receipt(&receipt, expected, &self.inner.proof) => receipt,
            _ => return Err("The original Return retry did not confirm its exact captured receipt".into()),
        };
        io.remaining().map_err(|error| error.to_string())?;
        let current = self.source_binding();
        if current.as_ref().is_some_and(|current| current != expected && receipt.retained_binding.as_ref() != Some(current)) {
            return Err("The original Return owner changed before its retry completed".into());
        }
        Ok(receipt)
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
    pub fn send(&self, command: Command) -> Result<(), String> {
        self.request(ClientRequest::ChatCommand {
            chat_id: self.inner.id,
            command,
        })
    }
    pub fn set_visible(&self, visible: bool) -> Result<(), String> {
        if !self.inner.alive.load(Ordering::Acquire) {
            return Err("Chat is disconnected".into());
        }
        let changed = self.inner.visible.swap(visible, Ordering::AcqRel) != visible;
        if visible && changed {
            // An idle daemon may have the same generation as our cached
            // handshake. Present that cache now; a conditional read can
            // legitimately return no update when a hidden pane reappears.
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

/// Retire metadata only after saving each COMPLETE host-issued receipt.
/// Missing/replaced daemons cannot retire an unknown operation: keep the
/// durable receipt so the caller can report or reconcile it explicitly.
pub fn acknowledge_terminal_handoff_receipts(
    receipts: &[crate::terminal_ipc::TerminalHandoffReceipt],
) -> Vec<Result<(), String>> {
    if receipts.is_empty() {
        return Vec::new();
    }
    let socket = match ipc::daemon_socket_path() {
        Ok(socket) => socket,
        Err(error) => return vec![Err(error.to_string()); receipts.len()],
    };
    acknowledge_terminal_handoff_receipts_on(
        crate::control::ControlClient::connect(&socket),
        receipts,
    )
}

fn acknowledge_terminal_handoff_receipts_on(
    connection: Result<crate::control::ControlClient, crate::control::ControlError>,
    receipts: &[crate::terminal_ipc::TerminalHandoffReceipt],
) -> Vec<Result<(), String>> {
    let mut client = match connection {
        Ok(client) => client,
        Err(error) => return vec![Err(error.to_string()); receipts.len()],
    };
    let mut connection_failure = None;
    receipts.iter().map(|receipt| {
        if receipt.source.proof.as_ref() != Some(client.proof()) {
            return Err("The exact daemon that issued this terminal handoff is unavailable; its receipt was retained".into());
        }
        if let Some(error) = &connection_failure { return Err(error.clone()); }
        match client.commit_terminal_handoff_receipt(receipt) {
            Ok(()) => Ok(()),
            Err(error) => {
                if !matches!(error, crate::control::ControlError::Remote { .. }) {
                    connection_failure = Some(error.to_string());
                }
                Err(error.to_string())
            }
        }
    }).collect()
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
        // Socket EOF unregisters the subscription, never stops the daemon's actor.
        self.alive.store(false, Ordering::Release);
        let _ = self.visibility_tx.try_send(());
        let _ = self.cancel.shutdown(Shutdown::Both);
        if let Some(reader) = self
            .reader
            .get_mut()
            .unwrap_or_else(|p| p.into_inner())
            .take()
            && reader.thread().id() != thread::current().id()
        {
            let _ = reader.join();
        }
        if let Some(worker) = self
            .visibility_worker
            .get_mut()
            .unwrap_or_else(|p| p.into_inner())
            .take()
            && worker.thread().id() != thread::current().id()
        {
            let _ = worker.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    include!("client_restart_tests.rs");

    #[test]
    fn authenticated_same_record_thread_refresh_preserves_captured_return_intents() {
        let proof = host_proof("live-thread-binding");
        let source = omp_card(Some("thread-a"), Some(proof.clone()));
        let instance = uuid::Uuid::new_v4();
        let a = crate::terminal_ipc::ChatSourceBinding {
            source: source.clone(),
            execution_instance: instance,
            backend: uuid::Uuid::new_v4(),
        };
        let mut b = a.clone();
        b.source.launch.session_id = Some("thread-b".into());
        b.backend = uuid::Uuid::new_v4();
        let (client, mut server) = UnixStream::pair().unwrap();
        let (advance, ready) = std::sync::mpsc::channel();
        let (finish, finished) = std::sync::mpsc::channel();
        let sent_a = a.clone();
        let sent_b = b.clone();
        let worker = thread::spawn(move || {
            server
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            server
                .set_write_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut codec = FrameCodec::new(ipc::MAX_FRAME_BYTES);
            let connect: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
            handoff_reply(
                &mut server,
                &mut codec,
                Some(connect.request_id),
                &proof,
                ServerMessage::Response {
                    response: ServerResponse::Connected {
                        proof: proof.clone(),
                        resumed: Vec::new(),
                    },
                },
            );
            let attach: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
            handoff_reply(
                &mut server,
                &mut codec,
                Some(attach.request_id),
                &proof,
                ServerMessage::Response {
                    response: ServerResponse::ChatAttached {
                        session: sent_a.source.clone(),
                        execution_instance: instance,
                        binding: Some(sent_a.clone()),
                        snapshot: Snapshot {
                            generation: 1,
                            session_id: Some("thread-a".into()),
                            ..Default::default()
                        },
                    },
                },
            );
            ready.recv_timeout(Duration::from_secs(5)).unwrap();
            loop {
                let visibility: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
                if matches!(
                    visibility.request,
                    ClientRequest::ChatVisibility { visible: true, .. }
                ) {
                    break;
                }
            }
            handoff_reply(
                &mut server,
                &mut codec,
                None,
                &proof,
                ServerMessage::ChatChanged {
                    chat_id: sent_a.source.id,
                },
            );
            let read: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
            assert!(matches!(read.request, ClientRequest::ChatRead { .. }));
            handoff_reply(
                &mut server,
                &mut codec,
                Some(read.request_id),
                &proof,
                ServerMessage::Response {
                    response: ServerResponse::ChatSnapshot {
                        chat_id: sent_a.source.id,
                        execution_instance: instance,
                        binding: Some(sent_b.clone()),
                        snapshot: Some(Snapshot {
                            generation: 2,
                            session_id: Some("thread-b".into()),
                            ..Default::default()
                        }),
                    },
                },
            );
            for (expected, reason) in [
                (sent_a, "old captured intent refused"),
                (sent_b, "new captured intent reached host"),
            ] {
                let request: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
                assert!(
                    matches!(request.request, ClientRequest::ChatReturnTerminal { expected: ref actual, .. } if *actual == expected)
                );
                handoff_reply(
                    &mut server,
                    &mut codec,
                    Some(request.request_id),
                    &proof,
                    ServerMessage::Error(ipc::ProtocolError::new(
                        ipc::ProtocolErrorCode::InvalidRequest,
                        reason,
                        false,
                    )),
                );
            }
            finished.recv_timeout(Duration::from_secs(5)).unwrap();
        });
        let handle = ClientHandle::connect_on(
            client,
            source,
            Intent::Start,
            None,
            false,
            None,
            None,
            |_| {},
        )
        .unwrap();
        assert_eq!(handle.source_binding(), Some(a.clone()));
        handle.set_visible(true).unwrap();
        advance.send(()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while handle.source_binding() != Some(b.clone())
            || handle.session().launch.session_id.as_deref() != Some("thread-b")
        {
            assert!(
                Instant::now() < deadline,
                "same Record refresh did not publish B"
            );
            thread::yield_now();
        }
        assert!(handle.inner.alive.load(Ordering::Acquire));
        assert_eq!(
            handle.return_terminal(a).unwrap_err(),
            "old captured intent refused"
        );
        assert_eq!(
            handle.return_terminal(b).unwrap_err(),
            "new captured intent reached host"
        );
        assert!(handle.inner.alive.load(Ordering::Acquire));
        assert!(handle.snapshot().error.is_none());
        finish.send(()).unwrap();
        worker.join().unwrap();
    }

    #[test]
    fn handled_command_refusals_do_not_become_shared_snapshot_errors() {
        for initial_error in [None, Some("previous unsolicited error".to_owned())] {
            let (client, mut server) = UnixStream::pair().unwrap();
            let seed = initial_error.clone();
            let (advance_send, advance_receive) = std::sync::mpsc::channel();
            let (done_send, done_receive) = std::sync::mpsc::channel();
            let worker = thread::spawn(move || {
                server
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                server
                    .set_write_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let proof = host_proof("live");
                let mut codec = FrameCodec::new(ipc::MAX_FRAME_BYTES);
                let connect: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
                handoff_reply(
                    &mut server,
                    &mut codec,
                    Some(connect.request_id),
                    &proof,
                    ServerMessage::Response {
                        response: ServerResponse::Connected {
                            proof: proof.clone(),
                            resumed: Vec::new(),
                        },
                    },
                );
                let start: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
                let ClientRequest::ChatStart { session, .. } = start.request else {
                    panic!("expected chat start")
                };
                handoff_reply(
                    &mut server,
                    &mut codec,
                    Some(start.request_id),
                    &proof,
                    ServerMessage::Response {
                        response: ServerResponse::ChatAttached {
                            execution_instance: uuid::Uuid::from_u128(1),
                            binding: None,
                            session,
                            snapshot: Snapshot {
                                generation: 1,
                                error: seed,
                                ..Default::default()
                            },
                        },
                    },
                );
                let command: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
                assert!(matches!(command.request, ClientRequest::ChatCommand { .. }));
                handoff_reply(
                    &mut server,
                    &mut codec,
                    Some(command.request_id),
                    &proof,
                    ServerMessage::Error(crate::terminal_ipc::ProtocolError::new(
                        crate::terminal_ipc::ProtocolErrorCode::InvalidRequest,
                        "Command was refused",
                        false,
                    )),
                );
                advance_receive
                    .recv_timeout(Duration::from_secs(5))
                    .unwrap();
                handoff_reply(
                    &mut server,
                    &mut codec,
                    None,
                    &proof,
                    ServerMessage::Error(crate::terminal_ipc::ProtocolError::new(
                        crate::terminal_ipc::ProtocolErrorCode::InvalidRequest,
                        "Unsolicited provider failure",
                        false,
                    )),
                );
                done_receive.recv_timeout(Duration::from_secs(5)).unwrap();
            });
            let (wake_send, wake_receive) = std::sync::mpsc::channel();
            let handle = ClientHandle::connect_on(
                client,
                omp_card(None, None),
                Intent::Start,
                None,
                false,
                None,
                None,
                move |wake| {
                    let _ = wake_send.send(wake);
                },
            )
            .unwrap();
            assert_eq!(
                handle.send(Command::Compact).unwrap_err(),
                "Command was refused"
            );
            assert_eq!(handle.snapshot().error, initial_error);
            advance_send.send(()).unwrap();
            while handle.snapshot().error.as_deref() != Some("Unsolicited provider failure") {
                wake_receive.recv_timeout(Duration::from_secs(5)).unwrap();
            }
            done_send.send(()).unwrap();
            worker.join().unwrap();
        }
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
    fn completed_handoff_retains_exact_operation_proof_without_a_live_continuity_claim() {
        let terminal = handoff_terminal();
        let source = Session { archive_report: None,
            id: TerminalId::new(),
            proof: Some(handoff_proof()),
            launch: crate::chat::Launch {
                provider: crate::agent::Provider::Omp,
                recipe: terminal.launch.clone(),
                session_id: Some("original".into()),
            },
            terminal: None,
            native_continuity: Default::default(),
            owned_codex_listener: None,
        };
        let binding = crate::terminal_ipc::ChatSourceBinding {
            source: source.clone(),
            execution_instance: uuid::Uuid::new_v4(),
            backend: uuid::Uuid::new_v4(),
        };
        let mut descriptor = crate::terminal_ipc::TerminalDescriptor::relaunchable(
            terminal.id,
            terminal.launch,
            None,
        );
        descriptor.execution_instance = uuid::Uuid::new_v4();
        let mut receipt = crate::terminal_ipc::TerminalHandoffReceipt {
            operation: uuid::Uuid::new_v4(),
            source,
            source_execution_instance: binding.execution_instance,
            source_binding: binding.clone(),
            retained_binding: None,
            terminal_native_epoch: uuid::Uuid::new_v4(),
            terminal: descriptor,
        };
        assert!(receipt.terminal.session.continuity.live_proof().is_none());
        assert!(valid_handoff_receipt(&receipt, &binding, &handoff_proof()));
        receipt.terminal.execution_instance = uuid::Uuid::nil();
        assert!(!valid_handoff_receipt(&receipt, &binding, &handoff_proof()));
        receipt.terminal.execution_instance = uuid::Uuid::new_v4();
        receipt.source.launch.session_id = Some("another".into());
        assert!(!valid_handoff_receipt(&receipt, &binding, &handoff_proof()));
    }

    #[derive(Clone, Copy)]
    enum ReaderCase { Completed, Shared, Late, Backend, Actor, Thread, ForeignProof, WrongKind, CurrentConflict, CurrentNone, Close }

    // In-process framed peer only: finite socket/latch/reply deadlines and an
    // armed socket shutdown guard cover assertion paths. No child/provider or
    // daemon is started, and no raw pthread join can block the fixture caller.
    // The strict outer test guardian separately proves final thread quiescence.
    struct ReaderPeer {
        socket: UnixStream,
        release: crossbeam_channel::Sender<()>,
        done: crossbeam_channel::Receiver<bool>,
        settled: bool,
        workers: Vec<JoinHandle<()>>,
    }
    impl Drop for ReaderPeer {
        fn drop(&mut self) {
            let _ = self.release.try_send(());
            let _ = self.socket.shutdown(Shutdown::Both);
            if !self.settled && self.done.recv_timeout(Duration::from_secs(6)) != Ok(true) {
                eprintln!("Framed handoff peer did not acknowledge shutdown; outer guardian retains quiescence obligation");
            }
            // JoinHandle destruction detaches; a bounded fixture never calls
            // join based only on is_finished or a message from before TLS exit.
            self.workers.clear();
        }
    }
    fn reader_case(case: ReaderCase) {
        let proof = handoff_proof();
        let source = omp_card(Some("original"), Some(proof.clone()));
        let binding = crate::terminal_ipc::ChatSourceBinding {
            source: source.clone(), execution_instance: uuid::Uuid::new_v4(), backend: uuid::Uuid::new_v4(),
        };
        let terminal = handoff_terminal();
        let mut descriptor = crate::terminal_ipc::TerminalDescriptor::relaunchable(terminal.id, terminal.launch.clone(), None);
        descriptor.execution_instance = uuid::Uuid::new_v4();
        let receipt = crate::terminal_ipc::TerminalHandoffReceipt {
            operation: uuid::Uuid::new_v4(), source: source.clone(), source_execution_instance: binding.execution_instance,
            source_binding: binding.clone(), retained_binding: None, terminal_native_epoch: uuid::Uuid::new_v4(), terminal: descriptor,
        };
        assert!(valid_handoff_receipt(&receipt, &binding, &proof));
        assert!(receipt.terminal.session.continuity.live_proof().is_none());
        let (client, mut server) = UnixStream::pair().unwrap();
        let socket = server.try_clone().unwrap();
        let (release, released) = crossbeam_channel::bounded(1);
        let (done, completed) = crossbeam_channel::bounded(1);
        let (wake, wakes) = crossbeam_channel::unbounded();
        let sent_binding = binding.clone(); let sent_receipt = receipt.clone(); let sent_proof = proof.clone();
        let worker = thread::spawn(move || {
            struct Done { send: crossbeam_channel::Sender<bool>, success: bool }
            impl Drop for Done { fn drop(&mut self) { let _ = self.send.try_send(self.success); } }
            let mut completion = Done { send: done, success: false };
            server.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            server.set_write_timeout(Some(Duration::from_secs(5))).unwrap();
            let mut codec = FrameCodec::new(ipc::MAX_FRAME_BYTES);
            let connect: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
            handoff_reply(&mut server, &mut codec, Some(connect.request_id), &sent_proof,
                ServerMessage::Response { response: ServerResponse::Connected { proof: sent_proof.clone(), resumed: vec![] } });
            let attach: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
            handoff_reply(&mut server, &mut codec, Some(attach.request_id), &sent_proof,
                ServerMessage::Response { response: ServerResponse::ChatAttached {
                    session: sent_binding.source.clone(), execution_instance: sent_binding.execution_instance,
                    binding: Some(sent_binding.clone()), snapshot: Snapshot { generation: 1, session_id: Some("original".into()), ..Default::default() },
                } });
            let request = loop {
                let request: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
                if matches!(request.request, ClientRequest::ChatReturnTerminal {..} | ClientRequest::ChatCommand {..} | ClientRequest::ChatClose {..}) { break request; }
                handoff_reply(&mut server, &mut codec, Some(request.request_id), &sent_proof,
                    ServerMessage::Response { response: ServerResponse::Accepted });
            };
            if matches!(case, ReaderCase::Late) {
                handoff_reply(&mut server, &mut codec, Some(request.request_id + 1000), &sent_proof,
                    ServerMessage::Response { response: ServerResponse::ChatTerminal {
                        chat_id: sent_binding.source.id, session: sent_receipt.terminal.session.clone(), receipt: sent_receipt.clone(),
                        snapshot: Snapshot { generation: 99, exited: true, ..Default::default() },
                    } });
                handoff_reply(&mut server, &mut codec, None, &sent_proof,
                    ServerMessage::ChatActivity { chat_id: sent_binding.source.id, state: ActivityState::Waiting, generation: 10, outcome: None });
                released.recv_timeout(Duration::from_secs(5)).unwrap();
            }
            if matches!(case, ReaderCase::CurrentConflict | ReaderCase::CurrentNone) {
                let mut newer = sent_binding.clone(); newer.backend = uuid::Uuid::new_v4();
                handoff_reply(&mut server, &mut codec, None, &sent_proof,
                    ServerMessage::Response { response: ServerResponse::ChatSnapshot {
                        chat_id: sent_binding.source.id, execution_instance: sent_binding.execution_instance,
                        binding: matches!(case, ReaderCase::CurrentConflict).then_some(newer),
                        snapshot: Some(Snapshot { generation: 2, session_id: Some("original".into()), ..Default::default() }),
                    } });
            }
            let mut response = sent_receipt;
            match case {
                ReaderCase::Backend => response.source_binding.backend = uuid::Uuid::new_v4(),
                ReaderCase::Actor => { response.source_execution_instance = uuid::Uuid::new_v4(); response.source_binding.execution_instance = response.source_execution_instance; },
                ReaderCase::Thread => response.source.launch.session_id = Some("another".into()),
                ReaderCase::ForeignProof => response.terminal.session.continuity = crate::model::TerminalContinuity::DetachedSameBoot {
                    host_session_id: sent_proof.host_session_id.clone(), boot_id: sent_proof.boot_id.clone(),
                    host_incarnation: crate::model::HostIncarnation::new("another"),
                },
                _ => {},
            }
            let snapshot = Snapshot { generation: 77, session_id: Some("original".into()), exited: !matches!(case, ReaderCase::Shared), ..Default::default() };
            let response = if matches!(case, ReaderCase::Close) {
                ServerResponse::ChatClosed { chat_id: sent_binding.source.id, snapshot }
            } else { ServerResponse::ChatTerminal { chat_id: sent_binding.source.id, session: response.terminal.session.clone(), receipt: response, snapshot } };
            handoff_reply(&mut server, &mut codec, Some(request.request_id), &sent_proof, ServerMessage::Response { response });
            if matches!(case, ReaderCase::Shared) {
                loop {
                    let request: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
                    let command = matches!(request.request, ClientRequest::ChatCommand { .. });
                    handoff_reply(&mut server, &mut codec, Some(request.request_id), &sent_proof,
                        ServerMessage::Response { response: ServerResponse::Accepted });
                    if command { break; }
                }
                let _ = released.recv_timeout(Duration::from_secs(5));
            }
            completion.success = true;
        });
        let mut peer = ReaderPeer { socket, release, done: completed, settled: false, workers: vec![worker] };
        let handle = ClientHandle::connect_on(client, source, Intent::Restore, Some(proof), false, None, None,
            move |event| { let _ = wake.send(event); }).unwrap();
        assert_eq!(handle.source_binding(), Some(binding.clone()));
        let requester = handle.clone(); let expected = binding.clone();
        let (result, reply) = crossbeam_channel::bounded(1);
        peer.workers.push(thread::spawn(move || {
            let response = match case {
                ReaderCase::WrongKind => requester.request(ClientRequest::ChatCommand { chat_id: expected.source.id,
                    command: Command::SetModel { model: "original".into(), reasoning: None } }).map(|()|None),
                ReaderCase::Close => requester.request(ClientRequest::ChatClose { chat_id: expected.source.id }).map(|()|None),
                _ => requester.return_terminal(expected).map(Some),
            };
            let _ = result.try_send(response);
        }));
        if matches!(case, ReaderCase::Late) {
            let deadline = Instant::now() + Duration::from_secs(3);
            loop {
                match wakes.recv_timeout(deadline.saturating_duration_since(Instant::now())).unwrap() {
                    Wake::Activity => break,
                    Wake::Disconnected(reason) => panic!("uncorrelated final payload must not close the actor: {reason}"),
                    _ => {},
                }
            }
            assert_eq!(handle.snapshot().generation, 1, "late response cannot publish its final payload");
            assert!(handle.inner.alive.load(Ordering::Acquire));
            peer.release.try_send(()).unwrap();
        }
        let result = reply.recv_timeout(Duration::from_secs(5)).expect("actual reader must resolve the matched request");
        if matches!(case, ReaderCase::Backend | ReaderCase::Actor | ReaderCase::Thread | ReaderCase::ForeignProof | ReaderCase::WrongKind | ReaderCase::CurrentConflict) {
            let error = result.expect_err("invalid receipt must not become a return success");
            let expected = match case {
                ReaderCase::WrongKind => "another pending command",
                ReaderCase::CurrentConflict => "backend changed before terminal handoff publication",
                _ => "inconsistent terminal handoff authority",
            };
            assert!(error.contains(expected), "protocol admission must refuse before payload effects, not succeed through peer EOF: {error}");
            assert_eq!(handle.snapshot().generation, if matches!(case, ReaderCase::CurrentConflict) { 2 } else { 1 }, "invalid final payload must not replace the current snapshot");
        } else {
            assert_eq!(result.unwrap(), if matches!(case, ReaderCase::Close) { None } else { Some(receipt) });
            assert_eq!(handle.snapshot().generation, 77);
            assert_eq!(handle.inner.alive.load(Ordering::Acquire), matches!(case, ReaderCase::Shared));
            if matches!(case, ReaderCase::Shared) {
                handle.request(ClientRequest::ChatCommand { chat_id: binding.source.id,
                    command: Command::SetModel { model: "still-alive".into(), reasoning: None } }).unwrap();
                assert!(handle.inner.alive.load(Ordering::Acquire));
            }
        }
        let _ = peer.release.try_send(());
        assert!(peer.done.recv_timeout(Duration::from_secs(6)).expect("finite actual peer completion"), "the actual peer must finish its complete scripted exchange without panic");
        peer.settled = true;
    }

    #[test]
    fn handoff_reader_accepts_completed_receipt_without_live_continuity() { reader_case(ReaderCase::Completed); reader_case(ReaderCase::CurrentNone); }
    #[test]
    fn handoff_reader_refuses_wrong_source_actor_and_request_kind_before_final_payload() {
        for case in [ReaderCase::Backend, ReaderCase::Actor, ReaderCase::Thread, ReaderCase::ForeignProof, ReaderCase::WrongKind, ReaderCase::CurrentConflict] { reader_case(case); }
    }
    #[test]
    fn handoff_reader_ignores_uncorrelated_final_payload_until_exact_request() { reader_case(ReaderCase::Late); }
    #[test]
    fn handoff_reader_preserves_shared_server_and_later_command_admission() { reader_case(ReaderCase::Shared); }
    #[test]
    fn handoff_reader_preserves_correlated_chat_close() { reader_case(ReaderCase::Close); }

    #[derive(Clone, Copy)]
    enum ReplayCase { Exact, CanonicalPair, ForeignProof, WrongBackend, Refused }
    fn disconnected_return_replay(case: ReplayCase) {
        use std::os::unix::net::UnixListener;
        let directory = tempfile::tempdir().unwrap();
        let endpoint = directory.path().join("original.sock");
        let listener = UnixListener::bind(&endpoint).unwrap();
        listener.set_nonblocking(true).unwrap();
        let proof = handoff_proof();
        let source = omp_card(Some("original"), Some(proof.clone()));
        let binding = ipc::ChatSourceBinding {source:source.clone(), execution_instance:uuid::Uuid::new_v4(), backend:uuid::Uuid::new_v4()};
        let mut descriptor = ipc::TerminalDescriptor::relaunchable(if matches!(case,ReplayCase::CanonicalPair) {source.id} else {TerminalId::new()}, handoff_terminal().launch, None);
        descriptor.execution_instance = uuid::Uuid::new_v4();
        let mut receipt = ipc::TerminalHandoffReceipt {operation:uuid::Uuid::new_v4(), source:source.clone(), source_execution_instance:binding.execution_instance,
            source_binding:binding.clone(), retained_binding:None, terminal_native_epoch:uuid::Uuid::new_v4(), terminal:descriptor};
        if matches!(case,ReplayCase::CanonicalPair) {
            let mut canonical=binding.clone();canonical.backend=uuid::Uuid::new_v4();
            canonical.source.terminal=Some(receipt.terminal.session.clone());
            receipt.retained_binding=Some(canonical);
        }
        let sockets = Arc::new(Mutex::new(Vec::<UnixStream>::new()));
        let abort = Arc::new(AtomicBool::new(false));
        let (done, completed) = crossbeam_channel::bounded(1);
        struct Peer { sockets:Arc<Mutex<Vec<UnixStream>>>, abort:Arc<AtomicBool>, done:crossbeam_channel::Receiver<bool>, settled:bool }
        impl Drop for Peer {
            fn drop(&mut self) {
                self.abort.store(true, Ordering::Release);
                for socket in self.sockets.lock().unwrap_or_else(|p|p.into_inner()).iter() {let _ = socket.shutdown(Shutdown::Both);}
                if !self.settled {let _ = self.done.recv_timeout(Duration::from_secs(6));}
            }
        }
        let mut peer = Peer {sockets:sockets.clone(), abort:abort.clone(), done:completed, settled:false};
        let original = binding.clone(); let sent_receipt = receipt.clone(); let sent_proof = proof.clone();
        let worker = thread::spawn(move || {
            struct Done {sender:crossbeam_channel::Sender<bool>, success:bool}
            impl Drop for Done {fn drop(&mut self) {let _ = self.sender.try_send(self.success);}}
            let mut finished = Done {sender:done, success:false};
            let accept = || {
                let deadline = Instant::now() + Duration::from_secs(5);
                loop {match listener.accept() {
                    Ok((stream, _)) => {
                        stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
                        stream.set_write_timeout(Some(Duration::from_secs(5))).unwrap();
                        sockets.lock().unwrap().push(stream.try_clone().unwrap());
                        return stream;
                    },
                    Err(error) if error.kind()==io::ErrorKind::WouldBlock => {
                        assert!(!abort.load(Ordering::Acquire) && Instant::now()<deadline, "bounded original peer accept");
                        thread::sleep(Duration::from_millis(2));
                    }, Err(error) => panic!("private peer accept: {error}"),
                }}
            };
            let mut first = accept(); let mut codec = FrameCodec::new(ipc::MAX_FRAME_BYTES);
            let connect:ClientEnvelope = codec.read(&mut first).unwrap().unwrap();
            handoff_reply(&mut first, &mut codec, Some(connect.request_id), &sent_proof,
                ServerMessage::Response {response:ServerResponse::Connected {proof:sent_proof.clone(), resumed:vec![]}});
            let attach:ClientEnvelope = codec.read(&mut first).unwrap().unwrap();
            assert!(matches!(attach.request, ClientRequest::ChatStart {..}));
            handoff_reply(&mut first, &mut codec, Some(attach.request_id), &sent_proof,
                ServerMessage::Response {response:ServerResponse::ChatAttached {
                    session:original.source.clone(), execution_instance:original.execution_instance, binding:Some(original.clone()),
                    snapshot:Snapshot {generation:1, session_id:Some("original".into()), ..Default::default()},
                }});
            let submitted = loop {
                let frame:ClientEnvelope = codec.read(&mut first).unwrap().unwrap();
                if matches!(frame.request, ClientRequest::ChatVisibility {..}) {continue;}
                break frame;
            };
            assert!(matches!(submitted.request, ClientRequest::ChatReturnTerminal {expected, ..} if expected==original));
            if let Some(canonical)=sent_receipt.retained_binding.as_ref() {
                handoff_reply(&mut first,&mut codec,None,&sent_proof,ServerMessage::Response {response:ServerResponse::ChatSnapshot {
                    chat_id:original.source.id,execution_instance:original.execution_instance,binding:Some(canonical.clone()),
                    snapshot:Some(Snapshot {generation:2,session_id:Some("original".into()),..Default::default()}),
                }});
            }
            // Model the response-loss seam, not provider/family cleanup proof.
            // Shutdown all duplicate descriptors for this original connection.
            first.shutdown(Shutdown::Both).unwrap();
            drop(first);
            let mut retry = accept(); let mut codec = FrameCodec::new(ipc::MAX_FRAME_BYTES);
            let connect:ClientEnvelope = codec.read(&mut retry).unwrap().unwrap();
            assert!(matches!(connect.request, ClientRequest::Connect {expected_host:Some(ref expected), ref resume} if *expected==sent_proof && resume.is_empty()));
            assert!(connect.proof.is_none());
            let response_proof = if matches!(case,ReplayCase::ForeignProof) {host_proof("replacement-daemon")} else {sent_proof.clone()};
            handoff_reply(&mut retry, &mut codec, Some(connect.request_id), &response_proof,
                ServerMessage::Response {response:ServerResponse::Connected {proof:response_proof.clone(), resumed:vec![]}});
            if matches!(case,ReplayCase::ForeignProof) {
                assert!(codec.read::<ClientEnvelope>(&mut retry).unwrap().is_none(), "proof mismatch must send no Return/Attach/List");
                finished.success=true;return;
            }
            let request:ClientEnvelope = codec.read(&mut retry).unwrap().unwrap();
            assert_eq!(request.proof, Some(sent_proof.clone()));
            assert!(matches!(request.request,ClientRequest::ChatReturnTerminal {chat_id, expected} if chat_id==original.source.id && expected==original), "retry must send ONLY original captured intent, without Attach/List");
            let message = if matches!(case,ReplayCase::Refused) {
                ServerMessage::Error(ipc::ProtocolError::new(ipc::ProtocolErrorCode::InvalidRequest,"exact original Return refused",false))
            } else {
                let mut receipt=sent_receipt;
                if matches!(case,ReplayCase::WrongBackend) {receipt.source_binding.backend=uuid::Uuid::new_v4();}
                ServerMessage::Response {response:ServerResponse::ChatTerminal {chat_id:original.source.id,
                    session:receipt.terminal.session.clone(), receipt,
                    snapshot:Snapshot {generation:77, exited:true, ..Default::default()}}}
            };
            handoff_reply(&mut retry, &mut codec, Some(request.request_id), &sent_proof, message);
            finished.success=true;
        });
        let client = UnixStream::connect(&endpoint).unwrap();
        let handle = ClientHandle::connect_on(client, source, Intent::Start, None, false, None, None, |_|{}).unwrap();
        assert_eq!(handle.inner.return_endpoint.as_ref(), Some(&endpoint));
        assert!(handle.return_terminal(binding.clone()).is_err(), "first request has no receipt");
        let deadline=Instant::now()+Duration::from_secs(2);
        while handle.inner.alive.load(Ordering::Acquire) {assert!(Instant::now()<deadline);thread::yield_now();}
        if matches!(case,ReplayCase::CanonicalPair) {
            assert_eq!(handle.source_binding(),receipt.retained_binding,"the actual old reader must have accepted canonical post-pair metadata before EOF");
            assert_eq!(handle.snapshot().generation,2);
        }
        let result=handle.return_terminal(binding);
        match case {
            ReplayCase::Exact|ReplayCase::CanonicalPair => assert_eq!(result.unwrap(),receipt),
            ReplayCase::ForeignProof => assert!(result.unwrap_err().contains("proof")),
            ReplayCase::WrongBackend => assert!(result.unwrap_err().contains("exact captured receipt")),
            ReplayCase::Refused => assert_eq!(result.unwrap_err(),"exact original Return refused"),
        }
        assert!(!handle.inner.alive.load(Ordering::Acquire), "control replay cannot revive old attachment");
        assert_ne!(handle.snapshot().generation,77, "replayed final payload is not installed");
        assert_eq!(peer.done.recv_timeout(Duration::from_secs(6)),Ok(true),"actual bounded peer script must finish");
        peer.settled=true;drop(worker);
    }
    #[test]
    fn disconnected_return_replays_only_original_endpoint_proof_and_intent() {
        disconnected_return_replay(ReplayCase::Exact);
        disconnected_return_replay(ReplayCase::CanonicalPair);
    }
    #[test]
    fn disconnected_return_replay_refuses_changed_proof_backend_and_host_refusal() {
        disconnected_return_replay(ReplayCase::ForeignProof);
        disconnected_return_replay(ReplayCase::WrongBackend);
        disconnected_return_replay(ReplayCase::Refused);
    }
    #[test]
    fn original_return_replay_deadline_is_not_renewed_by_partial_frame_bytes() {
        let (client, mut server)=UnixStream::pair().unwrap();
        let cancel=server.try_clone().unwrap();
        let (done, completed)=crossbeam_channel::bounded(1);
        let worker=thread::spawn(move || {
            for byte in [0,0,0,16,0,0,0,0] {
                if io::Write::write_all(&mut server,&[byte]).is_err() {break;}
                thread::sleep(Duration::from_millis(15));
            }
            let _=done.send(());
        });
        let start=Instant::now();
        let mut io=ReturnReplayIo {stream:client,deadline:start+Duration::from_millis(40)};
        let result=FrameCodec::new(ipc::MAX_FRAME_BYTES).read::<ServerEnvelope>(&mut io);
        assert!(result.is_err());
        assert!(start.elapsed()<Duration::from_millis(500));
        let _=cancel.shutdown(Shutdown::Both);
        assert!(completed.recv_timeout(Duration::from_secs(1)).is_ok());
        drop(worker);
    }

    #[test]
    fn typed_handoff_ack_transmits_the_complete_operation_and_preserves_refusals() {
        let terminal = handoff_terminal();
        let source = Session { archive_report: None,
            id: TerminalId::new(),
            proof: Some(handoff_proof()),
            launch: crate::chat::Launch {
                provider: crate::agent::Provider::Omp,
                recipe: terminal.launch.clone(),
                session_id: Some("original".into()),
            },
            terminal: None,
            native_continuity: Default::default(),
            owned_codex_listener: None,
        };
        let binding = crate::terminal_ipc::ChatSourceBinding {
            source: source.clone(),
            execution_instance: uuid::Uuid::new_v4(),
            backend: uuid::Uuid::new_v4(),
        };
        let mut descriptor = crate::terminal_ipc::TerminalDescriptor::relaunchable(
            terminal.id,
            terminal.launch,
            None,
        );
        descriptor.execution_instance = uuid::Uuid::new_v4();
        let a = crate::terminal_ipc::TerminalHandoffReceipt {
            operation: uuid::Uuid::new_v4(),
            source,
            source_execution_instance: binding.execution_instance,
            source_binding: binding,
            retained_binding: None,
            terminal_native_epoch: uuid::Uuid::new_v4(),
            terminal: descriptor,
        };
        let mut b = a.clone();
        b.operation = uuid::Uuid::new_v4();
        b.terminal.execution_instance = uuid::Uuid::new_v4();
        let receipts = vec![a, b];
        let expected = receipts.clone();
        let (client, mut server) = UnixStream::pair().unwrap();
        struct Completed(std::sync::mpsc::Sender<()>);
        impl Drop for Completed {
            fn drop(&mut self) {
                let _ = self.0.send(());
            }
        }
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let worker = thread::spawn(move || {
            let _completed = Completed(done_tx);
            let mut server = server;
            server
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            server
                .set_write_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut codec = FrameCodec::new(ipc::MAX_FRAME_BYTES);
            handoff_handshake(&mut server, &mut codec);
            for (index, expected) in expected.into_iter().enumerate() {
                let request: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
                assert_eq!(
                    request.request,
                    ClientRequest::ChatCommitTerminalReceipt { receipt: expected }
                );
                let message = if index == 0 {
                    ServerMessage::Error(crate::terminal_ipc::ProtocolError::new(
                        crate::terminal_ipc::ProtocolErrorCode::InvalidRequest,
                        "exact receipt refused",
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
        });
        let result = acknowledge_terminal_handoff_receipts_on(
            crate::control::ControlClient::from_stream(client),
            &receipts,
        );
        let completed = done_rx.recv_timeout(Duration::from_secs(6));
        drop(worker);
        assert!(completed.is_ok(), "finite framed peer did not finish");
        assert!(
            result[0]
                .as_ref()
                .unwrap_err()
                .contains("exact receipt refused")
        );
        assert!(result[1].is_ok());
        assert!(
            acknowledge_terminal_handoff_receipts_on(
                Err(crate::control::ControlError::ConnectionClosed),
                &receipts
            )
            .iter()
            .all(Result::is_err)
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
            outcome: None,
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
            archive_report: None,
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
    fn only_a_previously_named_upgrade_rotation_carries_persistence_provenance() {
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
            assert!(
                matches!(outcome, Ok(ClientRequest::ChatAttach { session }) if session == resolved)
            );
            assert_eq!(
                recovery,
                previous
                    .filter(|id| *id != CONVERSATION)
                    .map(|_| (source, resolved))
            );
        }
        let source = omp_card(Some("previous-conversation"), Some(host_proof("previous")));
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

        // A replacement for a different chat proves nothing about this one.
        // The conversation identifier deliberately is not part of that
        // comparison: the daemon reports the conversation its restarted writer
        // actually holds, which is the answer being asked for.
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

    fn restore_with_lineage(
        session: Session,
        resolved: Option<Session>,
        answer: impl FnOnce(ClientRequest) -> ServerMessage + Send + 'static,
    ) -> Result<ClientHandle, StartFailure> {
        use crate::terminal_ipc::{ProtocolError, ProtocolErrorCode};
        let live = host_proof("live");
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
            |_| {},
        );
        worker.join().unwrap();
        result
    }

    #[test]
    fn upgrade_provenance_requires_the_exact_attached_descriptor_and_confirmation() {
        for mismatch in 0..3 {
            let source = omp_card(Some("prior-conversation"), Some(host_proof("prior")));
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
                        response: ServerResponse::ChatAttached {
                            execution_instance: uuid::Uuid::from_u128(1),
                            binding: None,
                            session,
                            snapshot,
                        },
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
                        execution_instance: uuid::Uuid::from_u128(1),
                        binding: None,
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
                        execution_instance: uuid::Uuid::from_u128(1),
                        binding: None,
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
                    response: ServerResponse::ChatUpdate {
                        execution_instance: uuid::Uuid::from_u128(1),
                        binding: None,
                        chat_id,
                        update,
                    },
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
                        execution_instance: uuid::Uuid::from_u128(1),
                        binding: None,
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
                        execution_instance: uuid::Uuid::from_u128(1),
                        binding: None,
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
                        execution_instance: uuid::Uuid::from_u128(1),
                        binding: None,
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
                            execution_instance: uuid::Uuid::from_u128(1),
                            binding: None,
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
}
