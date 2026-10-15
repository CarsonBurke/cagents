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

struct Inner {
    id: TerminalId,
    session: Session,
    upgrade_recovery: Option<(Session, Session)>,
    proof: HostSessionProof,
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
        Self {
            inner: Arc::new(Inner {
                id: session.id,
                session,
                upgrade_recovery: None,
                proof,
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
                visibility_tx,
                visibility_worker: Mutex::new(None),
            }),
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
        inner.upgrade_recovery = Some((source, session));
        handle
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
            Some((source, None, false)),
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
            Some((source, Some(generation), false)),
            wake,
        )
    }

    pub fn switch_account_explicit(
        source: Session,
        replacement: Session,
        generation: Option<u64>,
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
            Some((source, generation, true)),
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
        account_recovery: Option<(Session, Option<u64>, bool)>,
        wake: impl Fn(Wake) + Send + Sync + 'static,
    ) -> Result<Self, StartFailure> {
        let wake: Arc<dyn Fn(Wake) + Send + Sync> = Arc::new(wake);
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
        let request = if let Some((source, idle_generation, explicit)) = account_recovery {
            let execution_environment = native_environment(&session, true);
            ClientRequest::ChatRecoverAccount {
                source,
                replacement: session,
                idle_generation,
                explicit,
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
                    return Err(StartFailure {
                        message: error.message,
                        recovery: if error.code == ipc::ProtocolErrorCode::AccountRemoved {
                            super::Recovery::Account
                        } else if error.code == ipc::ProtocolErrorCode::AccountSwitchBusy {
                            super::Recovery::AccountSwitchBusy
                        } else {
                            super::Recovery::Retry
                        },
                    });
                }
                _ => return Err("Daemon did not attach the exact native chat".into()),
            }
        };
        if let Some((_, resolved)) = &upgrade_recovery
            && (&session != resolved || initial.session_id != resolved.launch.session_id)
        {
            return Err(
                "The attached conversation did not confirm the daemon's exact upgrade replacement"
                    .into(),
            );
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
            pending: false,
        };
        if let Some((generation, state)) = early_activity {
            activity.observe(generation, state);
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
        let pending: Pending = Arc::new(Mutex::new(BTreeMap::new()));
        let file_pending: FilePending = Arc::new(Mutex::new(BTreeMap::new()));
        let (visibility_tx, visibility_rx) = crossbeam_channel::bounded(1);
        let inner = Arc::new(Inner {
            id,
            session,
            upgrade_recovery,
            proof: proof.clone(),
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
                    let (chat_id, final_snapshot, terminal) = match envelope.message {
                        ServerMessage::Response { response: ServerResponse::ChatClosed { chat_id, snapshot } } => (chat_id, snapshot, None),
                        ServerMessage::Response { response: ServerResponse::ChatTerminal { chat_id, session, snapshot } } => (chat_id, snapshot, Some(session)),
                        _ => unreachable!(),
                    };
                    if chat_id!=id { break "Daemon closed a different chat".into(); }
                    if terminal.is_some() && !final_snapshot.exited {
                        // A shared-server CLI is another view, not a closed
                        // native actor. Keep this connection and pending work.
                        *snapshot.lock().unwrap_or_else(|p|p.into_inner())=Arc::new(final_snapshot);
                        if let Some(reply) = envelope.request_id.and_then(|id| pending.lock().unwrap_or_else(|p|p.into_inner()).remove(&id)) {
                            let _ = reply.try_send(Ok(terminal));
                        }
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
                    let replies=std::mem::take(&mut *pending.lock().unwrap_or_else(|p|p.into_inner()));
                    for (request,reply) in replies {
                        let _=reply.try_send(if Some(request)==envelope.request_id { Ok(terminal.clone()) } else { Err("Chat was explicitly closed".into()) });
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
                            let mut state = snapshot.lock().unwrap_or_else(|p| p.into_inner());
                            SourceClock::observe(&mut source_clock.lock().unwrap_or_else(|p| p.into_inner()), update.sampled_at, Instant::now());
                            *state = Arc::new(update);
                            drop(state);
                            if visible.load(Ordering::Acquire) { wake(Wake::Changed); }
                        }
                    },
                    ServerMessage::Response { response: ServerResponse::ChatUpdate { chat_id, update } } if chat_id == id => {
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
