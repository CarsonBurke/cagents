use std::{
    io::{self, Read},
    net::Shutdown,
    os::unix::net::UnixStream,
    sync::{Arc, TryLockError},
    thread,
    time::{Duration, Instant},
};

use crate::{
    model::HostSessionProof,
    terminal_ipc::{
        ClientEnvelope, ClientRequest, FrameCodec, FrameError, LIST_PAGE_BYTES,
        MAX_CLIENT_FRAME_BYTES, MAX_FRAME_BYTES, MAX_INPUT_BYTES, MAX_RESUME_TERMINALS,
        PROTOCOL_VERSION, ProtocolError, ProtocolErrorCode, ResumeCursor, SHUTDOWN_VERSION,
        ServerEnvelope, ServerMessage, ServerResponse, ShutdownOperation, ShutdownOutcome,
        TerminalDescriptor, TerminalScroll, TerminalSnapshot, TerminalVisibility,
        WireTerminalEvent, is_cli_peer, older_client, read_frame_with_limit, validate_proof,
    },
};

use super::{
    control_gate::{self, ControlGate, Exclusive, Shared},
    delivery::DeliveryQueue,
    state::{ClientId, SharedHost, send_error, spawn_terminal},
};

#[cfg(test)]
mod automation_authority_tests;
#[cfg(test)]
mod control_admission_tests;
#[cfg(test)]
mod device_request_tests;
#[cfg(all(test, target_os = "linux"))]
mod explicit_authority_server_tests;
#[cfg(test)]
mod retired_channel_work_tests;
#[cfg(test)]
mod workspace_progress_tests;

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
struct HandshakeReader<'a> {
    stream: &'a mut UnixStream,
    deadline: Instant,
}

impl Read for HandshakeReader<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let timeout = remaining_handshake_timeout(self.deadline, Instant::now())?;
        self.stream.set_read_timeout(Some(timeout))?;
        self.stream.read(buffer)
    }
}

fn read_handshake_frame(
    stream: &mut UnixStream,
    deadline: Instant,
) -> Result<Option<ClientEnvelope>, FrameError> {
    read_frame_with_limit(
        &mut HandshakeReader { stream, deadline },
        MAX_CLIENT_FRAME_BYTES,
    )
}

fn remaining_handshake_timeout(deadline: Instant, now: Instant) -> io::Result<Duration> {
    deadline
        .checked_duration_since(now)
        .filter(|remaining| !remaining.is_zero())
        .map(|remaining| remaining.max(Duration::from_millis(1)))
        .ok_or_else(|| io::Error::new(io::ErrorKind::TimedOut, "IPC handshake deadline elapsed"))
}

/// A registered client. Dropping it unregisters the client and lets its
/// writer drain and close the socket.
///
/// This runs when the client's thread unwinds too. Otherwise a panic would
/// leave the client subscribed with its writer holding the socket open, so the
/// peer would wait on a connection that looks alive and never answers.
struct Registration {
    host: SharedHost,
    client_id: ClientId,
    delivery: Arc<DeliveryQueue>,
}

impl Registration {
    fn new(host: SharedHost, client_id: ClientId) -> Self {
        let delivery = host
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .register_client(client_id);
        Self {
            host,
            client_id,
            delivery,
        }
    }
}

impl Drop for Registration {
    fn drop(&mut self) {
        disconnect_client(&self.host, self.client_id);
        self.delivery.close_after_drain();
    }
}

/// Keep an actual terminal or native chat Close's result queueable until its handler has
/// removed custody and serve_with has enqueued the correlated reply. This is
/// not a remote ACK: existing close_after_drain settles the actual writer.
struct CheckedCloseReply {
    host: SharedHost,
    client_id: ClientId,
}
impl CheckedCloseReply {
    fn new(host: &SharedHost, client_id: ClientId) -> Result<Self, ProtocolError> {
        host.lock()
            .unwrap_or_else(|p| p.into_inner())
            .begin_checked_close_reply(client_id)?;
        Ok(Self {
            host: Arc::clone(host),
            client_id,
        })
    }
}
impl Drop for CheckedCloseReply {
    fn drop(&mut self) {
        self.host
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .finish_checked_close_reply(self.client_id);
    }
}

type RequestHandler<R> =
    fn(&SharedHost, ClientId, ClientRequest) -> Result<RequestOutcome<R>, ProtocolError>;

pub(crate) fn serve(stream: UnixStream, client_id: ClientId, host: SharedHost) {
    serve_with(stream, client_id, host, process_wire_request);
}

fn peer_pid(stream: &UnixStream) -> Option<u32> {
    use std::os::fd::AsRawFd;
    let mut credentials: libc::ucred = unsafe { std::mem::zeroed() };
    let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: getsockopt writes into the initialized ucred-sized buffer.
    let result = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut credentials as *mut libc::ucred).cast(),
            &mut length,
        )
    };
    (result == 0
        && length as usize == std::mem::size_of::<libc::ucred>()
        && credentials.uid == unsafe { libc::geteuid() }
        && credentials.pid > 0)
        .then_some(credentials.pid as u32)
}

/// Lifecycle parks without a timer, so a request that removes a retention
/// exclusion (final Detach, handoff commit, failed startup, unpinned pane) must
/// wake it. Only terminal data and read traffic is exempt: a request missing
/// here costs one maintenance pass, while the reverse would strand a worker.
/// Startup reservations can also drop off the request path, so they wake too.
fn may_release_retention(request: &ClientRequest) -> bool {
    !matches!(
        request,
        ClientRequest::List
            | ClientRequest::ListPage { .. }
            | ClientRequest::ListResidentTerminals
            | ClientRequest::Input { .. }
            | ClientRequest::Key { .. }
            | ClientRequest::Mouse { .. }
            | ClientRequest::Focus { .. }
            | ClientRequest::Scroll { .. }
            | ClientRequest::Resize { .. }
            | ClientRequest::Paste { .. }
            | ClientRequest::SetVisibility { .. }
            | ClientRequest::Snapshot { .. }
            | ClientRequest::AcknowledgeRedraw { .. }
            | ClientRequest::TerminalAcknowledge { .. }
            | ClientRequest::SearchTerminal { .. }
            | ClientRequest::CopyTerminalSelection { .. }
            | ClientRequest::ChatCommand { .. }
            | ClientRequest::ChatDeliver { .. }
            | ClientRequest::ChatVisibility { .. }
            | ClientRequest::ChatRead { .. }
            | ClientRequest::ChatAcknowledge { .. }
            | ClientRequest::ChatList
            | ClientRequest::ReadWorkspaceModel
            | ClientRequest::ReadWorkspaceView { .. }
            | ClientRequest::ReadWorkspaceSnapshot
            | ClientRequest::ReadWorkspaceMetadata { .. }
            | ClientRequest::ReadTerminalRemovalBasis { .. }
            | ClientRequest::ReadCapturedTerminalRemovalBasis { .. }
            | ClientRequest::ReadNotifications
            | ClientRequest::ProviderStatus
    )
}

/// Whether the peer process runs the `cagents` CLI, the only client this
/// daemon serves at an older protocol.
fn peer_runs_cli(pid: u32) -> bool {
    let (Ok(executable), Ok(command_line)) = (
        std::fs::read_link(format!("/proc/{pid}/exe")),
        std::fs::read(format!("/proc/{pid}/cmdline")),
    ) else {
        return false;
    };
    is_cli_peer(&executable, &command_line)
}

fn serve_with<R: Into<crate::terminal_ipc::PreparedResponse>>(
    stream: UnixStream,
    client_id: ClientId,
    host: SharedHost,
    handle: RequestHandler<R>,
) {
    let registration = Registration::new(Arc::clone(&host), client_id);
    let peer = peer_pid(&stream);
    if let Some(pid) = peer {
        host.lock()
            .unwrap_or_else(|p| p.into_inner())
            .record_client_pid(client_id, pid);
    }
    let delivery = Arc::clone(&registration.delivery);
    let Ok(writer_stream) = stream.try_clone() else {
        return;
    };
    if writer_stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .is_err()
    {
        return;
    }
    let writer_delivery = Arc::clone(&delivery);
    let writer = match thread::Builder::new()
        .name(format!("cagentsd-writer-{client_id}"))
        .spawn(move || writer_loop(writer_stream, writer_delivery))
    {
        Ok(writer) => writer,
        Err(_) => {
            delivery.abort("the client writer thread could not be started");
            return;
        }
    };

    let mut reader = stream;
    let handshake_deadline = Instant::now() + HANDSHAKE_TIMEOUT;
    let mut codec = FrameCodec::new(MAX_CLIENT_FRAME_BYTES);
    let mut connected = false;
    // The first frame's version is the connection's: this protocol's, or an
    // older one this daemon still serves to the CLI of a chat carried across
    // an update. Any other older client is refused as before.
    let mut protocol = None;
    let mut older = None;
    loop {
        let frame = if connected {
            // An older CLI's requests are read as it encoded them; see
            // `REQUEST_FIELDS_DECODED_WITHOUT`.
            crate::terminal_ipc::decoding_from(protocol.unwrap_or(PROTOCOL_VERSION), || {
                codec.read::<ClientEnvelope>(&mut reader)
            })
        } else {
            read_handshake_frame(&mut reader, handshake_deadline)
        };
        let Ok(Some(envelope)) = frame else {
            break;
        };
        if protocol.is_none() {
            older = older_client(envelope.version).filter(|_| peer.is_some_and(peer_runs_cli));
            if let Some(client) = older {
                delivery.speak_older(client);
            }
            if older.is_some() || envelope.version == PROTOCOL_VERSION {
                protocol = Some(envelope.version);
            }
        }
        if protocol != Some(envelope.version) {
            let expected = protocol.unwrap_or(PROTOCOL_VERSION);
            let error = ProtocolError::new(
                ProtocolErrorCode::UnsupportedVersion,
                format!(
                    "protocol version {} is unsupported; expected {expected}",
                    envelope.version
                ),
                false,
            );
            let _ = push_error(&delivery, &host, envelope.request_id, error);
            break;
        }
        if let Some(client) = older
            && !client.admits_request(&envelope.request)
        {
            let _ = push_error(&delivery, &host, envelope.request_id, client.refusal());
            continue;
        }

        if !connected {
            let ClientRequest::Connect {
                expected_host,
                resume,
            } = envelope.request
            else {
                let error = ProtocolError::new(
                    ProtocolErrorCode::HandshakeRequired,
                    "connect must be the first request on a client stream",
                    false,
                );
                let _ = push_error(&delivery, &host, envelope.request_id, error);
                break;
            };
            if resume.len() > MAX_RESUME_TERMINALS {
                let error = ProtocolError::new(
                    ProtocolErrorCode::InvalidRequest,
                    format!(
                        "connect resume list exceeds the {MAX_RESUME_TERMINALS}-terminal limit"
                    ),
                    false,
                );
                let _ = push_error(&delivery, &host, envelope.request_id, error);
                break;
            }
            let proof = current_proof(&host);
            if let Some(expected) = expected_host.as_ref()
                && let Err(error) = validate_proof(Some(expected), &proof)
            {
                let _ = push_error(&delivery, &host, envelope.request_id, error);
                break;
            }
            if reader.set_read_timeout(None).is_err() {
                break;
            }
            connected = true;
            // A resumed terminal's live events follow its handshake and replay.
            // Held messages are not coalesced, so hold only to resume.
            let hold = (!resume.is_empty()).then(|| delivery.hold_broadcasts());
            let (resumed, replay) = match prepare_resume(&host, client_id, resume) {
                Ok(prepared) => prepared,
                Err(error) => {
                    let _ = push_error(&delivery, &host, envelope.request_id, error);
                    break;
                }
            };
            if push_response(
                &delivery,
                &proof,
                envelope.request_id,
                ServerResponse::Connected {
                    proof: proof.clone(),
                    resumed,
                },
            )
            .is_err()
            {
                break;
            }
            for message in replay {
                let message = ServerEnvelope {
                    version: PROTOCOL_VERSION,
                    request_id: None,
                    proof: proof.clone(),
                    message,
                };
                let queued = match &hold {
                    Some(hold) => hold.push(message),
                    None => delivery.push(message),
                };
                if queued.is_err() {
                    break;
                }
            }
            continue;
        }

        if let Err(error) = validate_proof(envelope.proof.as_ref(), &current_proof(&host)) {
            let _ = push_error(&delivery, &host, envelope.request_id, error);
            break;
        }

        let request_id = envelope.request_id;
        // A request that panics has released every guard it held by the time
        // it is caught here, so it costs its sender one failed request rather
        // than the connection. The panic hook has already recorded it.
        let request = envelope.request;
        let releases_retention = may_release_retention(&request);
        // An attached terminal's live events follow its reply and replay.
        let hold =
            matches!(request, ClientRequest::Attach { .. }).then(|| delivery.hold_broadcasts());
        // Register even before ordinary exit starts: its writer can enter
        // after process_request releases control, before this reply is queued.
        // The final HostMutex seal closes acquisition before any handler effect.
        let _close_reply = if matches!(
            &request,
            ClientRequest::CloseTerminalChecked { .. }
                | ClientRequest::CloseTerminalSource { .. }
                | ClientRequest::ReleaseCompletedTerminal { .. }
                | ClientRequest::ShutdownTerminal { .. }
                | ClientRequest::ChatClose { .. }
                | ClientRequest::ChatCloseRetained { .. }
                | ClientRequest::Agent {
                    operation: crate::agent_execution::Operation::Close(_),
                    ..
                }
        ) || matches!(
            &request,
            ClientRequest::ChatLostProcesses { end, .. } if !end.is_empty()
        ) {
            match CheckedCloseReply::new(&host, client_id) {
                Ok(reply) => Some(reply),
                Err(error) => {
                    let _ = push_error(&delivery, &host, envelope.request_id, error);
                    break;
                }
            }
        } else {
            None
        };
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            handle(&host, client_id, request)
        }))
        .unwrap_or_else(|_| {
            Err(ProtocolError::new(
                ProtocolErrorCode::Internal,
                "the daemon failed while handling this request",
                false,
            ))
        });
        if releases_retention {
            crate::daemon::wake();
        }
        match outcome {
            Ok((response, extras, shutdown)) => {
                let proof = current_proof(&host);
                let response_failed =
                    push_response(&delivery, &proof, request_id, response).is_err();
                if shutdown {
                    host.lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .request_shutdown();
                    crate::daemon::wake();
                }
                if response_failed {
                    break;
                }
                for message in extras {
                    let message = ServerEnvelope {
                        version: PROTOCOL_VERSION,
                        request_id: None,
                        proof: proof.clone(),
                        message,
                    };
                    let queued = match &hold {
                        Some(hold) => hold.push(message),
                        None => delivery.push(message),
                    };
                    if queued.is_err() {
                        break;
                    }
                }
            }
            Err(error) => {
                if push_error(&delivery, &host, request_id, error).is_err() {
                    break;
                }
            }
        }
    }

    drop(registration);
    let _ = writer.join();
}

fn control_gate(host: &SharedHost) -> Arc<ControlGate> {
    // Never wait for admission while holding HostState: admitted operations
    // need to reacquire it after synchronous provider/PTY work.
    Arc::clone(&host.lock().unwrap_or_else(|p| p.into_inner()).control_gate)
}

pub(super) fn ensure_control_open(host: &SharedHost) -> Result<(), ProtocolError> {
    if host.lock().unwrap_or_else(|p| p.into_inner()).shutting_down {
        return Err(ProtocolError::new(
            ProtocolErrorCode::TerminalUnavailable,
            "Daemon upgrade/shutdown is committed; no further control mutations are accepted",
            false,
        ));
    }
    Ok(())
}

/// Fresh read-only clients must still complete their Connect handshake while
/// the ordinary exit caller retains original child tickets. This is not the
/// general mutation gate used by account/worktree/provider operations.
fn ensure_inspection_open(host: &SharedHost) -> Result<(), ProtocolError> {
    if host
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .ordinary_exit_cleanup_pending()
    {
        Ok(())
    } else {
        ensure_control_open(host)
    }
}

fn chat_startup_request(request: &ClientRequest) -> bool {
    matches!(
        request,
        ClientRequest::ChatStart { .. }
            | ClientRequest::ChatStartSelected { .. }
            | ClientRequest::ChatAttach { .. }
            | ClientRequest::ChatList
            | ClientRequest::ResolveUpgradeChat { .. }
            | ClientRequest::ChatPrepareResume { .. }
    )
}

fn ensure_request_open(host: &SharedHost, request: &ClientRequest) -> Result<(), ProtocolError> {
    if !host
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .ordinary_exit_cleanup_pending()
    {
        return ensure_control_open(host).map_err(|mut failure| {
            // This refused startup admitted no provider work. The same card
            // may reconnect after shutdown finishes; other command policies
            // retain their existing retry/replay contract.
            if chat_startup_request(request) {
                failure.retryable = true;
            }
            failure
        });
    }
    if matches!(
        request,
        ClientRequest::BuildInfo
            | ClientRequest::StartupState
            | ClientRequest::List
            | ClientRequest::ListPage { .. }
            | ClientRequest::ListResidentTerminals
            | ClientRequest::Attach { .. }
            | ClientRequest::Detach { .. }
            | ClientRequest::Snapshot { .. }
            | ClientRequest::TerminalAcknowledge { .. }
            | ClientRequest::AcknowledgeRedraw { .. }
            | ClientRequest::WatchTerminals
            | ClientRequest::Authenticate { .. }
            | ClientRequest::CloseTerminalChecked { .. }
            | ClientRequest::CloseTerminalSource { .. }
    ) {
        return Ok(());
    }
    if let ClientRequest::ShutdownV1 { version, .. } = request
        && *version != SHUTDOWN_VERSION
    {
        return Err(ProtocolError::new(
            ProtocolErrorCode::UnsupportedVersion,
            format!("shutdown version {version} is unsupported; expected {SHUTDOWN_VERSION}"),
            false,
        ));
    }
    Err(ProtocolError::new(
        ProtocolErrorCode::TerminalUnavailable,
        "Ordinary daemon exit is waiting for exact original child cleanup; only inspection and explicit checked Close remain available",
        chat_startup_request(request),
    ))
}

fn disconnect_client(host: &SharedHost, client_id: ClientId) {
    {
        let gate = control_gate(host);
        let _control = gate.shared();
        let handles = {
            let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
            if state.shutting_down && !state.ordinary_exit_cleanup_pending() {
                // Shutdown owns the remaining session state. Removing delivery is
                // safe, but unregistering would enqueue visibility/redraw effects
                // after the upgrade's terminal snapshots have been committed.
                state.forget_client_delivery(client_id);
                Vec::new()
            } else {
                state.unregister_client(client_id)
            }
        };
        // Clear a disconnected GUI's outstanding redraw edge outside HostState.
        for handle in handles {
            handle.acknowledge_redraw(handle.generation());
        }
    }
    // Outside the control gate: unwatching waits for a board being assembled,
    // which must not hold up a writer waiting at the gate.
    let work = host.lock().unwrap_or_else(|p| p.into_inner()).work.clone();
    if let Some(work) = work {
        work.unwatch(client_id);
        work.unwatch_conversation(client_id);
    }
}

type RequestOutcome<R = ServerResponse> = (R, Vec<ServerMessage>, bool);

/// Startup recovery's hold admits only what touches none of its originals;
/// see [`super::recovery_admission`].
fn ensure_startup_recovery_request(
    host: &SharedHost,
    request: &ClientRequest,
) -> Result<(), ProtocolError> {
    super::recovery_admission::admit(host, request)
}

/// The encoded `Listed` reply a frame still holds with room for its envelope.
const LIST_REPLY_LIMIT: usize = MAX_FRAME_BYTES - 64 * 1024;

/// The whole inventory as one `Listed` reply, which only an older CLI still
/// asks for, or a refusal once it passes `limit`: past a frame, the writer
/// would drop the connection without a word.
fn listed_in_one_reply(
    terminals: Vec<TerminalDescriptor>,
    limit: usize,
) -> Result<ServerResponse, ProtocolError> {
    let response = ServerResponse::Listed { terminals };
    match postcard::experimental::serialized_size(&response) {
        Ok(size) if size <= limit => Ok(response),
        _ => Err(ProtocolError::new(
            ProtocolErrorCode::InvalidRequest,
            "The terminal inventory is too large to list in one reply; update cagents to list it",
            false,
        )),
    }
}

/// The resident terminals as one reply, or a refusal once it passes `limit`,
/// which no real daemon reaches: an identity encodes in 17 bytes and a
/// transcript path in a few hundred.
fn resident_in_one_reply(
    response: ServerResponse,
    limit: usize,
) -> Result<ServerResponse, ProtocolError> {
    match postcard::experimental::serialized_size(&response) {
        Ok(size) if size <= limit => Ok(response),
        _ => Err(ProtocolError::new(
            ProtocolErrorCode::InvalidRequest,
            "This daemon holds too many terminals to name them in one reply",
            false,
        )),
    }
}

fn ensure_terminal_close_open(host: &SharedHost) -> Result<(), ProtocolError> {
    let state = host.lock().unwrap_or_else(|p| p.into_inner());
    if state.terminal_close_admission_open() {
        return Ok(());
    }
    Err(ProtocolError::new(
        ProtocolErrorCode::TerminalUnavailable,
        "Daemon exit has sealed terminal cleanup admission",
        false,
    ))
}

fn retention_pin_generation(
    host: &SharedHost,
    terminal: crate::model::TerminalId,
) -> Result<Option<String>, String> {
    super::agent_execution::registry(host).and_then(|registry| {
        registry
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .retention_pin_generation(terminal)
    })
}

/// Capture the exact native pair before stopping the PTY. Re-resolving only
/// its durable ID after a wait could close a replacement conversation.
fn terminal_close_pair(
    host: &SharedHost,
    terminal: crate::model::TerminalId,
    expected: &crate::terminal::TerminalHandle,
) -> Result<Option<crate::chat::runtime::Handle>, ProtocolError> {
    super::chat::preflight_for_terminal_close(host, terminal, expected)?;
    let state = host.lock().unwrap_or_else(|p| p.into_inner());
    if !state
        .terminals
        .get(&terminal)
        .is_some_and(|record| record.handle.same_worker(expected))
    {
        return Err(ProtocolError::new(
            ProtocolErrorCode::TerminalUnavailable,
            "The terminal changed before close; its current owner was retained",
            true,
        ));
    }
    state
        .chats
        .get(&terminal)
        .filter(|record| {
            (record.session.launch.provider == crate::agent::Provider::Codex
                || record.owns_execution_console())
                && record
                    .session
                    .terminal
                    .as_ref()
                    .is_some_and(|source| source.id == terminal)
        })
        .map(|record| {
            if !record
                .paired_terminal
                .as_ref()
                .is_some_and(|handle| handle.same_worker(expected))
            {
                return Err(ProtocolError::new(
                    ProtocolErrorCode::TerminalUnavailable,
                    "The terminal worker no longer belongs to this provider pairing",
                    true,
                ));
            }
            Ok(record.handle.clone())
        })
        .transpose()
}

struct TerminalCloseRetirement {
    terminal: crate::model::TerminalId,
    handle: crate::terminal::TerminalHandle,
    source: crate::model::TerminalSession,
    proof: Arc<HostSessionProof>,
    generation: Result<Option<String>, String>,
    ticket: uuid::Uuid,
    deadline: Instant,
    _interrupted: ExplicitCloseGuard,
    previous: Option<super::state::Retirement>,
}

struct ExplicitCloseGuard {
    host: std::sync::Weak<super::state::HostLock>,
    terminal: crate::model::TerminalId,
    ticket: uuid::Uuid,
}

impl Drop for ExplicitCloseGuard {
    fn drop(&mut self) {
        let Some(host) = self.host.upgrade() else {
            return;
        };
        let changed = {
            let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
            if let Some(entry) = state.terminal_retirements.get_mut(&self.terminal)
                && entry.ticket == self.ticket
                && entry.explicit_close
                && entry.failure.is_none()
            {
                entry.failure = Some("Explicit close was interrupted before cleanup could be confirmed; retry the retained result".into());
                state.inventory_generation += 1;
                true
            } else {
                false
            }
        };
        if changed {
            crate::daemon::wake();
        }
    }
}

/// One close outlasts a slow commit or shared request, then asks for a retry.
/// The client allows TERMINAL_CLOSE_TIMEOUT, which also covers its teardown.
const CLOSE_FENCE_BUDGET: Duration = Duration::from_secs(10);

fn close_busy() -> ProtocolError {
    ProtocolError::new(
        ProtocolErrorCode::TerminalUnavailable,
        "Cagents is busy; retry closing this terminal",
        true,
    )
}

/// Take the writer and read the saved pin under it. A close is routine, so it
/// waits for a free gate without queuing: it holds back no other request, and
/// under steady traffic it refuses at its deadline instead. A held writer must
/// not wait out another thread's durable commit, so it is released while the
/// registry is busy.
fn close_fence<'gate>(
    host: &SharedHost,
    gate: &'gate ControlGate,
    terminal: crate::model::TerminalId,
    deadline: Instant,
) -> Result<(Exclusive<'gate>, Result<Option<String>, String>), ProtocolError> {
    let registry = super::agent_execution::registry(host);
    loop {
        let admission = gate
            .exclusive_when_free_until(deadline)
            .ok_or_else(close_busy)?;
        let registry = match &registry {
            Ok(registry) => registry,
            Err(cause) => return Ok((admission, Err(cause.clone()))),
        };
        let pin = match registry.try_lock() {
            Ok(registry) => Some(registry.retention_pin_generation(terminal)),
            Err(TryLockError::Poisoned(poisoned)) => {
                Some(poisoned.into_inner().retention_pin_generation(terminal))
            }
            Err(TryLockError::WouldBlock) => None,
        };
        if let Some(pin) = pin {
            return Ok((admission, pin));
        }
        drop(admission);
        if Instant::now() >= deadline {
            return Err(close_busy());
        }
        thread::sleep(Duration::from_millis(5));
    }
}

/// The caller dropped its shared reader. Drain earlier publications and reserve
/// this exact owner under the writer BEFORE any destructive close or cache release.
fn reserve_terminal_close(
    host: &SharedHost,
    terminal: crate::model::TerminalId,
    expected: &crate::terminal::TerminalHandle,
    source: &crate::model::TerminalSession,
    paired: Option<&crate::chat::runtime::Handle>,
    generation: Result<Option<String>, String>,
    capture: Option<&crate::workspace_model::views::CapturedTerminalSource>,
) -> Result<TerminalCloseRetirement, ProtocolError> {
    let gate = control_gate(host);
    let deadline = Instant::now() + CLOSE_FENCE_BUDGET;
    let reserve = |current: Result<Option<String>, String>| -> Result<TerminalCloseRetirement, ProtocolError> {
    ensure_terminal_close_open(host)?;
    let unavailable =
        |message| ProtocolError::new(ProtocolErrorCode::TerminalUnavailable, message, true);
    let captured = generation.as_ref().map_err(|cause| {
        unavailable(format!(
            "Saved terminal ownership was unknown before close: {cause}"
        ))
    })?;
    let current = current.map_err(|cause| {
        unavailable(format!(
            "Saved terminal ownership cannot be revalidated before close: {cause}"
        ))
    })?;
    if &current != captured {
        return Err(unavailable(
            "Saved terminal ownership changed before close; no cleanup was started".into(),
        ));
    }
    let (mut reservation, previous) = {
        let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
        let pair_matches = match paired {
            Some(actor) => state.chats.get(&terminal).is_some_and(|record| {
                record.handle.same_attachment(actor)
                    && record
                        .paired_terminal
                        .as_ref()
                        .is_some_and(|handle| handle.same_worker(expected))
            }),
            None => true,
        };
        if !pair_matches
            || state.chats.values().any(|record| {
                record
                    .paired_terminal
                    .as_ref()
                    .is_some_and(|handle| handle.same_worker(expected))
                    && paired.is_none_or(|actor| !record.handle.same_attachment(actor))
            })
            || (capture.is_some() && state.removal_in_transition(terminal))
            || super::chat::terminal_claim_pending(&state, terminal)
        {
            return Err(ProtocolError::new(
                ProtocolErrorCode::TerminalUnavailable,
                "The native conversation changed during close; its terminal pair was retained",
                true,
            ));
        }
        let source = state.terminals.get(&terminal)
            .filter(|record| record.handle.same_worker(expected) && record.session == *source)
            .ok_or_else(|| ProtocolError::new(ProtocolErrorCode::TerminalUnavailable,
                "The terminal changed during close; its current owner and saved pin were retained", true))?
            .session.clone();
        if state
            .terminal_retirements
            .get(&terminal)
            .is_some_and(|entry| !entry.handle.same_worker(expected) || entry.failure.is_none())
        {
            return Err(ProtocolError::new(
                ProtocolErrorCode::TerminalUnavailable,
                "Terminal cleanup already owns this result; retry after it settles",
                true,
            ));
        }
        let entry = super::state::Retirement::explicit_close(expected.clone());
        let reservation = TerminalCloseRetirement {
            terminal,
            handle: expected.clone(),
            source,
            proof: state.proof_handle(),
            generation,
            ticket: entry.ticket,
            deadline,
            _interrupted: ExplicitCloseGuard {
                host: Arc::downgrade(host),
                terminal,
                ticket: entry.ticket,
            },
            previous: None,
        };
        let previous = state.terminal_retirements.insert(terminal, entry);
        state.inventory_generation += 1;
        (reservation, previous)
    };
    reservation.previous = previous;
    Ok(reservation)
    };
    if let Some(capture) = capture {
        with_source_close_fence(host, &gate, terminal, deadline, capture, reserve)
    } else {
        let (_admission, current) = close_fence(host, &gate, terminal, deadline)?;
        reserve(current)
    }
}

fn source_cleanup_conflict() -> ProtocolError {
    ProtocolError::new(
        ProtocolErrorCode::InvalidRequest,
        crate::terminal_ipc::TERMINAL_CLOSE_SOURCE_CONFLICT,
        false,
    )
}

/// Acquire SQL before State, and never wait for the control writer while holding
/// SQL. Readers queued on the registry can therefore finish before the retry.
fn with_source_close_fence<T>(
    host: &SharedHost,
    gate: &ControlGate,
    terminal: crate::model::TerminalId,
    deadline: Instant,
    capture: &crate::workspace_model::views::CapturedTerminalSource,
    reserve: impl FnOnce(Result<Option<String>, String>) -> Result<T, ProtocolError>,
) -> Result<T, ProtocolError> {
    let writer = super::workspace::writer(host)?;
    loop {
        let registry = match writer.try_lock() {
            Ok(registry) => Some(registry),
            Err(TryLockError::Poisoned(p)) => Some(p.into_inner()),
            Err(TryLockError::WouldBlock) => None,
        };
        if let Some(mut registry) = registry {
            let sql = registry
                .source_admission()
                .map_err(|error| ProtocolError::new(ProtocolErrorCode::Internal, error, true))?;
            let current = sql.retention_pin_generation(terminal);
            if let Some(_admission) = gate.try_exclusive() {
                sql.validate(capture)
                    .map_err(|_| source_cleanup_conflict())?;
                if !matches!(&capture.before.kind, crate::model::CardKind::Terminal(s) if s.id == terminal)
                {
                    return Err(source_cleanup_conflict());
                }
                return reserve(current);
            }
        }
        if Instant::now() >= deadline {
            return Err(close_busy());
        }
        thread::sleep(Duration::from_millis(5));
    }
}

/// Joining can transfer a still-live original holder to the reaper. Check the
/// held pidfd and exact authenticated receipt outside Host AND control locks.
fn finish_terminal_close(
    host: &SharedHost,
    close: TerminalCloseRetirement,
) -> Result<(), ProtocolError> {
    let result = match close
        .handle
        .completed_retirement(&close.source, &close.proof)
    {
        crate::terminal::TerminalRetirement::Released => Ok(()),
        crate::terminal::TerminalRetirement::Pending => {
            Err("Terminal worker retirement is still pending".into())
        }
        crate::terminal::TerminalRetirement::WaitingForHolder(_) => {
            Err("The original terminal holder has not exited; cleanup is pending".into())
        }
        crate::terminal::TerminalRetirement::Retained(error) => Err(error),
    };
    let error = |message| ProtocolError::new(ProtocolErrorCode::TerminalUnavailable, message, true);
    if let Err(message) = result {
        let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(entry) = state.terminal_retirements.get_mut(&close.terminal)
            && entry.ticket == close.ticket
            && entry.handle.same_worker(&close.handle)
        {
            entry.failure = Some(message.clone());
            state.inventory_generation += 1;
        }
        return Err(error(message));
    }
    let gate = control_gate(host);
    let (mut admission, current) = close_fence(host, &gate, close.terminal, close.deadline)?;
    ensure_terminal_close_open(host)?;
    // None is a known unpinned local window, not an unavailable Registry.
    // The writer also drains publications that passed their initial check
    // before our reservation. It fences local/shared edits through removal.
    let captured = close.generation.as_ref().map_err(|cause| {
        error(format!(
            "Saved terminal ownership was unknown before close: {cause}"
        ))
    })?;
    let current = current.map_err(|cause| {
        error(format!(
            "Saved terminal ownership cannot be revalidated: {cause}"
        ))
    })?;
    if &current != captured {
        return Err(error(
            "Saved terminal ownership changed during close; retry the retained result".into(),
        ));
    }
    if !close_retirement_is_current(&host.lock().unwrap_or_else(|p| p.into_inner()), &close) {
        return Err(error(
            "Terminal ownership changed while cleanup settled; its cleanup record was retained"
                .into(),
        ));
    }
    if let Ok(Some(generation)) = &close.generation {
        // This commit runs outside the writer. The DELETE fences itself on the
        // generation, and the fresh read below refuses any newer publication.
        drop(admission);
        let result = super::agent_execution::registry(host).and_then(|registry| {
            registry
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .forget_retention_pin(close.terminal, generation)
        });
        if let Err(cause) = result {
            let message = format!(
                "Saved terminal pin could not be removed; retry the retained cleanup record: {cause}"
            );
            let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
            if let Some(entry) = state.terminal_retirements.get_mut(&close.terminal)
                && entry.ticket == close.ticket
                && entry.handle.same_worker(&close.handle)
            {
                entry.failure = Some(message.clone());
                state.inventory_generation += 1;
            }
            return Err(error(message));
        }
        let (fenced, current) = close_fence(host, &gate, close.terminal, close.deadline)?;
        admission = fenced;
        ensure_terminal_close_open(host)?;
        let current = current.map_err(|cause| {
            error(format!(
                "Saved terminal ownership cannot be revalidated: {cause}"
            ))
        })?;
        if current.is_some() {
            return Err(error(
                "Saved terminal ownership changed during close; retry the retained result".into(),
            ));
        }
    }
    let retired = {
        let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
        if !close_retirement_is_current(&state, &close) {
            return Err(error(
                "The exact terminal changed during pin removal; its cleanup record was retained"
                    .into(),
            ));
        }
        // Released was proved outside both locks. Clear only this original's
        // startup slot while its exact record still exists, then compact it.
        state.startup_closed(close.terminal, &close.handle);
        state.retire_terminal_if_current(close.terminal, &close.handle)
    };
    let retired =
        retired.ok_or_else(|| error("The exact closed terminal is no longer available".into()))?;
    drop(admission);
    drop(retired);
    crate::daemon::wake();
    Ok(())
}

fn close_retirement_is_current(
    state: &super::state::HostState,
    close: &TerminalCloseRetirement,
) -> bool {
    state
        .terminal_retirements
        .get(&close.terminal)
        .is_some_and(|entry| {
            entry.ticket == close.ticket && entry.handle.same_worker(&close.handle)
        })
        && state.terminals.get(&close.terminal).is_some_and(|record| {
            record.handle.same_worker(&close.handle) && record.session == close.source
        })
        && !state.chats.values().any(|record| {
            record
                .paired_terminal
                .as_ref()
                .is_some_and(|handle| handle.same_worker(&close.handle))
        })
        && !super::chat::terminal_claim_pending(state, close.terminal)
}

#[cfg(test)]
pub(super) fn process_request(
    host: &SharedHost,
    client_id: ClientId,
    request: ClientRequest,
) -> Result<RequestOutcome, ProtocolError> {
    let (response, extras, shutdown) = process_wire_request(host, client_id, request)?;
    Ok((
        response
            .decode()
            .map_err(|message| ProtocolError::new(ProtocolErrorCode::Internal, message, false))?,
        extras,
        shutdown,
    ))
}

/// No client sends this yet; its GitHub read has already spent part of the
/// 120-second device-operation deadline before the writer is wanted. Cleanup is
/// routine, so it waits for a free gate without holding anyone back.
const MERGED_WORKTREE_REMOVAL_PATIENCE: Duration = Duration::from_secs(10);

/// The requests that hold the control gate exclusively, and how long each waits
/// for it: half of what its client allows for the reply. The daemon then never
/// runs one its client has given up on, and the admitted work has the other half.
fn exclusive_patience(request: &ClientRequest) -> Option<Duration> {
    match request {
        // `ControlClient::request_strict` waits five seconds for the reply.
        ClientRequest::PrepareUpgrade { .. }
        | ClientRequest::Shutdown
        | ClientRequest::ShutdownV1 { .. }
        | ClientRequest::ChatRelease { dry_run: false, .. } => Some(Duration::from_millis(2500)),
        // `ControlClient::prepare_confirmed_upgrade` allows sixty seconds.
        ClientRequest::PrepareConfirmedUpgrade { .. } => Some(Duration::from_secs(30)),
        // The chat connection's `return_terminal` waits forty seconds.
        ClientRequest::ChatReturnTerminal { .. } => Some(Duration::from_secs(20)),
        // An account switch reads its attach connection for seventy seconds.
        ClientRequest::ChatRecoverAccount {
            idle_generation: Some(_),
            ..
        } => Some(Duration::from_secs(35)),
        _ => None,
    }
}

/// Requests answered from HostState and a send to the terminal worker. They
/// launch nothing and change no ownership, so they are admitted beside a
/// queued exclusive request instead of behind the requests it waits for. They
/// still wait while an exclusive request holds the gate, so no client request
/// reaches a worker during an exclusive section. Most sends never wait; search, copy and
/// clearing scrollback wait up to three seconds, and only while that
/// terminal's worker is preparing its archive.
fn terminal_io_request(request: &ClientRequest) -> bool {
    matches!(
        request,
        ClientRequest::Input { .. }
            | ClientRequest::Key { .. }
            | ClientRequest::Mouse { .. }
            | ClientRequest::Focus { .. }
            | ClientRequest::Scroll { .. }
            | ClientRequest::Resize { .. }
            | ClientRequest::Paste { .. }
            | ClientRequest::SetTheme { .. }
            | ClientRequest::SetVisibility { .. }
            | ClientRequest::Hibernate { .. }
            | ClientRequest::AcknowledgeRedraw { .. }
            | ClientRequest::TerminalAcknowledge { .. }
            | ClientRequest::SearchTerminal { .. }
            | ClientRequest::CopyTerminalSelection { .. }
            | ClientRequest::ClearTerminalScrollback { .. }
    )
}

/// An account operation's failure as the daemon's reply. Both refusals left
/// everything as it was and say so with `retryable`, so a client tells the
/// person to wait and retry rather than reporting a failed operation. Only a
/// busy control gate is the daemon's own busy sentence.
pub(super) fn lifecycle_refusal(error: crate::account::remote::LifecycleError) -> ProtocolError {
    match error {
        crate::account::remote::LifecycleError::Busy => control_gate::busy(),
        crate::account::remote::LifecycleError::ListHeld(held) => {
            ProtocolError::new(ProtocolErrorCode::InvalidRequest, held.to_string(), true)
        }
        crate::account::remote::LifecycleError::Failed(message) => {
            ProtocolError::new(ProtocolErrorCode::InvalidRequest, message, false)
        }
    }
}

/// Runs checks that admit `request` before it changes anything. An automatic
/// account switch they refuse is marked as refused before any effect, so its
/// client keeps the conversation on its account; the switch's own checks are
/// marked the same way in `chat::recover_account`. Every other request keeps
/// the refusal as it is.
fn admission(
    request: &ClientRequest,
    checks: impl FnOnce() -> Result<(), ProtocolError>,
) -> Result<(), ProtocolError> {
    checks().map_err(|failure| {
        if matches!(
            request,
            ClientRequest::ChatRecoverAccount {
                idle_generation: Some(_),
                ..
            }
        ) {
            failure.not_admitted()
        } else {
            failure
        }
    })
}

fn process_wire_request(
    host: &SharedHost,
    client_id: ClientId,
    request: ClientRequest,
) -> Result<RequestOutcome<crate::terminal_ipc::PreparedResponse>, ProtocolError> {
    admission(&request, || {
        ensure_request_open(host, &request)?;
        ensure_startup_recovery_request(host, &request)
    })?;
    let gate = control_gate(host);
    // The GitHub read must not monopolize the exclusive admission fence. All
    // authorizing local state is checked again only after that fence is held.
    if let ClientRequest::DeviceOperation {
        operation:
            crate::device_service::Operation::RemoveMergedWorktree {
                request: removal_request,
            },
    } = &request
    {
        let prepared = {
            let _shared = gate.shared();
            ensure_control_open(host)?;
            super::agent_execution::terminals::admit(host, client_id, &request)?;
            crate::worktree_cleanup::PreparedRemoval::prepare(removal_request.clone())
        };
        let removed = match prepared {
            Ok(prepared) => {
                let _exclusive = gate
                    .exclusive_when_free_until(Instant::now() + MERGED_WORKTREE_REMOVAL_PATIENCE)
                    .ok_or_else(control_gate::busy)?;
                ensure_control_open(host)?;
                super::account_lifecycle::ensure_idle(host)?;
                prepared
                    .remove(|windows| {
                        host.lock()
                            .unwrap_or_else(|p| p.into_inner())
                            .allow_automatic_worktree_cleanup(client_id, windows)
                    })
                    .ok()
            }
            Err(_) => None,
        };
        return Ok((
            ServerResponse::DeviceOperation {
                reply: crate::device_service::Reply::MergedWorktreeRemoved(removed),
            }
            .into(),
            Vec::new(),
            false,
        ));
    }
    // Install only the physical token under a short writer. Provider and
    // registry work runs outside global admission while related starts refuse.
    if let ClientRequest::DeviceOperation {
        operation: crate::device_service::Operation::AccountLifecycle { operation },
    } = &request
    {
        {
            let _shared = gate.shared();
            ensure_control_open(host)?;
            super::agent_execution::terminals::admit(host, client_id, &request)?;
        }
        let reservation = match operation {
            crate::account::remote::Operation::Restore { .. } => {
                super::account_lifecycle::reserve_restore(host, operation)?
            }
            _ => super::account_lifecycle::Reservation::for_operation(host, operation)?,
        };
        let value = crate::account::remote::execute_reserved(operation.clone(), &reservation)
            .map_err(lifecycle_refusal)?;
        return Ok((
            ServerResponse::DeviceOperation {
                reply: crate::device_service::Reply::AccountLifecycle(value),
            }
            .into(),
            Vec::new(),
            false,
        ));
    }
    // Compiled only into debug builds, and inert outside a native-evidence
    // fixture that asks for it.
    #[cfg(debug_assertions)]
    if matches!(
        &request,
        ClientRequest::ShutdownV1 {
            operation: ShutdownOperation::Preflight,
            ..
        }
    ) {
        super::fixture_hold::hold_preflight(&gate);
    }
    // Inventory decisions and commit must wait for all previously admitted
    // effects, including provider closes that finish outside HostState.
    let (_exclusive, _shared) = match exclusive_patience(&request) {
        Some(patience) => (
            Some(
                gate.exclusive_until(Instant::now() + patience)
                    .ok_or_else(control_gate::busy)?,
            ),
            None,
        ),
        // A snapshot waits for the worker's next frame, which can take
        // seconds. It is admitted as terminal I/O only while its read is
        // queued (`state::client_snapshot`), so it neither holds back the
        // input behind it on its connection nor keeps the gate shared.
        None if matches!(request, ClientRequest::Snapshot { .. }) => (None, None),
        None if terminal_io_request(&request) => (None, Some(gate.terminal_io())),
        None => (None, Some(gate.shared())),
    };
    admission(&request, || {
        ensure_request_open(host, &request)?;
        ensure_startup_recovery_request(host, &request)?;
        super::agent_execution::terminals::admit(host, client_id, &request)?;
        super::account_lifecycle::admit(host, &request)?;
        super::worktree_removal::admit(host, &request)
    })?;
    // Retained originals are outside the session inventory. Refuse before
    // publishing a preflight capability or requesting termination. This only
    // observes custody already published now; quarantine can publish later.
    // A malformed wire revision keeps its existing UnsupportedVersion result.
    let shutdown_admission = matches!(&request, ClientRequest::Shutdown)
        || matches!(&request, ClientRequest::ShutdownV1 { version, .. }
            if *version == SHUTDOWN_VERSION);
    if shutdown_admission && crate::daemon::child_reaper::has_retained_ownership() {
        return Err(ProtocolError::new(
            ProtocolErrorCode::TerminalUnavailable,
            super::state::RETAINED_CHILD_CLEANUP_MESSAGE,
            true,
        ));
    }
    if shutdown_admission
        || matches!(
            &request,
            ClientRequest::PrepareUpgrade { .. }
                | ClientRequest::PrepareConfirmedUpgrade { .. }
                | ClientRequest::UpgradeImpact { .. }
        )
    {
        let state = host.lock().unwrap_or_else(|p| p.into_inner());
        if !state.terminal_retirements.is_empty() {
            let failed = state
                .terminal_retirements
                .values()
                .any(|entry| entry.failure.is_some());
            return Err(ProtocolError::new(
                ProtocolErrorCode::TerminalUnavailable,
                if failed {
                    "Terminal retirement is unconfirmed; explicitly close the retained result before shutdown or upgrade"
                } else {
                    "Terminal retirement is pending; retry shutdown or upgrade after cleanup settles"
                },
                true,
            ));
        }
    }
    if let ClientRequest::ChatRead {
        chat_id,
        after_generation,
    } = request
    {
        return super::chat::read_presentation(host, client_id, chat_id, after_generation)
            .map(|response| (response, Vec::new(), false));
    }
    let (response, extras, shutdown) = process_admitted_request(host, client_id, request, _shared)?;
    Ok((response.into(), extras, shutdown))
}

fn validate_cleanup_source(
    state: &super::state::HostState,
    expected: Option<&(
        crate::model::TerminalSession,
        crate::workspace_model::views::TerminalRemovalBasis,
    )>,
) -> Result<(), ProtocolError> {
    if let Some((source, basis)) = expected {
        if !state.terminals.contains_key(&source.id) {
            return Err(ProtocolError::new(
                ProtocolErrorCode::TerminalNotFound,
                crate::terminal_ipc::TERMINAL_CLOSE_SOURCE_ABSENT,
                false,
            ));
        }
        let valid = match basis {
            crate::workspace_model::views::TerminalRemovalBasis::Worker { owner, instance } => {
                owner == state.proof()
                    && owner.is_complete()
                    && !instance.is_nil()
                    && state.terminals.get(&source.id).is_some_and(|record| {
                        if record.handle.execution_instance() != *instance {
                            return false;
                        }
                        state.descriptor(0, source.id).is_ok_and(|descriptor| {
                            let mut current = descriptor.session;
                            current.continuity = source.continuity.clone();
                            current == *source
                        })
                    })
            }
            _ => false,
        };
        if !valid {
            return Err(ProtocolError::new(
                ProtocolErrorCode::InvalidRequest,
                crate::terminal_ipc::TERMINAL_CLOSE_SOURCE_CONFLICT,
                false,
            ));
        }
    }
    Ok(())
}

fn process_admitted_request(
    host: &SharedHost,
    client_id: ClientId,
    request: ClientRequest,
    _shared: Option<Shared<'_>>,
) -> Result<RequestOutcome, ProtocolError> {
    // Adapt the additive request to the established close paths. Its original
    // source is checked under the same lock that selects the resident handle;
    // the existing close reservation then retains that exact handle.
    let mut cleanup_capture = None;
    let (request, cleanup_source) = match request {
        ClientRequest::CloseTerminalSource {
            session,
            basis,
            completed_only,
            allow_active,
            capture,
        } => {
            cleanup_capture = capture;
            let request = if completed_only {
                ClientRequest::ReleaseCompletedTerminal {
                    session: session.clone(),
                    allow_active,
                }
            } else {
                ClientRequest::CloseTerminalChecked {
                    terminal_id: session.id,
                    allow_active,
                }
            };
            (request, Some((session, basis)))
        }
        request => (request, None),
    };
    let notification_watch = matches!(&request, ClientRequest::WatchNotifications { .. });
    match request {
        ClientRequest::DeviceOperation { operation } => {
            if let crate::device_service::Operation::AccountReset { request } = operation {
                let outcome = super::agent_execution::account_info::consume(host, request)
                    .map_err(|message| {
                        ProtocolError::new(ProtocolErrorCode::InvalidRequest, message, false)
                    })?;
                let value = serde_json::to_value(outcome).map_err(|_| {
                    ProtocolError::new(
                        ProtocolErrorCode::Internal,
                        "The reset result could not be encoded; refresh usage before retrying",
                        false,
                    )
                })?;
                return Ok((
                    ServerResponse::DeviceOperation {
                        reply: crate::device_service::Reply::AccountReset(value),
                    },
                    Vec::new(),
                    false,
                ));
            }
            match &operation {
                crate::device_service::Operation::AccountLifecycle { .. } => {
                    return Err(ProtocolError::new(
                        ProtocolErrorCode::InvalidRequest,
                        "Account operations require daemon admission",
                        false,
                    ));
                }
                crate::device_service::Operation::Skills { launch } => {
                    super::account_lifecycle::ensure_launch(
                        host,
                        &launch.recipe.working_directory,
                        &launch.recipe.environment,
                        &Default::default(),
                        true,
                    )?;
                    super::account_lifecycle::ensure_launch(
                        host,
                        &launch.recipe.working_directory,
                        &launch.recipe.environment,
                        &crate::account::launch_credential_overrides(&launch.recipe),
                        true,
                    )?;
                }
                crate::device_service::Operation::AccountsUpdate { registry, .. } => {
                    let proposed = serde_json::from_value(registry.clone()).map_err(|error| {
                        ProtocolError::new(
                            ProtocolErrorCode::InvalidRequest,
                            format!("Invalid account preferences: {error}"),
                            false,
                        )
                    })?;
                    super::account_lifecycle::ensure_update(host, &proposed)?;
                }
                crate::device_service::Operation::AccountLogin { account }
                | crate::device_service::Operation::AccountProbe { account, .. }
                | crate::device_service::Operation::AccountProbeAfterLogin { account } => {
                    super::account_lifecycle::ensure_paths(host, account.resolved_home())?;
                }
                _ => {}
            }
            let reply = match operation {
                crate::device_service::Operation::AccountDefault {
                    expected_revision,
                    provider,
                    selection,
                } => {
                    crate::device_service::account_default(expected_revision, provider, &selection)
                        .map_err(lifecycle_refusal)?
                }
                crate::device_service::Operation::AccountsUpdate {
                    expected_revision,
                    registry,
                } => crate::device_service::update_accounts(expected_revision, registry)
                    .map_err(lifecycle_refusal)?,
                operation => crate::device_service::execute(operation).map_err(|message| {
                    ProtocolError::new(ProtocolErrorCode::InvalidRequest, message, false)
                })?,
            };
            Ok((ServerResponse::DeviceOperation { reply }, Vec::new(), false))
        }
        ClientRequest::Authenticate { authority } => {
            super::agent_execution::terminals::authenticate(host, client_id, &authority)?;
            Ok((ServerResponse::Accepted, Vec::new(), false))
        }
        ClientRequest::DeviceTrust { request } => {
            drop(_shared);
            let reply = super::device_trust::process(host, request)?;
            Ok((ServerResponse::DeviceTrust(reply), Vec::new(), false))
        }
        ClientRequest::PeerChallenge => {
            let nonce = super::device_trust::challenge(host, client_id)?;
            Ok((ServerResponse::PeerChallenge { nonce }, Vec::new(), false))
        }
        ClientRequest::PeerAuthenticate { peer, claim, proof } => {
            drop(_shared);
            let authority =
                super::device_trust::authenticate(host, client_id, peer, &claim, &proof)?;
            Ok((
                ServerResponse::PeerAuthenticated { authority },
                Vec::new(),
                false,
            ))
        }
        ClientRequest::SignDeviceChallenge {
            authority,
            device,
            nonce,
        } => {
            drop(_shared);
            let (peer, claim, proof) =
                super::device_trust::sign(host, &authority, &device, &nonce)?;
            Ok((
                ServerResponse::DeviceProof { peer, claim, proof },
                Vec::new(),
                false,
            ))
        }
        ClientRequest::RetiredChannelWork => Err(ProtocolError::new(
            ProtocolErrorCode::InvalidRequest,
            crate::terminal_ipc::RETIRED_CHANNEL_WORK,
            false,
        )),
        ClientRequest::ProviderStatus => {
            drop(_shared);
            let providers = super::provider_status::snapshot()?;
            Ok((ServerResponse::ProviderStatus(providers), Vec::new(), false))
        }
        ClientRequest::Work(request) => {
            // Admission was checked above. A board may wait on scans and
            // GitHub for its bound, which must not hold the fence that
            // shutdown and upgrades take.
            drop(_shared);
            let work = host
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .work
                .clone()
                .ok_or_else(|| {
                    ProtocolError::new(
                        ProtocolErrorCode::InvalidRequest,
                        "The Work board is unavailable",
                        false,
                    )
                })?;
            let response = work.process(host, request).map_err(|message| {
                ProtocolError::new(ProtocolErrorCode::InvalidRequest, message, false)
            })?;
            Ok((ServerResponse::Work(response), Vec::new(), false))
        }
        ClientRequest::WatchWork => {
            // Like a board read, the first answer may wait on scans.
            drop(_shared);
            let (work, delivery) = {
                let state = host.lock().unwrap_or_else(|p| p.into_inner());
                (
                    state.work.clone(),
                    state.clients.get(&client_id).map(Arc::downgrade),
                )
            };
            let unavailable = |message: &str| {
                ProtocolError::new(ProtocolErrorCode::InvalidRequest, message, false)
            };
            let work = work.ok_or_else(|| unavailable("The Work board is unavailable"))?;
            let delivery =
                delivery.ok_or_else(|| unavailable("This connection cannot be sent updates"))?;
            let (revision, board) = work
                .watch(host, client_id, delivery)
                .map_err(|message| unavailable(&message))?;
            Ok((
                ServerResponse::Work(crate::work::Response::Watching {
                    revision,
                    board: Box::new(board),
                }),
                Vec::new(),
                false,
            ))
        }
        ClientRequest::WatchConversationPullRequests { conversation } => {
            drop(_shared);
            let (work, delivery) = {
                let state = host.lock().unwrap_or_else(|p| p.into_inner());
                (
                    state.work.clone(),
                    state.clients.get(&client_id).map(Arc::downgrade),
                )
            };
            let unavailable = |message: &str| {
                ProtocolError::new(ProtocolErrorCode::InvalidRequest, message, false)
            };
            let work = work.ok_or_else(|| unavailable("The Work board is unavailable"))?;
            let delivery =
                delivery.ok_or_else(|| unavailable("This connection cannot be sent updates"))?;
            let (revision, pull_requests) = work
                .watch_conversation(host, client_id, conversation, delivery)
                .map_err(|message| unavailable(&message))?;
            Ok((
                ServerResponse::Work(crate::work::Response::WatchingConversation {
                    revision,
                    pull_requests: Box::new(pull_requests),
                }),
                Vec::new(),
                false,
            ))
        }
        ClientRequest::Automation {
            authority,
            operation,
        } => {
            let reply =
                super::automations::service::process_under_control(host, authority, operation)
                    .map_err(super::automations::service::Error::into_protocol)?;
            Ok((ServerResponse::Automation(reply), Vec::new(), false))
        }
        ClientRequest::Agent {
            authority,
            operation,
        } => {
            let reply = super::agent_execution::process_request(host, authority, operation)
                .map_err(|error| match error {
                    crate::agent_execution::AcceptanceError::Invalid(message) => {
                        ProtocolError::new(ProtocolErrorCode::InvalidRequest, message, false)
                    }
                    crate::agent_execution::AcceptanceError::StoreBusy(message) => {
                        ProtocolError::new(ProtocolErrorCode::StoreBusy, message, true)
                    }
                    crate::agent_execution::AcceptanceError::MailboxFull(message) => {
                        ProtocolError::new(ProtocolErrorCode::QueueFull, message, true)
                    }
                })?;
            Ok((ServerResponse::Agent(reply), Vec::new(), false))
        }
        ClientRequest::ReconcileReviewRequests { update } => {
            update.validate().map_err(|message| {
                ProtocolError::new(ProtocolErrorCode::InvalidRequest, message, false)
            })?;
            let notifications = host
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .notifications
                .clone()
                .ok_or_else(|| {
                    ProtocolError::new(
                        ProtocolErrorCode::TerminalUnavailable,
                        "Notification delivery is unavailable",
                        true,
                    )
                })?;
            notifications
                .reconcile_review_requests(*update)
                .map_err(|message| {
                    ProtocolError::new(ProtocolErrorCode::StoreBusy, message, true)
                })?;
            Ok((ServerResponse::Accepted, Vec::new(), false))
        }
        ClientRequest::ObserveReviewRequests { observed } => {
            observed.validate().map_err(|message| {
                ProtocolError::new(ProtocolErrorCode::InvalidRequest, message, false)
            })?;
            let notifications = host
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .notifications
                .clone()
                .ok_or_else(|| {
                    ProtocolError::new(
                        ProtocolErrorCode::TerminalUnavailable,
                        "Notification delivery is unavailable",
                        true,
                    )
                })?;
            for event in observed.events() {
                notifications.record(event);
            }
            Ok((ServerResponse::Accepted, Vec::new(), false))
        }
        ClientRequest::WatchNotifications { presence }
        | ClientRequest::NotificationPresence { presence } => {
            crate::instance::validate_window_slot(&presence.window).map_err(|error| {
                ProtocolError::new(ProtocolErrorCode::InvalidRequest, error.to_string(), false)
            })?;
            if let Some(crate::notifications::Target::PullRequest(request)) =
                &presence.focused_target
            {
                request.validate().map_err(|message| {
                    ProtocolError::new(ProtocolErrorCode::InvalidRequest, message, false)
                })?;
            }
            if presence.application == crate::notifications::Application::Ceditor
                && (!presence.sessions.is_empty() || presence.focused.is_some())
            {
                return Err(ProtocolError::new(
                    ProtocolErrorCode::InvalidRequest,
                    "Editor notification windows cannot claim provider sessions",
                    false,
                ));
            }
            if presence.sessions.len() > super::state::MAX_TERMINALS * 2
                || presence
                    .focused
                    .is_some_and(|id| !presence.sessions.contains(&id))
            {
                return Err(ProtocolError::new(
                    ProtocolErrorCode::InvalidRequest,
                    "Invalid notification window ownership",
                    false,
                ));
            }
            if presence.application == crate::notifications::Application::Cagents {
                let (registry, (mut claims, held)) = {
                    let state = host.lock().unwrap_or_else(|p| p.into_inner());
                    state.ensure_retention_available(&presence.sessions)?;
                    (
                        state.execution_registry.clone(),
                        state.partition_notification_sessions(&presence),
                    )
                };
                // The shared control reader spans selection and publication;
                // automatic cleanup takes its writer before reserving an ID.
                // Disconnect is not card closure, so these pins outlive it.
                if let Some(registry) = registry {
                    let internal =
                        |message| ProtocolError::new(ProtocolErrorCode::Internal, message, true);
                    let mut registry = registry.lock().unwrap_or_else(|p| p.into_inner());
                    // Windows republish on any workspace change. Only a new
                    // claim, or a pin that a close removed, needs a write, and
                    // rotating any other would refuse its close in progress.
                    claims.extend(registry.missing_retention_pins(&held).map_err(internal)?);
                    if !claims.is_empty() {
                        registry.pin_window_terminals(&claims).map_err(internal)?;
                    }
                }
                // Missing Registry keeps the old UI delivery behavior. The
                // automatic census separately refuses unknown ownership.
            }
            let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
            // Watch and subsequent focus reports share the same live window
            // identity, even though short control requests use another stream.
            if notification_watch {
                let activation = state.watch_notifications(client_id, presence);
                Ok((
                    ServerResponse::Notifications {
                        snapshot: state.notification_snapshot(),
                    },
                    activation
                        .into_iter()
                        .map(|target| ServerMessage::NotificationActivated { target })
                        .collect(),
                    false,
                ))
            } else {
                state.notification_presence(presence);
                Ok((ServerResponse::Accepted, Vec::new(), false))
            }
        }
        ClientRequest::ReadNotifications => {
            let state = host.lock().unwrap_or_else(|p| p.into_inner());
            Ok((
                ServerResponse::Notifications {
                    snapshot: state.notification_snapshot(),
                },
                Vec::new(),
                false,
            ))
        }
        ClientRequest::NotificationPreferencesChanged { preferences } => {
            if let Some(notifications) =
                &host.lock().unwrap_or_else(|p| p.into_inner()).notifications
            {
                notifications.reload(preferences);
            }
            Ok((ServerResponse::Accepted, Vec::new(), false))
        }
        ClientRequest::SendTestNotification => {
            let answer = host
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .notifications
                .as_ref()
                .map(|notifications| notifications.test());
            // Waited for here, on this connection's own thread and without the
            // host lock, so only the person who asked waits on the desktop.
            let outcome = match answer {
                None => crate::notifications::TestOutcome::Unavailable(
                    "this daemon has no notification service".into(),
                ),
                Some(answer) => match answer.recv_timeout(crate::notifications::TEST_TIMEOUT) {
                    Ok(outcome) => outcome,
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                        crate::notifications::TestOutcome::TimedOut
                    }
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                        crate::notifications::TestOutcome::Unavailable(
                            "the notification service stopped".into(),
                        )
                    }
                },
            };
            Ok((
                ServerResponse::NotificationTest { outcome },
                Vec::new(),
                false,
            ))
        }
        ClientRequest::AcknowledgeNotification { id } => {
            if let Some(notifications) =
                &host.lock().unwrap_or_else(|p| p.into_inner()).notifications
            {
                notifications.acknowledge(id);
            }
            Ok((ServerResponse::Accepted, Vec::new(), false))
        }
        ClientRequest::AcknowledgeSessionNotifications { session } => {
            if let Some(notifications) =
                &host.lock().unwrap_or_else(|p| p.into_inner()).notifications
            {
                notifications.acknowledge_session(session);
            }
            Ok((ServerResponse::Accepted, Vec::new(), false))
        }
        ClientRequest::BuildInfo => Ok((
            ServerResponse::BuildInfo {
                version: crate::version().into(),
            },
            Vec::new(),
            false,
        )),
        ClientRequest::StartupState => Ok((
            ServerResponse::StartupState(
                host.lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .startup_state(),
            ),
            Vec::new(),
            false,
        )),
        ClientRequest::PrepareUpgrade {
            capability,
            relaunch_agents,
        } => {
            let (terminals, chats) =
                super::state::prepare_upgrade(host, capability, relaunch_agents)?;
            Ok((
                ServerResponse::UpgradePrepared { terminals, chats },
                Vec::new(),
                true,
            ))
        }
        ClientRequest::UpgradeImpact { policy } => {
            let impact = super::state::update::inspect(host, policy, false)?;
            Ok((ServerResponse::UpgradeImpact(impact), Vec::new(), false))
        }
        ClientRequest::PrepareConfirmedUpgrade {
            impact,
            relaunch_agents,
        } => {
            let current = super::state::update::inspect(host, impact.policy, true)?;
            if !impact.preserves_consent(&current) || !current.ready() {
                return Err(ProtocolError::new(
                    ProtocolErrorCode::InvalidRequest,
                    "Update impact changed or sessions are not ready; review the current update preview",
                    false,
                ));
            }
            let (terminals, chats) = super::state::prepare_preserving_upgrade(
                host,
                current.capability,
                relaunch_agents,
            )?;
            Ok((
                ServerResponse::UpgradePrepared { terminals, chats },
                Vec::new(),
                true,
            ))
        }
        ClientRequest::ResolveUpgradeChat { source } => {
            let session = super::chat::upgrade_resolution::resolve(host, client_id, &source)?;
            Ok((
                ServerResponse::UpgradeChatResolved { session },
                Vec::new(),
                false,
            ))
        }
        ClientRequest::RefreshAgentActivity { terminal_id } => {
            // Retained on the wire for existing clients; observation is owned
            // by the daemon and this request only reads its current snapshot.
            let state = host.lock().unwrap_or_else(|p| p.into_inner());
            let activity = state
                .surface_handle(client_id, terminal_id)?
                .agent_activity();
            Ok((
                ServerResponse::AgentActivity {
                    terminal_id,
                    activity,
                },
                Vec::new(),
                false,
            ))
        }
        ClientRequest::ChatCloseRetained { session } => {
            super::chat::close_retained_from_pane(host, &session).map_err(|message| {
                ProtocolError::new(ProtocolErrorCode::TerminalUnavailable, message, true)
            })?;
            Ok((ServerResponse::Accepted, Vec::new(), false))
        }
        ClientRequest::ChatLostProcesses { session, end } => {
            let processes =
                super::chat::lost_processes(host, &session, &end).map_err(|message| {
                    ProtocolError::new(ProtocolErrorCode::TerminalUnavailable, message, true)
                })?;
            Ok((
                ServerResponse::ChatLostProcesses { processes },
                Vec::new(),
                false,
            ))
        }
        request @ (ClientRequest::ChatStart { .. }
        | ClientRequest::ChatStartInPane { .. }
        | ClientRequest::ChatPrepareResume { .. }
        | ClientRequest::ChatRecoverAccount { .. }
        | ClientRequest::ChatPreview { .. }
        | ClientRequest::ChatCandidates { .. }
        | ClientRequest::ChatStartSelected { .. }
        | ClientRequest::ChatAttach { .. }
        | ClientRequest::ChatDetach { .. }
        | ClientRequest::ChatCommand { .. }
        | ClientRequest::ChatDeliver { .. }
        | ClientRequest::ChatVisibility { .. }
        | ClientRequest::ChatAcknowledge { .. }
        | ClientRequest::ChatRead { .. }
        | ClientRequest::ChatClose { .. }
        | ClientRequest::ChatReturnTerminal { .. }
        | ClientRequest::ChatCommitTerminal { .. }
        | ClientRequest::ChatList
        | ClientRequest::ChatCapacityInspect
        | ClientRequest::ChatCapacitySet { .. }
        | ClientRequest::ChatReleaseInspect { .. }
        | ClientRequest::ChatRelease { .. }) => {
            let response = super::chat::process(host, client_id, request)?;
            let mut extras = Vec::new();
            if let ServerResponse::ChatAttached { session, .. } = &response {
                // The courier tells a new subscriber the conversation's usage limit.
                super::agent_execution::mailbox::wake(host);
                // A subscriber hears of addresses only as they are learned,
                // so it is told those known already. They only accumulate, so
                // this and a concurrent change may arrive in either order.
                extras.extend(super::chat::agent_addresses(host, session.id));
                extras.extend(super::chat::active_model(host, session.id));
            }
            Ok((response, extras, false))
        }
        ClientRequest::Connect { .. } => Err(ProtocolError::new(
            ProtocolErrorCode::AlreadyConnected,
            "this client stream already completed its handshake",
            false,
        )),
        ClientRequest::List => {
            let terminals = host
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .list(client_id);
            Ok((
                listed_in_one_reply(terminals, LIST_REPLY_LIMIT)?,
                Vec::new(),
                false,
            ))
        }
        ClientRequest::ListPage { after } => {
            let (terminals, complete) = host
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .list_page(client_id, after, LIST_PAGE_BYTES);
            Ok((
                ServerResponse::ListedPage {
                    terminals,
                    complete,
                },
                Vec::new(),
                false,
            ))
        }
        ClientRequest::ListResidentTerminals => {
            let (terminals, transcripts) = host
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .resident_terminals();
            Ok((
                resident_in_one_reply(
                    ServerResponse::ResidentTerminals {
                        terminals,
                        transcripts,
                    },
                    LIST_REPLY_LIMIT,
                )?,
                Vec::new(),
                false,
            ))
        }
        ClientRequest::WatchTerminals => {
            host.lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .watch_terminals(client_id);
            Ok((ServerResponse::Accepted, Vec::new(), false))
        }
        ClientRequest::WatchWorkspaceModel => {
            let (model, closed) = super::workspace::subscriber_snapshot(host, Some(client_id))?;
            Ok((
                ServerResponse::WorkspaceSnapshot { model, closed },
                Vec::new(),
                false,
            ))
        }
        ClientRequest::ReadWorkspaceModel => {
            let model = super::workspace::snapshot(host, None)?;
            Ok((ServerResponse::WorkspaceModel { model }, Vec::new(), false))
        }
        ClientRequest::ReadCapturedTerminalRemovalBasis { capture } => {
            let writer = super::workspace::writer(host)?;
            let mut registry = writer.lock().unwrap_or_else(|p| p.into_inner());
            let admission = registry
                .source_admission()
                .map_err(|error| ProtocolError::new(ProtocolErrorCode::Internal, error, true))?;
            admission
                .validate(&capture)
                .map_err(|_| source_cleanup_conflict())?;
            let crate::model::CardKind::Terminal(source) = &capture.before.kind else {
                return Err(source_cleanup_conflict());
            };
            let basis = host
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .terminal_removal_basis(source)
                .map_err(|error| {
                    if error.message == super::model::Registry::VIEW_REMOVAL_CONFLICT {
                        source_cleanup_conflict()
                    } else {
                        error
                    }
                })?;
            Ok((
                ServerResponse::TerminalRemovalBasis(basis),
                Vec::new(),
                false,
            ))
        }
        ClientRequest::ReadTerminalRemovalBasis { source } => {
            let basis = host
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .terminal_removal_basis(&source)?;
            Ok((
                ServerResponse::TerminalRemovalBasis(basis),
                Vec::new(),
                false,
            ))
        }
        ClientRequest::ReadWorkspaceMetadata { query } => {
            let metadata = super::workspace::metadata(host, query)?;
            Ok((
                ServerResponse::WorkspaceMetadata(metadata),
                Vec::new(),
                false,
            ))
        }
        ClientRequest::ReadWorkspaceSnapshot => {
            let (model, closed) = super::workspace::subscriber_snapshot(host, None)?;
            Ok((
                ServerResponse::WorkspaceSnapshot { model, closed },
                Vec::new(),
                false,
            ))
        }
        ClientRequest::ReadWorkspaceView { scope } => {
            let (view, sequence) = super::workspace::view(host, &scope)?;
            Ok((
                ServerResponse::WorkspaceView { view, sequence },
                Vec::new(),
                false,
            ))
        }
        ClientRequest::ApplyWorkspaceEdit { edit } => {
            let receipt = super::workspace::edit_delta(host, client_id, edit)?;
            crate::daemon::wake();
            Ok((ServerResponse::WorkspaceEdit(receipt), Vec::new(), false))
        }
        ClientRequest::MovePane {
            pane,
            destination,
            operation,
        } => {
            let moved = super::workspace::move_pane(host, client_id, pane, destination, operation)?;
            crate::daemon::wake();
            Ok((ServerResponse::PaneMoved(moved), Vec::new(), false))
        }
        ClientRequest::EditWorkspaceModel { edit } => {
            let model = super::workspace::edit(host, client_id, edit)?;
            crate::daemon::wake();
            Ok((ServerResponse::WorkspaceModel { model }, Vec::new(), false))
        }
        ClientRequest::Spawn { session, options } => {
            super::agent_execution::terminals::adopt(host, client_id, session.id)?;
            let terminal = spawn_terminal(host, client_id, session, options)?;
            {
                let state = host.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                // Every other observer learns that this terminal exists here,
                // and only here: a spawn is the one inventory edge that no
                // redraw, event or snapshot broadcast implies.
                state.announce_created(client_id, terminal.session.id);
            };
            let snapshot = super::state::client_snapshot(host, client_id, terminal.session.id, 0)?;
            let mut extras = snapshot
                .map(|snapshot| ServerMessage::Snapshot {
                    terminal_id: terminal.session.id,
                    snapshot,
                })
                .into_iter()
                .collect::<Vec<_>>();
            extras.push(ServerMessage::Synchronized {
                terminal_id: terminal.session.id,
                generation: terminal.generation,
                event_sequence: terminal.event_sequence,
            });
            Ok((ServerResponse::Spawned { terminal }, extras, false))
        }
        ClientRequest::RetryPendingTerminalSave { pane, terminal_id } => {
            super::state::retry_pending_terminal_save(host, pane, terminal_id)?;
            Ok((ServerResponse::Accepted, Vec::new(), false))
        }
        ClientRequest::CompletePendingTerminal {
            pane,
            terminal_id,
            options,
        } => {
            let terminal =
                super::state::spawn_pending_terminal(host, client_id, pane, terminal_id, options)?;
            {
                let state = host.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                // Every other observer learns that this terminal exists here,
                // and only here: a spawn is the one inventory edge that no
                // redraw, event or snapshot broadcast implies.
                state.announce_created(client_id, terminal.session.id);
            };
            let snapshot = super::state::client_snapshot(host, client_id, terminal.session.id, 0)?;
            let mut extras = snapshot
                .map(|snapshot| ServerMessage::Snapshot {
                    terminal_id: terminal.session.id,
                    snapshot,
                })
                .into_iter()
                .collect::<Vec<_>>();
            extras.push(ServerMessage::Synchronized {
                terminal_id: terminal.session.id,
                generation: terminal.generation,
                event_sequence: terminal.event_sequence,
            });
            Ok((ServerResponse::Spawned { terminal }, extras, false))
        }
        ClientRequest::Attach {
            terminal_id,
            after_generation,
            after_event_sequence,
        } => {
            let (terminal, events, worker) = {
                let mut state = host.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                let terminal = state.attach(client_id, terminal_id)?;
                state.resume_presentation(client_id, terminal_id, after_generation);
                let events = state.replay_events(terminal_id, after_event_sequence)?;
                let worker = state.worker_marker(terminal_id)?;
                (terminal, events, worker)
            };
            let snapshot =
                super::state::client_snapshot(host, client_id, terminal_id, after_generation)?;
            let mut extras = replay_messages(
                terminal_id,
                events,
                snapshot,
                terminal.generation,
                terminal.event_sequence,
            );
            extras.insert(0, worker);
            Ok((ServerResponse::Attached { terminal }, extras, false))
        }
        ClientRequest::Detach { terminal_id } => {
            host.lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .detach(client_id, terminal_id)?;
            Ok((ServerResponse::Detached, Vec::new(), false))
        }
        ClientRequest::Input { terminal_id, bytes } => {
            if bytes.len() > MAX_INPUT_BYTES {
                return Err(ProtocolError::new(
                    ProtocolErrorCode::InvalidRequest,
                    format!("terminal input exceeds the {MAX_INPUT_BYTES}-byte limit"),
                    false,
                ));
            }
            let handle = host
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .surface_handle(client_id, terminal_id)?;
            handle.input(bytes).map_err(send_error)?;
            Ok((ServerResponse::Accepted, Vec::new(), false))
        }
        ClientRequest::Paste {
            terminal_id,
            text,
            origin,
        } => {
            if text.len() > MAX_INPUT_BYTES {
                return Err(ProtocolError::new(
                    ProtocolErrorCode::InvalidRequest,
                    format!("terminal paste exceeds the {MAX_INPUT_BYTES}-byte limit"),
                    false,
                ));
            }
            let handle = host
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .surface_handle(client_id, terminal_id)?;
            handle.paste(text, origin).map_err(send_error)?;
            Ok((ServerResponse::Accepted, Vec::new(), false))
        }
        ClientRequest::Key { terminal_id, input } => {
            if input
                .text
                .as_ref()
                .is_some_and(|text| text.len() > MAX_INPUT_BYTES)
            {
                return Err(ProtocolError::new(
                    ProtocolErrorCode::InvalidRequest,
                    format!("terminal key text exceeds the {MAX_INPUT_BYTES}-byte limit"),
                    false,
                ));
            }
            let handle = host
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .surface_handle(client_id, terminal_id)?;
            handle.key(input).map_err(send_error)?;
            Ok((ServerResponse::Accepted, Vec::new(), false))
        }
        ClientRequest::Mouse { terminal_id, input } => {
            if !input.x.is_finite()
                || !input.y.is_finite()
                || input.x < 0.0
                || input.y < 0.0
                || input.screen_width == 0
                || input.screen_height == 0
                || input.cell_width == 0
                || input.cell_height == 0
                || input.screen_width > 65_536
                || input.screen_height > 65_536
                || input.cell_width > 4096
                || input.cell_height > 4096
                || input.x > input.screen_width as f32
                || input.y > input.screen_height as f32
            {
                return Err(ProtocolError::new(
                    ProtocolErrorCode::InvalidRequest,
                    "terminal mouse coordinates or dimensions are outside supported bounds",
                    false,
                ));
            }
            let handle = host
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .surface_handle(client_id, terminal_id)?;
            handle.mouse(input).map_err(send_error)?;
            Ok((ServerResponse::Accepted, Vec::new(), false))
        }
        ClientRequest::Focus {
            terminal_id,
            focused,
        } => {
            host.lock()
                .unwrap_or_else(|p| p.into_inner())
                .focus_client(client_id, terminal_id, focused)?;
            Ok((ServerResponse::Accepted, Vec::new(), false))
        }
        ClientRequest::SetTheme { terminal_id, theme } => {
            let handle = host
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .history_handle(client_id, terminal_id)?;
            handle.set_theme(theme).map_err(send_error)?;
            Ok((ServerResponse::Accepted, Vec::new(), false))
        }
        ClientRequest::SearchTerminal {
            terminal_id,
            request,
            query,
            case_sensitive,
        } => {
            if query.len() > 4096 {
                return Err(ProtocolError::new(
                    ProtocolErrorCode::InvalidRequest,
                    "Terminal search query is too long",
                    false,
                ));
            }
            let handle = host
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .history_handle(client_id, terminal_id)?;
            handle
                .search_for(Some(client_id), request, query, case_sensitive)
                .map_err(send_error)?;
            Ok((ServerResponse::Accepted, Vec::new(), false))
        }
        ClientRequest::ClearTerminalScrollback { terminal_id } => {
            let handle = host
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .history_handle(client_id, terminal_id)?;
            handle.clear_scrollback().map_err(send_error)?;
            Ok((ServerResponse::Accepted, Vec::new(), false))
        }
        ClientRequest::CopyTerminalSelection {
            terminal_id,
            request,
            selection,
        } => {
            let handle = host
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .history_handle(client_id, terminal_id)?;
            handle
                .copy_selection(request, selection)
                .map_err(send_error)?;
            Ok((ServerResponse::Accepted, Vec::new(), false))
        }
        ClientRequest::Scroll {
            terminal_id,
            scroll,
        } => {
            if matches!(
                scroll,
                TerminalScroll::Lines(delta) if !(-1_000_000..=1_000_000).contains(&delta)
            ) {
                return Err(ProtocolError::new(
                    ProtocolErrorCode::InvalidRequest,
                    "terminal scroll delta is outside supported bounds",
                    false,
                ));
            }
            host.lock()
                .unwrap_or_else(|p| p.into_inner())
                .scroll_client(client_id, terminal_id, scroll)?;
            Ok((ServerResponse::Accepted, Vec::new(), false))
        }
        ClientRequest::Resize { terminal_id, size } => {
            if size.columns == 0
                || size.rows == 0
                || size.columns > 1000
                || size.rows > 500
                || size.cell_width_px == 0
                || size.cell_height_px == 0
                || size.cell_width_px > 4096
                || size.cell_height_px > 4096
            {
                return Err(ProtocolError::new(
                    ProtocolErrorCode::InvalidRequest,
                    "terminal dimensions are outside supported bounds",
                    false,
                ));
            }
            host.lock()
                .unwrap_or_else(|p| p.into_inner())
                .resize_client(client_id, terminal_id, size.into())?;
            Ok((ServerResponse::Accepted, Vec::new(), false))
        }
        ClientRequest::SetVisibility {
            terminal_id,
            visibility,
        } => {
            host.lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .set_visibility(client_id, terminal_id, visibility)?;
            Ok((ServerResponse::Accepted, Vec::new(), false))
        }
        ClientRequest::Hibernate { terminal_id } => {
            host.lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .set_visibility(client_id, terminal_id, TerminalVisibility::Hibernated)?;
            Ok((ServerResponse::Accepted, Vec::new(), false))
        }
        ClientRequest::Resume {
            terminal_id,
            visible,
        } => {
            let visibility = if visible {
                TerminalVisibility::Visible
            } else {
                TerminalVisibility::Hidden
            };
            host.lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .set_visibility(client_id, terminal_id, visibility)?;
            Ok((ServerResponse::Accepted, Vec::new(), false))
        }
        ClientRequest::Snapshot {
            terminal_id,
            after_generation,
        } => {
            let snapshot =
                super::state::client_snapshot(host, client_id, terminal_id, after_generation)?;
            Ok((
                ServerResponse::Snapshot {
                    terminal_id,
                    snapshot,
                },
                Vec::new(),
                false,
            ))
        }
        ClientRequest::TerminalAcknowledge { terminal_id, epoch } => {
            host.lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .acknowledge_output(client_id, terminal_id, epoch)?;
            Ok((ServerResponse::Accepted, Vec::new(), false))
        }
        ClientRequest::AcknowledgeRedraw {
            terminal_id,
            generation,
        } => {
            let handle = host
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .redraw_ack_handle(client_id, terminal_id, generation)?;
            let source = host
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .redraw_source(client_id, terminal_id, generation);
            handle.acknowledge_redraw(source);
            Ok((ServerResponse::Accepted, Vec::new(), false))
        }
        ClientRequest::ShutdownTerminal { terminal_id } => {
            let (handle, source) = {
                let state = host.lock().unwrap_or_else(|p| p.into_inner());
                state.ensure_retention_available(&[terminal_id])?;
                (
                    state.history_handle(client_id, terminal_id)?,
                    state.terminals[&terminal_id].session.clone(),
                )
            };
            let paired = terminal_close_pair(host, terminal_id, &handle)?;
            let pin = retention_pin_generation(host, terminal_id);
            drop(_shared);
            let close = reserve_terminal_close(
                host,
                terminal_id,
                &handle,
                &source,
                paired.as_ref(),
                pin,
                None,
            )?;
            // Explicit shutdown permits active work, but still needs the same
            // inspected family cleanup proof as a checked close. A plain worker
            // hangup/join cannot certify release of its original holder.
            match handle.close_checked(true) {
                crate::terminal_ipc::TerminalCloseResult::Closed => {}
                crate::terminal_ipc::TerminalCloseResult::Active => {
                    return Err(ProtocolError::new(
                        ProtocolErrorCode::TerminalUnavailable,
                        "Terminal process cleanup remains unconfirmed; its original owner is retained",
                        true,
                    ));
                }
                crate::terminal_ipc::TerminalCloseResult::Unknown(detail) => {
                    return Err(ProtocolError::new(
                        ProtocolErrorCode::TerminalUnavailable,
                        detail,
                        true,
                    ));
                }
            }
            host.lock()
                .unwrap_or_else(|p| p.into_inner())
                .close_terminal_attention(terminal_id, Some(&handle));
            if let Some(paired) = paired {
                super::chat::close_terminal_actor(host, terminal_id, &paired)?;
            }
            finish_terminal_close(host, close)?;
            Ok((ServerResponse::Accepted, Vec::new(), false))
        }
        ClientRequest::ReleaseCompletedTerminal {
            session,
            allow_active,
        } => {
            if !matches!(
                session.continuity,
                crate::model::TerminalContinuity::Exited { .. }
                    | crate::model::TerminalContinuity::RelaunchAvailable {
                        last_exit: Some(_),
                        ..
                    }
            ) {
                return Err(ProtocolError::new(
                    ProtocolErrorCode::InvalidRequest,
                    "Only an ended terminal result may be retired",
                    false,
                ));
            }
            let (handle, paired, source) = {
                let state = host.lock().unwrap_or_else(|p| p.into_inner());
                validate_cleanup_source(&state, cleanup_source.as_ref())?;
                state.ensure_retention_available(&[session.id])?;
                let handle = match state.terminals.get(&session.id) {
                    None => {
                        return Err(ProtocolError::new(
                            ProtocolErrorCode::TerminalUnavailable,
                            "The completed terminal is no longer available; absence does not confirm cleanup",
                            true,
                        ));
                    }
                    Some(record)
                        if record.session.launch == session.launch
                            && record.handle.is_completed() =>
                    {
                        record.handle.clone()
                    }
                    Some(_) => {
                        return Err(ProtocolError::new(
                            ProtocolErrorCode::InvalidRequest,
                            "The terminal changed or still owns work; nothing was closed",
                            false,
                        ));
                    }
                };
                let paired = state.chats.get(&session.id).map(|record| {
                    if !record.paired_terminal.as_ref().is_some_and(|expected| {
                        handle.same_worker(expected)
                    }) || (record.session.launch.provider != crate::agent::Provider::Codex
                        && !record.owns_execution_console())
                        || !record.session.terminal.as_ref().is_some_and(|terminal| {
                            terminal.id == session.id && terminal.launch == session.launch
                        })
                    {
                        return Err(ProtocolError::new(ProtocolErrorCode::InvalidRequest,
                            "The completed terminal no longer identifies its native conversation", false));
                    }
                    Ok(record.handle.clone())
                }).transpose()?;
                (handle, paired, state.terminals[&session.id].session.clone())
            };
            let pin = retention_pin_generation(host, session.id);
            if paired.as_ref().is_some_and(|handle| handle.owns_work()) && !allow_active {
                return Ok((
                    ServerResponse::TerminalCloseChecked {
                        terminal_id: session.id,
                        outcome: crate::terminal_ipc::TerminalCloseResult::Active,
                    },
                    Vec::new(),
                    false,
                ));
            }
            drop(_shared);
            let close = reserve_terminal_close(
                host,
                session.id,
                &handle,
                &source,
                paired.as_ref(),
                pin,
                cleanup_capture.as_ref(),
            )?;
            if let Some(paired) = paired {
                super::chat::close_terminal_actor(host, session.id, &paired)?;
            }
            handle.shutdown().map_err(send_error)?;
            host.lock()
                .unwrap_or_else(|p| p.into_inner())
                .close_terminal_attention(session.id, Some(&handle));
            finish_terminal_close(host, close)?;
            Ok((
                ServerResponse::TerminalCloseChecked {
                    terminal_id: session.id,
                    outcome: crate::terminal_ipc::TerminalCloseResult::Closed,
                },
                Vec::new(),
                false,
            ))
        }
        ClientRequest::CloseTerminalChecked {
            terminal_id,
            allow_active,
        } => {
            let (handle, source) = {
                let state = host.lock().unwrap_or_else(|p| p.into_inner());
                validate_cleanup_source(&state, cleanup_source.as_ref())?;
                state.ensure_retention_available(&[terminal_id])?;
                (
                    state.history_handle(client_id, terminal_id)?,
                    state.terminals[&terminal_id].session.clone(),
                )
            };
            let paired = terminal_close_pair(host, terminal_id, &handle)?;
            let pin = retention_pin_generation(host, terminal_id);
            drop(_shared);
            let mut close = reserve_terminal_close(
                host,
                terminal_id,
                &handle,
                &source,
                paired.as_ref(),
                pin,
                cleanup_capture.as_ref(),
            )?;
            let outcome = if cleanup_source.is_some()
                && !allow_active
                && paired.as_ref().is_some_and(|actor| actor.owns_work())
            {
                crate::terminal_ipc::TerminalCloseResult::Active
            } else {
                handle.close_checked(allow_active)
            };
            if outcome == crate::terminal_ipc::TerminalCloseResult::Closed {
                host.lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .close_terminal_attention(terminal_id, Some(&handle));
                if let Some(paired) = paired {
                    super::chat::close_terminal_actor(host, terminal_id, &paired)?;
                }
                finish_terminal_close(host, close)?;
            } else if outcome == crate::terminal_ipc::TerminalCloseResult::Active {
                let removed = {
                    let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
                    if state
                        .terminal_retirements
                        .get(&terminal_id)
                        .is_some_and(|entry| entry.ticket == close.ticket)
                    {
                        state.inventory_generation += 1;
                        let removed = state.terminal_retirements.remove(&terminal_id);
                        if let Some(previous) = close.previous.take() {
                            state.terminal_retirements.insert(terminal_id, previous);
                        }
                        removed
                    } else {
                        None
                    }
                };
                drop(removed);
            }
            Ok((
                ServerResponse::TerminalCloseChecked {
                    terminal_id,
                    outcome,
                },
                Vec::new(),
                false,
            ))
        }
        ClientRequest::CloseTerminalSource { .. } => unreachable!("source close normalized above"),
        ClientRequest::Shutdown => {
            let outcome = host
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .shutdown(
                    ShutdownOperation::Stop {
                        require_empty: false,
                        expected_generation: None,
                    },
                    Instant::now(),
                );
            if matches!(outcome, ShutdownOutcome::Accepted { .. }) {
                Ok((ServerResponse::ShutdownAccepted, Vec::new(), true))
            } else {
                Ok((ServerResponse::ShutdownOutcome(outcome), Vec::new(), false))
            }
        }
        ClientRequest::ShutdownV1 { version, operation } => {
            if version != SHUTDOWN_VERSION {
                return Err(ProtocolError::new(
                    ProtocolErrorCode::UnsupportedVersion,
                    format!(
                        "shutdown version {version} is unsupported; expected {SHUTDOWN_VERSION}"
                    ),
                    false,
                ));
            }
            let mut state = host.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            let outcome = if matches!(operation, ShutdownOperation::Preflight) {
                match state.shutdown_preflight(Instant::now()) {
                    Ok(preflight) => {
                        return Ok((
                            ServerResponse::ShutdownPreflight(preflight),
                            Vec::new(),
                            false,
                        ));
                    }
                    Err(outcome) => outcome,
                }
            } else {
                state.shutdown(operation, Instant::now())
            };
            let shutdown = matches!(outcome, ShutdownOutcome::Accepted { .. });
            Ok((
                ServerResponse::ShutdownOutcome(outcome),
                Vec::new(),
                shutdown,
            ))
        }
    }
}

fn prepare_resume(
    host: &SharedHost,
    client_id: ClientId,
    cursors: Vec<ResumeCursor>,
) -> Result<
    (
        Vec<crate::terminal_ipc::TerminalDescriptor>,
        Vec<ServerMessage>,
    ),
    ProtocolError,
> {
    let gate = control_gate(host);
    let _control = gate.shared();
    ensure_inspection_open(host).map_err(|mut failure| {
        // A refused Connect has admitted no mutations or uncertain writes.
        // Only this shutdown gate is retryable; expected-host/proof validation
        // below retains its permanent identity refusal.
        failure.retryable = true;
        failure
    })?;
    let mut resumed = Vec::new();
    let mut replay = Vec::new();
    for cursor in cursors {
        if super::account_lifecycle::ensure_terminal(host, cursor.terminal_id).is_err() {
            continue;
        }
        let (terminal, events, worker) = {
            let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
            let Ok(terminal) = state.attach(client_id, cursor.terminal_id) else {
                continue;
            };
            state.resume_presentation(client_id, cursor.terminal_id, cursor.generation);
            let events = state
                .replay_events(cursor.terminal_id, cursor.event_sequence)
                .unwrap_or_default();
            let worker = state.worker_marker(cursor.terminal_id)?;
            (terminal, events, worker)
        };
        let snapshot =
            super::state::client_snapshot(host, client_id, cursor.terminal_id, cursor.generation)?;
        let mut messages = replay_messages(
            cursor.terminal_id,
            events,
            snapshot,
            terminal.generation,
            terminal.event_sequence,
        );
        messages.insert(0, worker);
        let prepared = (terminal, messages);
        resumed.push(prepared.0);
        replay.extend(prepared.1);
    }
    Ok((resumed, replay))
}

fn replay_messages(
    terminal_id: crate::model::TerminalId,
    events: Vec<(u64, WireTerminalEvent)>,
    snapshot: Option<TerminalSnapshot>,
    generation: u64,
    event_sequence: u64,
) -> Vec<ServerMessage> {
    let mut messages = events
        .into_iter()
        .map(|(sequence, event)| ServerMessage::Event {
            terminal_id,
            sequence,
            event,
        })
        .collect::<Vec<_>>();
    if let Some(snapshot) = snapshot {
        messages.push(ServerMessage::Snapshot {
            terminal_id,
            snapshot,
        });
    }
    messages.push(ServerMessage::Synchronized {
        terminal_id,
        generation,
        event_sequence,
    });
    messages
}

fn current_proof(host: &SharedHost) -> HostSessionProof {
    host.lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .proof()
        .clone()
}

fn push_response(
    delivery: &Arc<DeliveryQueue>,
    proof: &HostSessionProof,
    request_id: u64,
    response: impl Into<crate::terminal_ipc::PreparedResponse>,
) -> Result<(), ()> {
    delivery
        .push(super::delivery::Outbound::response(
            proof,
            request_id,
            response.into(),
        ))
        .map_err(|_| ())
}

fn push_error(
    delivery: &Arc<DeliveryQueue>,
    host: &SharedHost,
    request_id: u64,
    error: ProtocolError,
) -> Result<(), ()> {
    delivery
        .push(ServerEnvelope {
            version: PROTOCOL_VERSION,
            request_id: Some(request_id),
            proof: current_proof(host),
            message: ServerMessage::Error(error),
        })
        .map_err(|_| ())
}

fn writer_loop(mut stream: UnixStream, delivery: Arc<DeliveryQueue>) {
    let mut codec = FrameCodec::new(MAX_FRAME_BYTES);
    while let Some(message) = delivery.pop() {
        if let Err(error) = message.write(&mut codec, &mut stream) {
            // Queued frames cannot be delivered, so there is nothing to drain
            // and the peer only sees the EOF below; `abort` records why.
            delivery.abort(write_failure(&error));
            break;
        }
    }
    let _ = stream.shutdown(Shutdown::Both);
}

/// Why a frame never reached the client, in the words the daemon log uses.
fn write_failure(error: &FrameError) -> String {
    match error {
        FrameError::Io(error)
            if matches!(
                error.kind(),
                io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
            ) =>
        {
            "the client socket could not be written within its send timeout".to_owned()
        }
        FrameError::Io(error)
            if matches!(
                error.kind(),
                io::ErrorKind::BrokenPipe | io::ErrorKind::ConnectionReset
            ) =>
        {
            "the client closed its connection".to_owned()
        }
        error => format!("a frame could not be written to the client: {error}"),
    }
}

#[cfg(test)]
pub(in crate::host) mod tests;
