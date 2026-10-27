use std::{
    io::{self, Read},
    net::Shutdown,
    os::unix::net::UnixStream,
    sync::{Arc, RwLock},
    thread,
    time::{Duration, Instant},
};

use crate::{
    model::HostSessionProof,
    terminal_ipc::{
        ClientEnvelope, ClientRequest, FrameCodec, FrameError, MAX_CLIENT_FRAME_BYTES,
        MAX_FRAME_BYTES, MAX_INPUT_BYTES, MAX_RESUME_TERMINALS, PROTOCOL_VERSION, ProtocolError,
        ProtocolErrorCode, ResumeCursor, SHUTDOWN_VERSION, ServerEnvelope, ServerMessage,
        ServerResponse, ShutdownOperation, ShutdownOutcome, TerminalScroll, TerminalSnapshot,
        TerminalVisibility, WireTerminalEvent, read_frame_with_limit, validate_proof,
    },
};

use super::{
    delivery::DeliveryQueue,
    state::{ClientId, SharedHost, send_error, spawn_terminal},
};

#[cfg(all(test, target_os = "linux"))]
mod explicit_authority_server_tests;

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

type RequestHandler =
    fn(&SharedHost, ClientId, ClientRequest) -> Result<RequestOutcome, ProtocolError>;

pub(crate) fn serve(stream: UnixStream, client_id: ClientId, host: SharedHost) {
    serve_with(stream, client_id, host, process_request);
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

fn serve_with(stream: UnixStream, client_id: ClientId, host: SharedHost, handle: RequestHandler) {
    let registration = Registration::new(Arc::clone(&host), client_id);
    if let Some(pid) = peer_pid(&stream) {
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
    loop {
        let frame = if connected {
            codec.read::<ClientEnvelope>(&mut reader)
        } else {
            read_handshake_frame(&mut reader, handshake_deadline)
        };
        let Ok(Some(envelope)) = frame else {
            break;
        };
        if envelope.version != PROTOCOL_VERSION {
            let error = ProtocolError::new(
                ProtocolErrorCode::UnsupportedVersion,
                format!(
                    "protocol version {} is unsupported; expected {PROTOCOL_VERSION}",
                    envelope.version
                ),
                false,
            );
            let _ = push_error(&delivery, &host, envelope.request_id, error);
            break;
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
                if delivery
                    .push(ServerEnvelope {
                        version: PROTOCOL_VERSION,
                        request_id: None,
                        proof: proof.clone(),
                        message,
                    })
                    .is_err()
                {
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
        // Register even before ordinary exit starts: its writer can enter
        // after process_request releases control, before this reply is queued.
        // The final HostMutex seal closes acquisition before any handler effect.
        let _close_reply = if matches!(
            &request,
            ClientRequest::CloseTerminalChecked { .. }
                | ClientRequest::ReleaseCompletedTerminal { .. }
                | ClientRequest::ShutdownTerminal { .. }
                | ClientRequest::ChatClose { .. }
                | ClientRequest::ChatCloseRetained { .. }
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
                    if delivery
                        .push(ServerEnvelope {
                            version: PROTOCOL_VERSION,
                            request_id: None,
                            proof: proof.clone(),
                            message,
                        })
                        .is_err()
                    {
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

fn control_gate(host: &SharedHost) -> Arc<RwLock<()>> {
    // Never wait for admission while holding HostState: admitted operations
    // need to reacquire it after synchronous provider/PTY work.
    Arc::clone(&host.lock().unwrap_or_else(|p| p.into_inner()).control_gate)
}

pub(super) fn try_scoped_control_write(
    gate: &RwLock<()>,
) -> Result<std::sync::RwLockWriteGuard<'_, ()>, ProtocolError> {
    match gate.try_write() {
        Ok(guard) => Ok(guard),
        Err(std::sync::TryLockError::Poisoned(poisoned)) => Ok(poisoned.into_inner()),
        Err(std::sync::TryLockError::WouldBlock) => Err(ProtocolError::new(
            ProtocolErrorCode::Internal,
            "The daemon is finishing another operation. Wait for it and retry.",
            true,
        )),
    }
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

fn ensure_request_open(host: &SharedHost, request: &ClientRequest) -> Result<(), ProtocolError> {
    if !host
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .ordinary_exit_cleanup_pending()
    {
        return ensure_control_open(host);
    }
    if matches!(
        request,
        ClientRequest::BuildInfo
            | ClientRequest::List
            | ClientRequest::Attach { .. }
            | ClientRequest::Detach { .. }
            | ClientRequest::Snapshot { .. }
            | ClientRequest::TerminalAcknowledge { .. }
            | ClientRequest::AcknowledgeRedraw { .. }
            | ClientRequest::WatchTerminals
            | ClientRequest::Authenticate { .. }
            | ClientRequest::CloseTerminalChecked { .. }
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
        false,
    ))
}

fn disconnect_client(host: &SharedHost, client_id: ClientId) {
    {
        let gate = control_gate(host);
        let _control = gate.read().unwrap_or_else(|p| p.into_inner());
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
    // which must not hold up a writer queued at the gate.
    let work = host.lock().unwrap_or_else(|p| p.into_inner()).work.clone();
    if let Some(work) = work {
        work.unwatch(client_id);
    }
}

type RequestOutcome = (ServerResponse, Vec<ServerMessage>, bool);

/// Recovery is deliberately a subset of existing authority-checked operations.
/// No visibility operation retries adoption, and no new command may race a
/// retained original whose commit outcome is unknown.
fn ensure_startup_recovery_request(
    host: &SharedHost,
    request: &ClientRequest,
) -> Result<(), ProtocolError> {
    let state = host.lock().unwrap_or_else(|p| p.into_inner());
    let Some(detail) = state.startup_recovery() else {
        return Ok(());
    };
    let inspection = matches!(
        request,
        ClientRequest::List
            | ClientRequest::ChatList
            | ClientRequest::Attach { .. }
            | ClientRequest::Detach { .. }
            | ClientRequest::Snapshot { .. }
            | ClientRequest::TerminalAcknowledge { .. }
            | ClientRequest::AcknowledgeRedraw { .. }
            | ClientRequest::WatchTerminals
            | ClientRequest::Authenticate { .. }
            | ClientRequest::CloseTerminalChecked { .. }
            | ClientRequest::ChatCloseRetained { .. }
    );
    let shutdown = matches!(
        request,
        ClientRequest::Shutdown | ClientRequest::ShutdownV1 { .. }
    ) && !state.recovery_owns_work();
    if inspection || shutdown {
        return Ok(());
    }
    Err(ProtocolError::new(
        ProtocolErrorCode::TerminalUnavailable,
        format!(
            "Startup recovery retained original ownership; inspect available sessions or explicitly Close the original actor: {detail}"
        ),
        false,
    ))
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
    let result = super::agent_execution::registry(host).and_then(|registry| {
        registry
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .retention_pin_generation(terminal)
    });
    result
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

/// The caller dropped its shared reader. Drain earlier publications and reserve
/// this exact owner under the writer BEFORE any destructive close or cache release.
fn reserve_terminal_close(
    host: &SharedHost,
    terminal: crate::model::TerminalId,
    expected: &crate::terminal::TerminalHandle,
    source: &crate::model::TerminalSession,
    paired: Option<&crate::chat::runtime::Handle>,
    generation: Result<Option<String>, String>,
) -> Result<TerminalCloseRetirement, ProtocolError> {
    let gate = control_gate(host);
    let _admission = gate.write().unwrap_or_else(|p| p.into_inner());
    ensure_terminal_close_open(host)?;
    let unavailable =
        |message| ProtocolError::new(ProtocolErrorCode::TerminalUnavailable, message, true);
    let captured = generation.as_ref().map_err(|cause| {
        unavailable(format!(
            "Saved terminal ownership was unknown before close: {cause}"
        ))
    })?;
    let current = retention_pin_generation(host, terminal).map_err(|cause| {
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
    let gate = control_gate(host);
    let _admission = gate.write().unwrap_or_else(|p| p.into_inner());
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
    ensure_terminal_close_open(host)?;
    // None is a known unpinned local window, not an unavailable Registry.
    // The writer also drains publications that passed their initial check
    // before our reservation. It fences local/shared edits through the final
    // removal and conditional generation DELETE.
    let captured = close.generation.as_ref().map_err(|cause| {
        error(format!(
            "Saved terminal ownership was unknown before close: {cause}"
        ))
    })?;
    let current = retention_pin_generation(host, close.terminal).map_err(|cause| {
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

pub(super) fn process_request(
    host: &SharedHost,
    client_id: ClientId,
    request: ClientRequest,
) -> Result<RequestOutcome, ProtocolError> {
    ensure_request_open(host, &request)?;
    ensure_startup_recovery_request(host, &request)?;
    let gate = control_gate(host);
    // The GitHub read must not monopolize the exclusive admission fence. All
    // authorizing local state is checked again only after that fence is held.
    if let ClientRequest::DeviceOperation {
        operation: crate::device_service::Operation::RemoveMergedWorktree { request },
    } = &request
    {
        let prepared = {
            let _shared = gate.read().unwrap_or_else(|p| p.into_inner());
            ensure_control_open(host)?;
            crate::worktree_cleanup::PreparedRemoval::prepare(request.clone())
        };
        let removed = match prepared {
            Ok(prepared) => {
                let _exclusive = gate.write().unwrap_or_else(|p| p.into_inner());
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
            },
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
            let _shared = gate.read().unwrap_or_else(|p| p.into_inner());
            ensure_control_open(host)?;
            super::agent_execution::terminals::admit(host, client_id, &request)?;
        }
        let reservation = super::account_lifecycle::Reservation::for_operation(host, operation)?;
        let value = crate::account::remote::execute_reserved(operation.clone(), &reservation)
            .map_err(|message| {
                ProtocolError::new(ProtocolErrorCode::InvalidRequest, message, false)
            })?;
        return Ok((
            ServerResponse::DeviceOperation {
                reply: crate::device_service::Reply::AccountLifecycle(value),
            },
            Vec::new(),
            false,
        ));
    }
    // Inventory decisions and commit must wait for all previously admitted
    // effects, including provider closes that finish outside HostState.
    let exclusive = matches!(
        &request,
        ClientRequest::PrepareUpgrade { .. }
            | ClientRequest::PrepareConfirmedUpgrade { .. }
            | ClientRequest::Shutdown
            | ClientRequest::ShutdownV1 { .. }
            | ClientRequest::ChatReturnTerminal { .. }
            | ClientRequest::ChatRelease { dry_run: false, .. }
            | ClientRequest::ChatRecoverAccount {
                idle_generation: Some(_),
                ..
            }
    );
    let _exclusive = exclusive.then(|| gate.write().unwrap_or_else(|p| p.into_inner()));
    let _shared = (!exclusive).then(|| gate.read().unwrap_or_else(|p| p.into_inner()));
    ensure_request_open(host, &request)?;
    ensure_startup_recovery_request(host, &request)?;
    super::agent_execution::terminals::admit(host, client_id, &request)?;
    super::account_lifecycle::admit(host, &request)?;
    super::worktree_removal::admit(host, &request)?;
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
    let notification_watch = matches!(&request, ClientRequest::WatchNotifications { .. });
    match request {
        ClientRequest::DeviceOperation { operation } => {
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
                | crate::device_service::Operation::AccountProbe { account, .. } => {
                    super::account_lifecycle::ensure_paths(host, account.resolved_home())?;
                }
                _ => {}
            }
            let reply = crate::device_service::execute(operation).map_err(|message| {
                ProtocolError::new(ProtocolErrorCode::InvalidRequest, message, false)
            })?;
            Ok((ServerResponse::DeviceOperation { reply }, Vec::new(), false))
        }
        ClientRequest::Authenticate { authority } => {
            super::agent_execution::terminals::authenticate(host, client_id, &authority)?;
            Ok((ServerResponse::Accepted, Vec::new(), false))
        }
        ClientRequest::ChannelWork { authority, request } => {
            let response = if let crate::channel_work_api::Request::Refresh { home } = request {
                drop(_shared);
                super::channel_work::refresh(host, &authority, home)
            } else {
                super::channel_work::process(host, &authority, request)
            };
            Ok((ServerResponse::ChannelWork(response), Vec::new(), false))
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
        ClientRequest::Automation {
            authority,
            operation,
        } => {
            let reply =
                super::automations::service::process_under_control(host, authority, operation)
                    .map_err(|message| {
                        ProtocolError::new(ProtocolErrorCode::InvalidRequest, message, false)
                    })?;
            Ok((ServerResponse::Automation(reply), Vec::new(), false))
        }
        ClientRequest::Agent {
            authority,
            operation,
        } => {
            let reply =
                super::agent_execution::process(host, authority, operation).map_err(|message| {
                    ProtocolError::new(ProtocolErrorCode::InvalidRequest, message, false)
                })?;
            Ok((ServerResponse::Agent(reply), Vec::new(), false))
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
                let registry = {
                    let state = host.lock().unwrap_or_else(|p| p.into_inner());
                    state.ensure_retention_available(&presence.sessions)?;
                    state.execution_registry.clone()
                };
                // The shared control reader spans selection and publication;
                // automatic cleanup takes its writer before reserving an ID.
                // Disconnect is not card closure, so these pins are durable.
                if let Some(registry) = registry {
                    registry
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .pin_window_terminals(&presence.sessions)
                        .map_err(|message| {
                            ProtocolError::new(ProtocolErrorCode::Internal, message, true)
                        })?;
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
        ClientRequest::BuildInfo => Ok((
            ServerResponse::BuildInfo {
                version: crate::VERSION.into(),
            },
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
            super::chat::close_retained_native(host, &session).map_err(|message| {
                ProtocolError::new(ProtocolErrorCode::TerminalUnavailable, message, true)
            })?;
            Ok((ServerResponse::Accepted, Vec::new(), false))
        }
        request @ (ClientRequest::ChatStart { .. }
        | ClientRequest::ChatPrepareResume { .. }
        | ClientRequest::ChatRecoverAccount { .. }
        | ClientRequest::ChatPreview { .. }
        | ClientRequest::ChatCandidates { .. }
        | ClientRequest::ChatStartSelected { .. }
        | ClientRequest::ChatAttach { .. }
        | ClientRequest::ChatDetach { .. }
        | ClientRequest::ChatCommand { .. }
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
        | ClientRequest::ChatRelease { .. }) => Ok((
            super::chat::process(host, client_id, request)?,
            Vec::new(),
            false,
        )),
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
            Ok((ServerResponse::Listed { terminals }, Vec::new(), false))
        }
        ClientRequest::WatchTerminals => {
            host.lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .watch_terminals(client_id);
            Ok((ServerResponse::Accepted, Vec::new(), false))
        }
        ClientRequest::WatchWorkspaceModel => {
            let model = super::workspace::snapshot(host, Some(client_id))?;
            Ok((ServerResponse::WorkspaceModel { model }, Vec::new(), false))
        }
        ClientRequest::ReadWorkspaceModel => {
            let model = super::workspace::snapshot(host, None)?;
            Ok((ServerResponse::WorkspaceModel { model }, Vec::new(), false))
        }
        ClientRequest::EditWorkspaceModel { edit } => {
            let model = super::workspace::edit(host, client_id, edit)?;
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
            let (terminal, events) = {
                let mut state = host.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                let terminal = state.attach(client_id, terminal_id)?;
                state.resume_presentation(client_id, terminal_id, after_generation);
                let events = state.replay_events(terminal_id, after_event_sequence)?;
                (terminal, events)
            };
            let snapshot =
                super::state::client_snapshot(host, client_id, terminal_id, after_generation)?;
            let extras = replay_messages(
                terminal_id,
                events,
                snapshot,
                terminal.generation,
                terminal.event_sequence,
            );
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
            let close =
                reserve_terminal_close(host, terminal_id, &handle, &source, paired.as_ref(), pin)?;
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
            let close =
                reserve_terminal_close(host, session.id, &handle, &source, paired.as_ref(), pin)?;
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
                state.ensure_retention_available(&[terminal_id])?;
                (
                    state.history_handle(client_id, terminal_id)?,
                    state.terminals[&terminal_id].session.clone(),
                )
            };
            let paired = terminal_close_pair(host, terminal_id, &handle)?;
            let pin = retention_pin_generation(host, terminal_id);
            drop(_shared);
            let mut close =
                reserve_terminal_close(host, terminal_id, &handle, &source, paired.as_ref(), pin)?;
            let outcome = handle.close_checked(allow_active);
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
    let _control = gate.read().unwrap_or_else(|p| p.into_inner());
    ensure_inspection_open(host)?;
    let mut resumed = Vec::new();
    let mut replay = Vec::new();
    for cursor in cursors {
        if super::account_lifecycle::ensure_terminal(host, cursor.terminal_id).is_err() {
            continue;
        }
        let (terminal, events) = {
            let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
            let Ok(terminal) = state.attach(client_id, cursor.terminal_id) else {
                continue;
            };
            state.resume_presentation(client_id, cursor.terminal_id, cursor.generation);
            let events = state
                .replay_events(cursor.terminal_id, cursor.event_sequence)
                .unwrap_or_default();
            (terminal, events)
        };
        let snapshot =
            super::state::client_snapshot(host, client_id, cursor.terminal_id, cursor.generation)?;
        let messages = replay_messages(
            cursor.terminal_id,
            events,
            snapshot,
            terminal.generation,
            terminal.event_sequence,
        );
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
    response: ServerResponse,
) -> Result<(), ()> {
    delivery
        .push(ServerEnvelope {
            version: PROTOCOL_VERSION,
            request_id: Some(request_id),
            proof: proof.clone(),
            message: ServerMessage::Response { response },
        })
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
        if let Err(error) = codec.write(&mut stream, &message.wire()) {
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
mod tests {
    use super::*;
    use crate::model::TerminalId;

    #[test]
    fn recovery_chat_requests_preserve_ordinary_exit_and_committed_shutdown_fences() {
        for ordinary_exit in [true, false] {
            let (proof, host) = panic_test_host("recovery-chat-exit-fence");
            host.lock()
                .unwrap()
                .retain_startup_recovery("original owner requires explicit Close".into());
            // The new read-only request crosses the real dispatch path before
            // shutdown, without requiring an actor, adoption or transcript.
            assert!(matches!(
                process_request(&host, 0, ClientRequest::ChatList).unwrap().0,
                ServerResponse::ChatListed { sessions } if sessions.is_empty()
            ));
            let source = crate::chat::Session {
                id: TerminalId::new(),
                proof: Some(proof),
                terminal: None,
                native_continuity: Default::default(),
                owned_codex_listener: None,
                launch: crate::chat::Launch {
                    provider: crate::agent::Provider::Codex,
                    session_id: None,
                    recipe: crate::model::LaunchRecipe {
                        program: "/must-not-execute".into(),
                        arguments: Vec::new(),
                        working_directory: "/".into(),
                        environment: Default::default(),
                        unset_environment: Default::default(),
                    },
                },
            };
            {
                let mut state = host.lock().unwrap();
                if ordinary_exit {
                    state.begin_ordinary_exit_cleanup();
                } else {
                    // No live originals exist in this local gate fixture, so
                    // the real request_shutdown transition may commit.
                    state.request_shutdown();
                }
                assert!(state.shutting_down);
            }
            for request in [
                ClientRequest::ChatList,
                ClientRequest::ChatCloseRetained { session: source },
            ] {
                let failure = process_request(&host, 0, request).unwrap_err();
                assert_eq!(failure.code, ProtocolErrorCode::TerminalUnavailable);
                assert_eq!(
                    failure.message,
                    if ordinary_exit {
                        "Ordinary daemon exit is waiting for exact original child cleanup; only inspection and explicit checked Close remain available"
                    } else {
                        "Daemon upgrade/shutdown is committed; no further control mutations are accepted"
                    }
                );
                let state = host.lock().unwrap();
                assert!(state.shutting_down);
                assert_eq!(state.ordinary_exit_cleanup_pending(), ordinary_exit);
                assert_eq!(
                    state.startup_recovery(),
                    Some("original owner requires explicit Close")
                );
                assert!(state.chats.is_empty());
                assert!(state.chat_spawning.is_empty());
                assert!(state.native_closing.is_empty());
            }
        }
    }

    fn wait_client_retirement(mut ready: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !ready() {
            assert!(
                Instant::now() < deadline,
                "original terminal retirement did not settle"
            );
            thread::sleep(Duration::from_millis(5));
        }
    }

    fn finish_test_terminal_close(
        host: &SharedHost,
        terminal: TerminalId,
        handle: &crate::terminal::TerminalHandle,
        generation: Result<Option<String>, String>,
    ) -> Result<(), ProtocolError> {
        let source = host.lock().unwrap().terminals[&terminal].session.clone();
        let close = reserve_terminal_close(host, terminal, handle, &source, None, generation)?;
        finish_terminal_close(host, close)
    }

    fn retry_retirement_close(
        host: &SharedHost,
        request: impl Fn() -> ClientRequest,
    ) -> RequestOutcome {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match process_request(host, 1, request()) {
                Ok(reply) => return reply,
                Err(error)
                    if error.code == ProtocolErrorCode::TerminalUnavailable
                        && error.retryable
                        && (error.message.contains("original terminal holder")
                            || error.message.contains("Terminal worker retirement")) =>
                {
                    assert!(Instant::now() < deadline, "close stayed pending: {error:?}");
                    thread::sleep(Duration::from_millis(5));
                }
                Err(error) => panic!("unexpected close refusal: {error:?}"),
            }
        }
    }

    fn retention_client_host(
        name: &str,
    ) -> (
        SharedHost,
        tempfile::TempDir,
        crate::model::TerminalSession,
        crate::terminal::TerminalHandle,
    ) {
        let (_, host) = panic_test_host(name);
        let directory = tempfile::Builder::new().prefix("rp-").tempdir().unwrap();
        let mut recipe = crate::model::LaunchRecipe::shell(directory.path().into()).unwrap();
        recipe.program = "/bin/true".into();
        recipe.arguments.clear();
        let session = crate::model::TerminalSession {
            id: TerminalId::new(),
            launch: recipe.clone(),
            transcript_path: None,
            resume_hint: None,
            owned_codex_listener: None,
            recovery_origin: None,
            predecessor: None,
            continuity: crate::model::TerminalContinuity::RelaunchAvailable {
                recipe,
                last_exit: None,
            },
        };
        let terminal =
            crate::terminal::TerminalHandle::spawn(session.clone(), Default::default(), |_| {})
                .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !terminal.is_completed() {
            assert!(Instant::now() < deadline, "real PTY did not complete");
            thread::sleep(Duration::from_millis(5));
        }
        terminal.shutdown().unwrap();
        let proof = host.lock().unwrap().proof_handle();
        wait_client_retirement(|| {
            matches!(
                terminal.completed_retirement(&session, &proof),
                crate::terminal::TerminalRetirement::Released
            )
        });
        {
            let mut state = host.lock().unwrap();
            state.install_model(
                super::super::model::Registry::at(&directory.path().join("model.sqlite3")).unwrap(),
            );
            state.register_client(1);
            state.reserve_spawn(session.id).unwrap();
            state
                .finish_spawn(1, session.clone(), terminal.clone(), false)
                .unwrap();
        }
        (host, directory, session, terminal)
    }

    /// Hold the real handler after final retirement but before serve_with can
    /// enqueue its response. Ordinary exit must not seal that delivery gap.
    #[test]
    fn destructive_terminal_replies_drain_before_ordinary_exit_seals_delivery() {
        use std::sync::mpsc;
        const CASE: &str = "CAGENTS_EXIT_DRAIN_TEST_CASE";
        let name = thread::current().name().unwrap().to_owned();
        let cases = match std::env::var(CASE) {
            Ok(case) => vec![case],
            Err(_) => ["checked", "release", "shutdown", "chat"]
                .into_iter()
                .map(str::to_owned)
                .collect(),
        };
        fn chat_close_case() {
            use crate::chat::protocol::{Adapter, ProcessSpec, Step};
            struct Quiet;
            impl Adapter for Quiet {
                fn initialize(&mut self) -> Result<Step, String> {
                    Ok(Step::default())
                }
                fn command(&mut self, _: crate::chat::Command) -> Result<Step, String> {
                    Ok(Step::default())
                }
                fn receive(&mut self, _: serde_json::Value) -> Result<Step, String> {
                    Ok(Step::default())
                }
            }
            thread_local! {
                static PAUSE: std::cell::RefCell<Option<(mpsc::Sender<()>, mpsc::Receiver<()>)>> =
                    const { std::cell::RefCell::new(None) };
            }
            fn close_then_pause(
                host: &SharedHost,
                client: ClientId,
                request: ClientRequest,
            ) -> Result<RequestOutcome, ProtocolError> {
                let closing = matches!(&request, ClientRequest::ChatClose { .. });
                if matches!(&request, ClientRequest::ChatCloseRetained { .. }) {
                    assert!(
                        host.lock().unwrap().ordinary_exit_cleanup_blocked(),
                        "retained Close must own reply custody even on refusal"
                    );
                }
                let result = process_request(host, client, request);
                if closing {
                    assert!(result.is_ok(), "real native chat close failed: {result:?}");
                    assert!(host.lock().unwrap().chats.is_empty());
                    PAUSE.with(|pause| {
                        let (closed, resume) = pause.borrow_mut().take().unwrap();
                        closed.send(()).unwrap();
                        resume.recv_timeout(Duration::from_secs(5)).unwrap();
                    });
                }
                result
            }
            fn send(
                stream: &mut UnixStream,
                proof: &HostSessionProof,
                id: u64,
                request: ClientRequest,
            ) {
                FrameCodec::new(MAX_CLIENT_FRAME_BYTES)
                    .write(
                        stream,
                        &ClientEnvelope {
                            version: PROTOCOL_VERSION,
                            request_id: id,
                            proof: Some(proof.clone()),
                            request,
                        },
                    )
                    .unwrap();
            }
            fn reply(stream: &mut UnixStream, id: u64) -> ServerMessage {
                let mut codec = FrameCodec::new(MAX_FRAME_BYTES);
                loop {
                    let envelope = codec
                        .read::<ServerEnvelope>(stream)
                        .unwrap()
                        .expect("native close reply lost before delivery drained");
                    if envelope.request_id == Some(id) {
                        return envelope.message;
                    }
                }
            }
            let (proof, host) = panic_test_host("chat-reply-custody");
            let directory = tempfile::Builder::new().prefix("cq-").tempdir().unwrap();
            let mut recipe = crate::model::LaunchRecipe::shell(directory.path().into()).unwrap();
            // The adapter is in-process; its harmless JSON-lines transport
            // is a real local cat, never an installed model/provider.
            recipe.program = "/bin/cat".into();
            recipe.arguments.clear();
            let spec = ProcessSpec::from(&recipe);
            let mut initial = Step::default();
            let store = crate::chat::runtime::preflight(&spec, &mut initial).unwrap();
            let actor = crate::chat::runtime::Handle::start(
                spec,
                Box::new(Quiet),
                initial,
                store,
                None,
                |_| {},
            )
            .unwrap();
            assert!(!actor.is_completed());
            let source = crate::chat::Session {
                id: TerminalId::new(),
                proof: Some(proof.clone()),
                terminal: None,
                native_continuity: Default::default(),
                owned_codex_listener: None,
                launch: crate::chat::Launch {
                    provider: crate::agent::Provider::Omp,
                    recipe,
                    session_id: None,
                },
            };
            {
                let mut state = host.lock().unwrap();
                state.install_model(
                    super::super::model::Registry::at(&directory.path().join("model.sqlite3"))
                        .unwrap(),
                );
                state.chats.insert(
                    source.id,
                    super::super::chat::Record {
                        execution_instance: uuid::Uuid::new_v4(),
                        console_recipe: None,
                        paired_terminal: None,
                        session: source.clone(),
                        handle: actor.clone(),
                        source_process: None,
                        subscribers: std::collections::BTreeMap::from([(1, false)]),
                        observations: Default::default(),
                        completion_observed: false,
                        returning_terminal: false,
                    },
                );
            }
            let (mut stream, server_stream) = UnixStream::pair().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let (closed_tx, closed_rx) = mpsc::channel();
            let (resume_tx, resume_rx) = mpsc::channel();
            let serving_host = Arc::clone(&host);
            let serving = thread::spawn(move || {
                PAUSE.with(|pause| *pause.borrow_mut() = Some((closed_tx, resume_rx)));
                serve_with(server_stream, 1, serving_host, close_then_pause);
            });
            send(
                &mut stream,
                &proof,
                1,
                ClientRequest::Connect {
                    expected_host: Some(proof.clone()),
                    resume: Vec::new(),
                },
            );
            assert!(matches!(
                reply(&mut stream, 1),
                ServerMessage::Response {
                    response: ServerResponse::Connected { .. }
                }
            ));
            // This actor has no detached authenticated holder. Real retained
            // cleanup must refuse it; its error reply must release the guard.
            // Successful retained-native teardown is not claimed here.
            send(
                &mut stream,
                &proof,
                2,
                ClientRequest::ChatCloseRetained {
                    session: source.clone(),
                },
            );
            assert!(matches!(reply(&mut stream, 2), ServerMessage::Error(error)
                if error.code == ProtocolErrorCode::TerminalUnavailable && error.retryable));
            let deadline = Instant::now() + Duration::from_secs(5);
            while host.lock().unwrap().ordinary_exit_cleanup_blocked() {
                assert!(
                    Instant::now() < deadline,
                    "refused retained close leaked reply custody"
                );
                thread::sleep(Duration::from_millis(5));
            }
            assert!(!actor.is_completed());
            assert!(
                host.lock().unwrap().chats[&source.id]
                    .handle
                    .same_attachment(&actor)
            );
            assert!(host.lock().unwrap().chat_spawning.is_empty());
            assert!(host.lock().unwrap().native_closing.is_empty());
            send(
                &mut stream,
                &proof,
                3,
                ClientRequest::ChatClose { chat_id: source.id },
            );
            closed_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            assert!(actor.is_completed());
            assert!(host.lock().unwrap().chats.is_empty());
            let drain = super::super::begin_ordinary_exit(&host).unwrap();
            let deadline = Instant::now() + Duration::from_secs(5);
            while !drain.try_seal_absence() {
                assert!(
                    Instant::now() < deadline,
                    "chat child custody did not settle"
                );
                thread::sleep(Duration::from_millis(5));
            }
            let mut cleanup_error = None;
            assert!(host.lock().unwrap().ordinary_exit_cleanup_blocked());
            assert!(!super::super::finish_shutdown_owners(
                &host,
                &mut cleanup_error,
                false
            ));
            resume_tx.send(()).unwrap();
            let deadline = Instant::now() + Duration::from_secs(5);
            while host.lock().unwrap().ordinary_exit_cleanup_blocked() {
                assert!(
                    Instant::now() < deadline,
                    "native chat reply guard did not release"
                );
                thread::sleep(Duration::from_millis(5));
            }
            assert!(super::super::finish_shutdown_owners(
                &host,
                &mut cleanup_error,
                false
            ));
            assert!(cleanup_error.is_none());
            let deliveries = {
                let mut state = host.lock().unwrap();
                assert!(drain.try_seal_absence());
                assert!(!state.ordinary_exit_cleanup_blocked());
                let deliveries = state.close_ordinary_exit_registration();
                assert_eq!(
                    state.begin_checked_close_reply(99).unwrap_err().code,
                    ProtocolErrorCode::TerminalUnavailable
                );
                deliveries
            };
            super::super::close_shutdown_deliveries(&host, &deliveries);
            assert!(matches!(reply(&mut stream, 3), ServerMessage::Response {
                response: ServerResponse::ChatClosed { chat_id, .. }
            } if chat_id == source.id));
            let mut codec = FrameCodec::new(MAX_FRAME_BYTES);
            while let Some(envelope) = codec.read::<ServerEnvelope>(&mut stream).unwrap() {
                assert_eq!(envelope.request_id, None);
                assert!(matches!(envelope.message, ServerMessage::HostShuttingDown));
            }
            serving.join().unwrap();
            assert!(host.lock().unwrap().clients.is_empty());
            assert!(!host.lock().unwrap().ordinary_exit_cleanup_blocked());
        }
        for case in cases {
            let Some(_root) =
                super::super::exit_drain_tests::private_runtime_case(&name, Some(&case))
            else {
                continue;
            };
            if case == "chat" {
                chat_close_case();
                continue;
            }
            thread_local! {
                static PAUSE: std::cell::RefCell<Option<(mpsc::Sender<()>, mpsc::Receiver<()>)>> =
                    const { std::cell::RefCell::new(None) };
            }
            fn retire_then_pause(
                host: &SharedHost,
                client: ClientId,
                request: ClientRequest,
            ) -> Result<RequestOutcome, ProtocolError> {
                let closing = matches!(
                    &request,
                    ClientRequest::CloseTerminalChecked { .. }
                        | ClientRequest::ReleaseCompletedTerminal { .. }
                        | ClientRequest::ShutdownTerminal { .. }
                );
                let result = process_request(host, client, request);
                if closing {
                    assert!(
                        result.is_ok(),
                        "real terminal retirement failed: {result:?}"
                    );
                    let state = host.lock().unwrap();
                    assert!(state.terminals.is_empty());
                    assert!(state.terminal_retirements.is_empty());
                    drop(state);
                    PAUSE.with(|pause| {
                        let (retired, resume) = pause.borrow_mut().take().unwrap();
                        retired.send(()).unwrap();
                        resume.recv_timeout(Duration::from_secs(5)).unwrap();
                    });
                }
                result
            }
            fn send(
                stream: &mut UnixStream,
                proof: &HostSessionProof,
                request_id: u64,
                request: ClientRequest,
            ) {
                FrameCodec::new(MAX_CLIENT_FRAME_BYTES)
                    .write(
                        stream,
                        &ClientEnvelope {
                            version: PROTOCOL_VERSION,
                            request_id,
                            proof: Some(proof.clone()),
                            request,
                        },
                    )
                    .unwrap();
            }
            fn reply(stream: &mut UnixStream, request_id: u64) -> ServerMessage {
                let mut codec = FrameCodec::new(MAX_FRAME_BYTES);
                loop {
                    let envelope = codec
                        .read::<ServerEnvelope>(stream)
                        .unwrap()
                        .expect("correlated reply was lost before delivery drained");
                    if envelope.request_id == Some(request_id) {
                        return envelope.message;
                    }
                }
            }

            let (host, _directory, mut session, _terminal) = retention_client_host(&case);
            let proof = current_proof(&host);
            assert!(!host.lock().unwrap().ordinary_exit_cleanup_blocked());
            let (mut stream, server_stream) = UnixStream::pair().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let (retired_tx, retired_rx) = mpsc::channel();
            let (resume_tx, resume_rx) = mpsc::channel();
            let serving_host = Arc::clone(&host);
            let serving = thread::spawn(move || {
                PAUSE.with(|pause| *pause.borrow_mut() = Some((retired_tx, resume_rx)));
                serve_with(server_stream, 1, serving_host, retire_then_pause);
            });
            send(
                &mut stream,
                &proof,
                1,
                ClientRequest::Connect {
                    expected_host: Some(proof.clone()),
                    resume: Vec::new(),
                },
            );
            assert!(matches!(
                reply(&mut stream, 1),
                ServerMessage::Response {
                    response: ServerResponse::Connected { .. }
                }
            ));
            let id = session.id;
            let request = match case.as_str() {
                "checked" => ClientRequest::CloseTerminalChecked {
                    terminal_id: id,
                    allow_active: false,
                },
                "release" => {
                    session.continuity = crate::model::TerminalContinuity::Exited {
                        last_exit: crate::model::TerminalExit {
                            code: Some(0),
                            signal: None,
                            occurred_at_unix_ms: 1,
                        },
                    };
                    ClientRequest::ReleaseCompletedTerminal {
                        session,
                        allow_active: false,
                    }
                }
                "shutdown" => ClientRequest::ShutdownTerminal { terminal_id: id },
                _ => panic!("unknown destructive request fixture"),
            };
            send(&mut stream, &proof, 2, request);
            retired_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            assert_eq!(retention_pin_generation(&host, id), Ok(None));
            let drain = super::super::begin_ordinary_exit(&host).unwrap();
            wait_client_retirement(|| drain.try_seal_absence());
            // Actual original-child absence is already sealed: only the
            // outstanding correlated reply can block final Host registration.
            assert!(host.lock().unwrap().ordinary_exit_cleanup_blocked());
            resume_tx.send(()).unwrap();
            wait_client_retirement(|| !host.lock().unwrap().ordinary_exit_cleanup_blocked());
            let deliveries = {
                let mut state = host.lock().unwrap();
                assert!(drain.try_seal_absence());
                assert!(!state.ordinary_exit_cleanup_blocked());
                let deliveries = state.close_ordinary_exit_registration();
                assert_eq!(
                    state.begin_checked_close_reply(99).unwrap_err().code,
                    ProtocolErrorCode::TerminalUnavailable,
                );
                deliveries
            };
            // Close the real writer BEFORE reading the queued success: the
            // frame must survive close_after_drain, not merely a live socket.
            super::super::close_shutdown_deliveries(&host, &deliveries);
            let response = reply(&mut stream, 2);
            if case == "shutdown" {
                assert!(matches!(
                    response,
                    ServerMessage::Response {
                        response: ServerResponse::Accepted
                    }
                ));
            } else {
                assert!(matches!(response, ServerMessage::Response {
                    response: ServerResponse::TerminalCloseChecked {
                        terminal_id,
                        outcome: crate::terminal_ipc::TerminalCloseResult::Closed,
                    }
                } if terminal_id == id));
            }
            let mut codec = FrameCodec::new(MAX_FRAME_BYTES);
            while let Some(envelope) = codec.read::<ServerEnvelope>(&mut stream).unwrap() {
                assert_eq!(envelope.request_id, None);
                assert!(matches!(envelope.message, ServerMessage::HostShuttingDown));
            }
            serving.join().unwrap();
            assert!(host.lock().unwrap().clients.is_empty());
            assert!(!host.lock().unwrap().ordinary_exit_cleanup_blocked());
        }
    }

    fn terminal_presence(id: TerminalId) -> crate::notifications::Presence {
        crate::notifications::Presence {
            application: crate::notifications::Application::Cagents,
            window: "retention-client".into(),
            focused_target: None,
            sessions: vec![id],
            focused: None,
        }
    }

    #[test]
    fn notification_presence_reservation_fences_durable_pins_and_shutdown() {
        let (host, directory, session, handle) = retention_client_host("presence-retention");
        let id = session.id;
        // A saved view claim must obey the same current-worker reservation,
        // including after workspace SQL moves outside the HostState mutex.
        use crate::workspace_model::views::{Change, Pane, Scope};
        let channel = crate::model::ChannelId::new();
        for edit in [
            ModelEdit::OpenProject {
                repository: directory.path().into(),
                name: "retention-view".into(),
                kind: Default::default(),
            },
            ModelEdit::CreateChannel {
                id: channel,
                repository: directory.path().into(),
                title: "retention-view".into(),
                title_source: Default::default(),
            },
        ] {
            process_request(&host, 1, ClientRequest::EditWorkspaceModel { edit }).unwrap();
        }
        let pane = Pane {
            id: crate::model::CardId::new(),
            title: "retained terminal".into(),
            user_title: None,
            pending_terminal_start: false,
            width_fraction: 0.5,
            scope: None,
            kind: crate::model::CardKind::Terminal(session.clone()),
        };
        process_request(
            &host,
            1,
            ClientRequest::EditWorkspaceModel {
                edit: ModelEdit::SeedView {
                    scope: Scope::Channel(channel),
                    panes: vec![],
                    closed: vec![],
                },
            },
        )
        .unwrap();
        let before = host.lock().unwrap().read_model().unwrap();
        let revision = before
            .views
            .iter()
            .find(|view| view.scope == Scope::Channel(channel))
            .unwrap()
            .revision;
        {
            let mut state = host.lock().unwrap();
            state
                .terminal_retirements
                .insert(id, super::super::state::Retirement::submitting(handle));
        }
        let mut replacement = pane.clone();
        replacement.title = "new title".into();
        for edit in [
            ModelEdit::SeedView {
                scope: Scope::Channel(channel),
                panes: vec![pane.clone()],
                closed: vec![],
            },
            ModelEdit::ChangeView {
                scope: Scope::Channel(channel),
                expected_revision: revision,
                operation: uuid::Uuid::new_v4(),
                change: Change::Insert {
                    pane: pane.clone(),
                    before: None,
                },
            },
            ModelEdit::ChangeView {
                scope: Scope::Channel(channel),
                expected_revision: revision,
                operation: uuid::Uuid::new_v4(),
                change: Change::Update {
                    before: pane,
                    pane: replacement,
                },
            },
        ] {
            let error =
                process_request(&host, 1, ClientRequest::EditWorkspaceModel { edit }).unwrap_err();
            assert_eq!(error.code, ProtocolErrorCode::TerminalUnavailable);
            assert!(error.retryable && error.message.contains("completing retirement"));
            assert_eq!(host.lock().unwrap().read_model().unwrap(), before);
        }
        let registry = super::super::agent_execution::registry(&host).unwrap();
        for watch in [false, true] {
            let request = if watch {
                ClientRequest::WatchNotifications {
                    presence: terminal_presence(id),
                }
            } else {
                ClientRequest::NotificationPresence {
                    presence: terminal_presence(id),
                }
            };
            let error = process_request(&host, 1, request).unwrap_err();
            assert_eq!(error.code, ProtocolErrorCode::TerminalUnavailable);
            assert!(error.retryable);
            assert!(
                registry
                    .lock()
                    .unwrap()
                    .retention_pin_generation(id)
                    .unwrap()
                    .is_none()
            );
        }
        for request in [
            ClientRequest::Shutdown,
            ClientRequest::ShutdownV1 {
                version: SHUTDOWN_VERSION,
                operation: ShutdownOperation::Preflight,
            },
            ClientRequest::UpgradeImpact {
                policy: crate::update::impact::Policy::Interrupt,
            },
        ] {
            let error = process_request(&host, 1, request).unwrap_err();
            assert!(error.retryable && error.message.contains("pending"));
        }
        host.lock()
            .unwrap()
            .terminal_retirements
            .get_mut(&id)
            .unwrap()
            .failure = Some("retained exact owner".into());
        let error = process_request(&host, 1, ClientRequest::Shutdown).unwrap_err();
        assert!(error.retryable && error.message.contains("unconfirmed"));
        assert!(!host.lock().unwrap().shutting_down);
        // Failed cleanup remains conservatively claimable for presentation;
        // only the in-flight cleanup reservation refuses a new saved claim.
        process_request(
            &host,
            1,
            ClientRequest::NotificationPresence {
                presence: terminal_presence(id),
            },
        )
        .unwrap();
        assert!(
            registry
                .lock()
                .unwrap()
                .retention_pin_generation(id)
                .unwrap()
                .is_some()
        );
        let mut editor = terminal_presence(id);
        editor.application = crate::notifications::Application::Ceditor;
        assert!(
            process_request(
                &host,
                1,
                ClientRequest::NotificationPresence { presence: editor }
            )
            .is_err()
        );
        process_request(
            &host,
            1,
            ClientRequest::ShutdownTerminal { terminal_id: id },
        )
        .unwrap();
        let state = host.lock().unwrap();
        assert!(!state.terminals.contains_key(&id));
        assert!(!state.terminal_retirements.contains_key(&id));
        drop(state);
        assert!(
            registry
                .lock()
                .unwrap()
                .retention_pin_generation(id)
                .unwrap()
                .is_none()
        );
        let (_, missing_registry) = panic_test_host("presence-without-registry");
        missing_registry.lock().unwrap().register_client(1);
        assert!(
            process_request(
                &missing_registry,
                1,
                ClientRequest::NotificationPresence {
                    presence: terminal_presence(TerminalId::new()),
                }
            )
            .is_ok(),
            "unavailable retention storage must not break the conservative old notification path"
        );
        assert!(
            super::super::model::Registry::at(&directory.path().join("model.sqlite3"))
                .unwrap()
                .retention_pin_generation(id)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn explicit_completed_close_cannot_forget_a_republished_pin_or_fake_missing_cleanup() {
        let (host, _directory, mut session, handle) = retention_client_host("close-republication");
        let id = session.id;
        process_request(
            &host,
            1,
            ClientRequest::NotificationPresence {
                presence: terminal_presence(id),
            },
        )
        .unwrap();
        let closing_generation = retention_pin_generation(&host, id);
        assert!(closing_generation.as_ref().unwrap().is_some());
        handle.shutdown().unwrap();
        // An already-admitted presence can publish after its initial Host
        // check. Final compaction must drain this real reader before deciding.
        let gate = control_gate(&host);
        let publication = gate.read().unwrap();
        host.lock()
            .unwrap()
            .ensure_retention_available(&[id])
            .unwrap();
        let worker_host = host.clone();
        let worker_handle = handle.clone();
        let worker_source = host.lock().unwrap().terminals[&id].session.clone();
        let worker_generation = closing_generation.clone();
        let (started, start) = crossbeam_channel::bounded(1);
        let (finished, finish) = crossbeam_channel::bounded(1);
        let worker = thread::spawn(move || {
            started.send(()).unwrap();
            assert!(
                finished
                    .send(reserve_terminal_close(
                        &worker_host,
                        id,
                        &worker_handle,
                        &worker_source,
                        None,
                        worker_generation
                    ))
                    .is_ok()
            );
        });
        start.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(
            matches!(
                finish.recv_timeout(Duration::from_millis(100)),
                Err(crossbeam_channel::RecvTimeoutError::Timeout)
            ),
            "compaction bypassed admitted ownership publication"
        );
        super::super::agent_execution::registry(&host)
            .unwrap()
            .lock()
            .unwrap()
            .pin_window_terminals(&[id])
            .unwrap();
        let replacement_generation = retention_pin_generation(&host, id);
        assert_ne!(closing_generation, replacement_generation);
        drop(publication);
        assert!(
            finish
                .recv_timeout(Duration::from_secs(5))
                .unwrap()
                .is_err()
        );
        worker.join().unwrap();
        assert!(host.lock().unwrap().terminals.contains_key(&id));
        assert_eq!(retention_pin_generation(&host, id), replacement_generation);
        // Retry captures the newly published generation only after the old
        // attempt truthfully retained both record and newer saved membership.
        let latest = retention_pin_generation(&host, id);
        finish_test_terminal_close(&host, id, &handle, latest).unwrap();
        assert!(!host.lock().unwrap().terminals.contains_key(&id));
        session.continuity = crate::model::TerminalContinuity::Exited {
            last_exit: crate::model::TerminalExit {
                code: Some(0),
                signal: None,
                occurred_at_unix_ms: 1,
            },
        };
        let error = process_request(
            &host,
            1,
            ClientRequest::ReleaseCompletedTerminal {
                session,
                allow_active: true,
            },
        )
        .unwrap_err();
        assert_eq!(error.code, ProtocolErrorCode::TerminalUnavailable);
        assert!(error.retryable);
        assert_eq!(retention_pin_generation(&host, id), Ok(None));
        let (ended_host, _ended_directory, ended, _) =
            retention_client_host("checked-ended-history");
        assert!(
            matches!(
                process_request(
                    &ended_host,
                    1,
                    ClientRequest::CloseTerminalChecked {
                        terminal_id: ended.id,
                        allow_active: false,
                    }
                )
                .unwrap()
                .0,
                ServerResponse::TerminalCloseChecked {
                    outcome: crate::terminal_ipc::TerminalCloseResult::Closed,
                    ..
                }
            ),
            "completed history still has a real close/join path"
        );
        assert!(!ended_host.lock().unwrap().terminals.contains_key(&ended.id));
        let (unknown_host, unknown_directory, unknown, _) =
            retention_client_host("unknown-pin-storage");
        let writer =
            rusqlite::Connection::open(unknown_directory.path().join("model.sqlite3")).unwrap();
        writer
            .execute_batch("PRAGMA user_version=2147483647")
            .unwrap();
        let error = process_request(
            &unknown_host,
            1,
            ClientRequest::CloseTerminalChecked {
                terminal_id: unknown.id,
                allow_active: false,
            },
        )
        .unwrap_err();
        assert_eq!(error.code, ProtocolErrorCode::TerminalUnavailable);
        assert!(error.retryable && error.message.contains("ownership was unknown"));
        assert!(
            unknown_host
                .lock()
                .unwrap()
                .terminals
                .contains_key(&unknown.id),
            "unknown pin storage must not be treated as a known absent local pin"
        );
    }

    #[test]
    fn completed_close_preserves_a_replacement_worker_and_exact_native_pair() {
        use crate::chat::protocol::{Adapter, ProcessSpec, Step};
        struct Quiet;
        impl Adapter for Quiet {
            fn initialize(&mut self) -> Result<Step, String> {
                Ok(Step::default())
            }
            fn command(&mut self, _: crate::chat::Command) -> Result<Step, String> {
                Ok(Step::default())
            }
            fn receive(&mut self, _: serde_json::Value) -> Result<Step, String> {
                Ok(Step::default())
            }
        }
        let (host, _directory, session, old) = retention_client_host("close-worker-replacement");
        let id = session.id;
        process_request(
            &host,
            1,
            ClientRequest::NotificationPresence {
                presence: terminal_presence(id),
            },
        )
        .unwrap();
        let old_pin = retention_pin_generation(&host, id);
        let removed = host.lock().unwrap().retire_terminal(id).unwrap();
        drop(removed);
        let mut current = session.clone();
        current.launch.program = "/bin/cat".into();
        let replacement =
            crate::terminal::TerminalHandle::spawn(current.clone(), Default::default(), |_| {})
                .unwrap();
        {
            let mut state = host.lock().unwrap();
            state.reserve_spawn(id).unwrap();
            state
                .finish_spawn(1, current.clone(), replacement.clone(), false)
                .unwrap();
        }
        assert!(terminal_close_pair(&host, id, &old).is_err());
        assert!(finish_test_terminal_close(&host, id, &old, old_pin.clone()).is_err());
        assert!(
            host.lock().unwrap().terminals[&id]
                .handle
                .same_worker(&replacement)
        );
        assert!(!replacement.is_completed());
        assert_eq!(retention_pin_generation(&host, id), old_pin);

        let mut initial = Step::default();
        let spec = ProcessSpec::from(&current.launch);
        let store = crate::chat::runtime::preflight(&spec, &mut initial).unwrap();
        let actor = crate::chat::runtime::Handle::start(
            spec,
            Box::new(Quiet),
            initial,
            store,
            Some("pair-one".into()),
            |_| {},
        )
        .unwrap();
        {
            let mut state = host.lock().unwrap();
            let proof = state.proof_handle().as_ref().clone();
            state.chats.insert(
                id,
                super::super::chat::Record {
                    execution_instance: uuid::Uuid::new_v4(),
                    session: crate::chat::Session {
                        id,
                        proof: Some(proof),
                        terminal: Some(current.clone()),
                        launch: crate::chat::Launch {
                            provider: crate::agent::Provider::Codex,
                            recipe: current.launch.clone(),
                            session_id: Some("pair-one".into()),
                        },
                        native_continuity: Default::default(),
                        owned_codex_listener: None,
                    },
                    console_recipe: None,
                    paired_terminal: Some(replacement.clone()),
                    handle: actor.clone(),
                    source_process: None,
                    subscribers: Default::default(),
                    observations: Default::default(),
                    completion_observed: false,
                    returning_terminal: false,
                },
            );
        }
        let captured = terminal_close_pair(&host, id, &replacement)
            .unwrap()
            .unwrap();
        let spec = ProcessSpec::from(&current.launch);
        let mut initial = Step::default();
        let store = crate::chat::runtime::preflight(&spec, &mut initial).unwrap();
        let next_actor = crate::chat::runtime::Handle::start(
            spec,
            Box::new(Quiet),
            initial,
            store,
            Some("pair-two".into()),
            |_| {},
        )
        .unwrap();
        {
            let mut state = host.lock().unwrap();
            let record = state.chats.get_mut(&id).unwrap();
            record.handle = next_actor.clone();
            record.execution_instance = uuid::Uuid::new_v4();
        }
        super::super::chat::close_terminal_actor(&host, id, &captured).unwrap();
        assert!(
            !next_actor.is_completed(),
            "old receipt must not close a replacement actor"
        );
        assert!(finish_test_terminal_close(&host, id, &replacement, old_pin.clone()).is_err());
        assert!(
            host.lock().unwrap().terminals[&id]
                .handle
                .same_worker(&replacement)
        );
        assert_eq!(retention_pin_generation(&host, id), old_pin);
        let removed = host.lock().unwrap().chats.remove(&id).unwrap();
        drop(removed);
        actor.close().unwrap();
        next_actor.close().unwrap();
        assert!(matches!(
            retry_retirement_close(&host, || ClientRequest::CloseTerminalChecked {
                terminal_id: id,
                allow_active: true,
            })
            .0,
            ServerResponse::TerminalCloseChecked {
                outcome: crate::terminal_ipc::TerminalCloseResult::Closed,
                ..
            }
        ));
        assert!(!host.lock().unwrap().terminals.contains_key(&id));
        assert!(retention_pin_generation(&host, id).unwrap().is_none());
    }

    #[test]
    fn explicit_close_retains_registered_original_until_actual_holder_receipt() {
        use std::os::unix::{
            ffi::{OsStrExt, OsStringExt},
            fs::PermissionsExt,
        };
        const CHILD: &str = "CAGENTS_EXPLICIT_RECEIPT_CHILD";
        const AUTH_PRESENT: &str = "CAGENTS_EXPLICIT_RECEIPT_AUTH_PRESENT";
        let name = thread::current().name().unwrap().to_owned();
        if std::env::var(CHILD).as_deref() != Ok(name.as_str()) {
            let private = tempfile::Builder::new().prefix("cr-").tempdir().unwrap();
            std::fs::set_permissions(private.path(), std::fs::Permissions::from_mode(0o700))
                .unwrap();
            let stdout = private.path().join("stdout.log");
            let stderr = private.path().join("stderr.log");
            let mut child = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", &name, "--nocapture", "--test-threads=1"])
                .env(CHILD, &name)
                .env("TMPDIR", private.path())
                .env("XDG_RUNTIME_DIR", private.path())
                .env(
                    AUTH_PRESENT,
                    std::env::var_os(crate::agent_execution::AUTHORITY_ENV)
                        .is_some()
                        .to_string(),
                )
                .stdout(std::fs::File::create(&stdout).unwrap())
                .stderr(std::fs::File::create(&stderr).unwrap())
                .spawn()
                .unwrap();
            let deadline = Instant::now() + Duration::from_secs(40);
            let status = loop {
                if let Some(status) = child.try_wait().unwrap() {
                    break status;
                }
                if Instant::now() >= deadline {
                    if let Ok(bytes) = std::fs::read(private.path().join("release-obstruction")) {
                        let fault = std::path::PathBuf::from(std::ffi::OsString::from_vec(bytes));
                        if fault.starts_with(private.path()) {
                            let _ = std::fs::remove_dir(fault);
                        }
                    }
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!(
                        "explicit receipt child timed out; evidence {}",
                        private.keep().display()
                    );
                }
                thread::sleep(Duration::from_millis(5));
            };
            let raw_out = std::fs::read_to_string(stdout).unwrap();
            let raw_err = std::fs::read_to_string(stderr).unwrap();
            print!("{raw_out}");
            eprint!("{raw_err}");
            assert!(
                status.success(),
                "receipt child {status}; evidence {}",
                private.keep().display()
            );
            assert!(raw_out.contains(&name) && raw_out.contains("1 passed; 0 failed"));
            return;
        }
        assert_eq!(
            std::env::var(AUTH_PRESENT).unwrap(),
            std::env::var_os(crate::agent_execution::AUTHORITY_ENV)
                .is_some()
                .to_string()
        );
        let proof = HostSessionProof {
            host_session_id: crate::model::HostSessionId::new(uuid::Uuid::new_v4().to_string()),
            boot_id: crate::daemon::read_boot_id().unwrap(),
            host_incarnation: crate::model::HostIncarnation::new(uuid::Uuid::new_v4().to_string()),
        };
        let paths = crate::daemon::RuntimePaths::discover().unwrap();
        let _lease = crate::daemon::HostLease::acquire(&paths, &proof).unwrap();
        let directory = tempfile::Builder::new().prefix("rp-").tempdir().unwrap();
        let mut recipe = crate::model::LaunchRecipe::shell(directory.path().into()).unwrap();
        recipe.program = "/bin/sh".into();
        recipe.arguments = vec![
            "-c".into(),
            "printf 'exact-registered-close-result'; exit 0".into(),
        ];
        let source = crate::model::TerminalSession {
            id: TerminalId::new(),
            launch: recipe,
            transcript_path: None,
            resume_hint: None,
            owned_codex_listener: None,
            recovery_origin: None,
            predecessor: None,
            continuity: crate::model::TerminalContinuity::DetachedSameBoot {
                host_session_id: proof.host_session_id.clone(),
                boot_id: proof.boot_id.clone(),
                host_incarnation: proof.host_incarnation.clone(),
            },
        };
        let recovery = crate::terminal::holder::Recovery {
            source: source.clone(),
            previous_sources: vec![],
            relaunches: vec![],
            options: Default::default(),
            execution_environment: None,
            execution_recipe: None,
        };
        let handle = crate::terminal::TerminalHandle::spawn(
            source.clone(),
            crate::terminal::TerminalConfig {
                holder_recovery: Some(recovery),
                ..Default::default()
            },
            |_| {},
        )
        .unwrap();
        wait_client_retirement(|| handle.is_completed() && handle.worker_joined_for_test());
        let original = handle.holder_identity().unwrap().clone();
        assert!(original.holder.is_current());
        assert!(!original.root.is_current());
        let host = Arc::new(super::super::state::HostLock::new(
            super::super::state::HostState::new(proof.clone()),
        ));
        {
            let mut state = host.lock().unwrap();
            state.install_model(
                super::super::model::Registry::at(&directory.path().join("model.sqlite3")).unwrap(),
            );
            state.register_client(1);
            state.reserve_spawn(source.id).unwrap();
            state
                .finish_spawn(1, source.clone(), handle.clone(), false)
                .unwrap();
        }
        process_request(
            &host,
            1,
            ClientRequest::NotificationPresence {
                presence: terminal_presence(source.id),
            },
        )
        .unwrap();
        let pin = retention_pin_generation(&host, source.id);
        assert!(pin.as_ref().unwrap().is_some());
        let receipt = paths.directory.join("holders").join(format!(
            "{}-{}-{}.closed",
            original.terminal, original.holder.pid, original.holder.start
        ));
        std::fs::create_dir(&receipt).unwrap();
        struct Obstruction(std::path::PathBuf);
        impl Drop for Obstruction {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir(&self.0);
            }
        }
        let obstruction = Obstruction(receipt.clone());
        let marker = std::env::temp_dir().join("release-obstruction");
        std::fs::write(&marker, receipt.as_os_str().as_bytes()).unwrap();
        let error = process_request(
            &host,
            1,
            ClientRequest::ShutdownTerminal {
                terminal_id: source.id,
            },
        )
        .unwrap_err();
        assert_eq!(error.code, ProtocolErrorCode::TerminalUnavailable);
        assert!(error.retryable);
        assert!(handle.worker_joined_for_test());
        assert!(
            original.holder.is_current(),
            "join must not be mistaken for original holder exit"
        );
        assert!(matches!(
            handle.completed_retirement(&source, &proof),
            crate::terminal::TerminalRetirement::WaitingForHolder(_)
        ));
        assert!(
            host.lock().unwrap().terminals[&source.id]
                .handle
                .same_worker(&handle)
        );
        assert_eq!(retention_pin_generation(&host, source.id), pin);
        // Force the unlocked-probe admission interval and its interrupted
        // request path without starting any provider or installing a test hook.
        let current_source = host.lock().unwrap().terminals[&source.id].session.clone();
        let interrupted = reserve_terminal_close(
            &host,
            source.id,
            &handle,
            &current_source,
            None,
            pin.clone(),
        )
        .unwrap();
        let mut refused_recipe = source.launch.clone();
        refused_recipe.program = "/missing-retirement-claim-never-started".into();
        let candidate = crate::chat::Session {
            id: TerminalId::new(),
            terminal: Some(source.clone()),
            proof: Some(proof.clone()),
            launch: crate::chat::Launch {
                provider: crate::agent::Provider::Claude,
                recipe: refused_recipe,
                session_id: None,
            },
            native_continuity: Default::default(),
            owned_codex_listener: None,
        };
        let error = process_request(
            &host,
            1,
            ClientRequest::ChatStart {
                session: candidate.clone(),
                source: None,
                terminate_source: false,
                execution_environment: Default::default(),
            },
        )
        .unwrap_err();
        assert_eq!(error.code, ProtocolErrorCode::TerminalUnavailable);
        assert!(error.retryable && error.message.contains("completing retirement"));
        drop(interrupted);
        assert!(
            host.lock().unwrap().terminal_retirements[&source.id]
                .failure
                .is_some()
        );
        let error = process_request(
            &host,
            1,
            ClientRequest::ChatStart {
                session: candidate,
                source: None,
                terminate_source: false,
                execution_environment: Default::default(),
            },
        )
        .unwrap_err();
        assert_eq!(error.code, ProtocolErrorCode::TerminalUnavailable);
        assert!(
            error.retryable && error.message.contains("unconfirmed"),
            "retained failure permits inspection/retry, never a new native writer"
        );
        let error = process_request(
            &host,
            1,
            ClientRequest::CloseTerminalChecked {
                terminal_id: source.id,
                allow_active: false,
            },
        )
        .unwrap_err();
        assert!(error.retryable && original.holder.is_current());
        let mut ended = source.clone();
        ended.continuity = crate::model::TerminalContinuity::Exited {
            last_exit: crate::model::TerminalExit {
                code: Some(0),
                signal: None,
                occurred_at_unix_ms: 1,
            },
        };
        assert!(
            process_request(
                &host,
                1,
                ClientRequest::ReleaseCompletedTerminal {
                    session: ended.clone(),
                    allow_active: false,
                }
            )
            .unwrap_err()
            .retryable
        );
        assert_eq!(retention_pin_generation(&host, source.id), pin);
        drop(obstruction);
        std::fs::remove_file(marker).unwrap();
        wait_client_retirement(|| {
            matches!(
                handle.completed_retirement(&source, &proof),
                crate::terminal::TerminalRetirement::Released
            )
        });
        let exact_receipt = std::fs::read(&receipt).unwrap();
        assert!(!exact_receipt.is_empty());
        let saved_receipt = receipt.with_extension("closed.saved");
        std::fs::rename(&receipt, &saved_receipt).unwrap();
        let error = process_request(
            &host,
            1,
            ClientRequest::ReleaseCompletedTerminal {
                session: ended.clone(),
                allow_active: false,
            },
        )
        .unwrap_err();
        assert!(error.retryable);
        assert!(
            host.lock().unwrap().terminals[&source.id]
                .handle
                .same_worker(&handle)
        );
        assert_eq!(
            retention_pin_generation(&host, source.id),
            pin,
            "kernel exit without the exact receipt must retain the retry surface"
        );
        std::fs::rename(&saved_receipt, &receipt).unwrap();
        assert_eq!(std::fs::read(&receipt).unwrap(), exact_receipt);
        // A separate real WAL writer permits the membership read but refuses
        // our IMMEDIATE deletion for the existing bounded busy timeout.
        let blocked = rusqlite::Connection::open(directory.path().join("model.sqlite3")).unwrap();
        blocked.execute_batch("BEGIN IMMEDIATE").unwrap();
        let error = process_request(
            &host,
            1,
            ClientRequest::ReleaseCompletedTerminal {
                session: ended.clone(),
                allow_active: false,
            },
        )
        .unwrap_err();
        assert_eq!(error.code, ProtocolErrorCode::TerminalUnavailable);
        assert!(error.retryable && error.message.contains("pin could not be removed"));
        assert!(
            host.lock().unwrap().terminals[&source.id]
                .handle
                .same_worker(&handle)
        );
        assert_eq!(retention_pin_generation(&host, source.id), pin);
        assert!(
            host.lock().unwrap().terminal_retirements[&source.id]
                .failure
                .is_some()
        );
        blocked.execute_batch("ROLLBACK").unwrap();
        assert!(matches!(
            process_request(
                &host,
                1,
                ClientRequest::ReleaseCompletedTerminal {
                    session: ended,
                    allow_active: false,
                }
            )
            .unwrap()
            .0,
            ServerResponse::TerminalCloseChecked {
                outcome: crate::terminal_ipc::TerminalCloseResult::Closed,
                ..
            }
        ));
        assert!(!host.lock().unwrap().terminals.contains_key(&source.id));
        assert!(
            !host
                .lock()
                .unwrap()
                .terminal_retirements
                .contains_key(&source.id)
        );
        assert_eq!(retention_pin_generation(&host, source.id), Ok(None));
        wait_client_retirement(|| {
            !original.holder.is_current()
                && !crate::daemon::child_reaper::has_outstanding_ownership()
        });
        assert_eq!(std::fs::read(receipt).unwrap(), exact_receipt);
    }

    #[test]
    fn resident_release_drains_admitted_control_operations_before_retirement() {
        let (proof, host) = panic_test_host("resident-release-gate");
        let gate = control_gate(&host);
        // Model an admitted submission between authentication and acceptance.
        let admitted = gate.read().unwrap();
        let release = crate::terminal_ipc::ChatReleaseProof {
            session: crate::chat::Session {
                id: TerminalId::new(),
                proof: Some(proof),
                terminal: None,
                native_continuity: Default::default(),
                owned_codex_listener: None,
                launch: crate::chat::Launch {
                    provider: crate::agent::Provider::Claude,
                    session_id: None,
                    recipe: crate::model::LaunchRecipe {
                        program: "/missing-never-started".into(),
                        arguments: vec![],
                        working_directory: "/fixture".into(),
                        environment: Default::default(),
                        unset_environment: Default::default(),
                    },
                },
            },
            execution_instance: uuid::Uuid::new_v4(),
            generation: 0,
        };
        // Inspection stays a shared operation and promptly reports absence.
        assert!(
            process_request(
                &host,
                1,
                ClientRequest::ChatRelease {
                    proof: release.clone(),
                    dry_run: true
                }
            )
            .is_err()
        );
        let worker_host = host.clone();
        let (started, ready) = crossbeam_channel::bounded(1);
        let (finished, result) = crossbeam_channel::bounded(1);
        let worker = thread::spawn(move || {
            started.send(()).unwrap();
            finished
                .send(process_request(
                    &worker_host,
                    1,
                    ClientRequest::ChatRelease {
                        proof: release,
                        dry_run: false,
                    },
                ))
                .unwrap();
        });
        ready.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(
            matches!(
                result.recv_timeout(Duration::from_millis(100)),
                Err(crossbeam_channel::RecvTimeoutError::Timeout)
            ),
            "mutating release crossed an admitted operation before its acceptance completed"
        );
        drop(admitted);
        assert!(
            result
                .recv_timeout(Duration::from_secs(2))
                .unwrap()
                .is_err()
        );
        worker.join().unwrap();
    }

    #[test]
    fn completed_release_refuses_live_or_changed_sessions() {
        use crate::model::{
            BootId, HostIncarnation, HostSessionId, LaunchRecipe, TerminalExit, TerminalSession,
        };
        let proof = HostSessionProof {
            host_session_id: HostSessionId::new("completed-release"),
            boot_id: BootId::new("boot"),
            host_incarnation: HostIncarnation::new("incarnation"),
        };
        let host = Arc::new(crate::host::state::HostLock::new(
            super::super::state::HostState::new(proof),
        ));
        let model = tempfile::tempdir().unwrap();
        host.lock().unwrap().install_model(
            super::super::model::Registry::at(&model.path().join("model.sqlite3")).unwrap(),
        );
        let mut recipe = LaunchRecipe::shell("/".into()).unwrap();
        recipe.program = "/bin/cat".into();
        recipe.arguments.clear();
        let mut session = TerminalSession {
            id: TerminalId::new(),
            launch: recipe.clone(),
            owned_codex_listener: None,
            recovery_origin: None,
            predecessor: None,
            transcript_path: None,
            resume_hint: None,
            continuity: crate::model::TerminalContinuity::RelaunchAvailable {
                recipe,
                last_exit: None,
            },
        };
        let handle = crate::terminal::TerminalHandle::spawn(
            session.clone(),
            crate::terminal::TerminalConfig::default(),
            |_| {},
        )
        .unwrap();
        {
            let mut state = host.lock().unwrap();
            state.register_client(1);
            state.reserve_spawn(session.id).unwrap();
            state
                .finish_spawn(1, session.clone(), handle.clone(), true)
                .unwrap();
        }
        session.continuity = crate::model::TerminalContinuity::Exited {
            last_exit: TerminalExit {
                code: Some(0),
                signal: None,
                occurred_at_unix_ms: 0,
            },
        };
        assert!(
            process_request(
                &host,
                1,
                ClientRequest::ReleaseCompletedTerminal {
                    session: session.clone(),
                    allow_active: false,
                }
            )
            .is_err()
        );
        assert!(
            !handle.is_completed(),
            "a forged completed receipt cannot stop a running process"
        );
        handle.shutdown().unwrap();
        let mut changed = session.clone();
        changed.launch.arguments.push("different".into());
        assert!(
            process_request(
                &host,
                1,
                ClientRequest::ReleaseCompletedTerminal {
                    session: changed,
                    allow_active: false
                }
            )
            .is_err()
        );
        let _ = retry_retirement_close(&host, || ClientRequest::ReleaseCompletedTerminal {
            session: session.clone(),
            allow_active: false,
        });
    }

    #[test]
    fn completed_terminal_requires_confirmation_for_its_exact_native_actor() {
        assert_completed_native_confirmation(crate::agent::Provider::Codex);
    }

    #[test]
    fn completed_console_requires_confirmation_for_its_exact_native_actor() {
        assert_completed_native_confirmation(crate::agent::Provider::Claude);
    }

    fn assert_completed_native_confirmation(provider: crate::agent::Provider) {
        use crate::{
            chat::{
                self,
                protocol::{Adapter, ProcessSpec, Step},
            },
            model::{
                BootId, HostIncarnation, HostSessionId, TerminalContinuity, TerminalExit,
                TerminalSession,
            },
        };
        struct Quiet;
        impl Adapter for Quiet {
            fn initialize(&mut self) -> Result<Step, String> {
                Ok(Step::default())
            }
            fn command(&mut self, _: chat::Command) -> Result<Step, String> {
                Ok(Step::default())
            }
            fn receive(&mut self, _: serde_json::Value) -> Result<Step, String> {
                Ok(Step::default())
            }
        }
        let proof = HostSessionProof {
            host_session_id: HostSessionId::new("paired-completion"),
            boot_id: BootId::new("boot"),
            host_incarnation: HostIncarnation::new("incarnation"),
        };
        let host = Arc::new(crate::host::state::HostLock::new(
            super::super::state::HostState::new(proof.clone()),
        ));
        let model = tempfile::tempdir().unwrap();
        host.lock().unwrap().install_model(
            super::super::model::Registry::at(&model.path().join("model.sqlite3")).unwrap(),
        );
        let mut recipe = crate::model::LaunchRecipe::shell("/".into()).unwrap();
        recipe.program = "/bin/true".into();
        recipe.arguments.clear();
        let session = TerminalSession {
            id: TerminalId::new(),
            launch: recipe.clone(),
            owned_codex_listener: None,
            recovery_origin: None,
            predecessor: None,
            transcript_path: None,
            resume_hint: None,
            continuity: TerminalContinuity::Exited {
                last_exit: TerminalExit {
                    code: Some(0),
                    signal: None,
                    occurred_at_unix_ms: 1,
                },
            },
        };
        let terminal =
            crate::terminal::TerminalHandle::spawn(session.clone(), Default::default(), |_| {})
                .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !terminal.is_completed() {
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(5));
        }
        recipe.program = "/bin/cat".into();
        let spec = ProcessSpec::from(&recipe);
        let mut initial = Step::default();
        let store = chat::runtime::preflight(&spec, &mut initial).unwrap();
        let actor = chat::runtime::Handle::start(
            spec,
            Box::new(Quiet),
            initial,
            store,
            Some("exact-thread".into()),
            |_| {},
        )
        .unwrap();
        {
            let mut state = host.lock().unwrap();
            state.register_client(1);
            state.reserve_spawn(session.id).unwrap();
            state
                .finish_spawn(1, session.clone(), terminal.clone(), false)
                .unwrap();
            state.chats.insert(
                session.id,
                super::super::chat::Record {
                    execution_instance: uuid::Uuid::new_v4(),
                    console_recipe: (provider != crate::agent::Provider::Codex)
                        .then(|| session.launch.clone()),
                    paired_terminal: Some(terminal),
                    session: chat::Session {
                        id: session.id,
                        native_continuity: Default::default(),
                        owned_codex_listener: None,
                        proof: Some(proof),
                        terminal: Some(session.clone()),
                        launch: chat::Launch {
                            provider,
                            recipe,
                            session_id: Some("exact-thread".into()),
                        },
                    },
                    handle: actor.clone(),
                    source_process: None,
                    subscribers: Default::default(),
                    observations: Default::default(),
                    completion_observed: false,
                    returning_terminal: false,
                },
            );
        }
        let request = |allow_active| ClientRequest::ReleaseCompletedTerminal {
            session: session.clone(),
            allow_active,
        };
        assert!(matches!(
            process_request(&host, 1, request(false)).unwrap().0,
            ServerResponse::TerminalCloseChecked {
                outcome: crate::terminal_ipc::TerminalCloseResult::Active,
                ..
            }
        ));
        assert!(
            !actor.is_completed(),
            "a completed PTY does not authorize stopping its native actor"
        );
        assert!(
            !host
                .lock()
                .unwrap()
                .notification_closed
                .contains(&session.id)
        );
        host.lock()
            .unwrap()
            .chats
            .get_mut(&session.id)
            .unwrap()
            .session
            .terminal
            .as_mut()
            .unwrap()
            .launch
            .arguments
            .push("changed".into());
        assert!(process_request(&host, 1, request(true)).is_err());
        assert!(!actor.is_completed());
        host.lock()
            .unwrap()
            .chats
            .get_mut(&session.id)
            .unwrap()
            .session
            .terminal = Some(session.clone());
        assert!(matches!(
            retry_retirement_close(&host, || request(true)).0,
            ServerResponse::TerminalCloseChecked {
                outcome: crate::terminal_ipc::TerminalCloseResult::Closed,
                ..
            }
        ));
        assert!(actor.is_completed());
        assert!(!host.lock().unwrap().chats.contains_key(&session.id));
        let state = host.lock().unwrap();
        assert!(!state.terminals.contains_key(&session.id));
        assert!(!state.notification_closed.contains(&session.id));
    }

    #[test]
    fn slow_chat_close_allows_list_but_fences_upgrade_and_shutdown() {
        use crate::{
            agent::Provider,
            chat::{
                self, Command,
                protocol::{Adapter, ProcessSpec, Step},
                runtime::Handle,
            },
            model::{BootId, HostIncarnation, HostSessionId, LaunchRecipe},
        };
        use std::collections::BTreeMap;

        struct DelayedCommand {
            entered: crossbeam_channel::Sender<()>,
            release: crossbeam_channel::Receiver<()>,
        }
        impl Adapter for DelayedCommand {
            fn initialize(&mut self) -> Result<Step, String> {
                Ok(Step::default())
            }
            fn command(&mut self, _: Command) -> Result<Step, String> {
                self.entered.send(()).map_err(|error| error.to_string())?;
                self.release
                    .recv_timeout(Duration::from_secs(5))
                    .map_err(|error| error.to_string())?;
                Ok(Step::default())
            }
            fn receive(&mut self, _: serde_json::Value) -> Result<Step, String> {
                Ok(Step::default())
            }
        }

        let directory = tempfile::tempdir().unwrap();
        let proof = HostSessionProof {
            host_session_id: HostSessionId::new("control-barrier"),
            boot_id: BootId::new("boot"),
            host_incarnation: HostIncarnation::new("incarnation"),
        };
        let host = Arc::new(crate::host::state::HostLock::new(
            super::super::state::HostState::new(proof.clone()),
        ));
        let recipe = LaunchRecipe {
            unset_environment: Default::default(),
            program: "/bin/cat".into(),
            arguments: Vec::new(),
            working_directory: directory.path().to_path_buf(),
            environment: BTreeMap::new(),
        };
        let spec = ProcessSpec::from(&recipe);
        let mut initial = Step::default();
        let store = chat::runtime::preflight(&spec, &mut initial).unwrap();
        let (entered, pending) = crossbeam_channel::bounded(1);
        let (release, released) = crossbeam_channel::bounded(1);
        let handle = Handle::start(
            spec,
            Box::new(DelayedCommand {
                entered,
                release: released,
            }),
            initial,
            store,
            None,
            |_| {},
        )
        .unwrap();
        let chat_id = TerminalId::new();
        let capability = {
            let mut state = host.lock().unwrap();
            state.register_client(1);
            state.register_client(2);
            state.chats.insert(
                chat_id,
                super::super::chat::Record {
                    execution_instance: uuid::Uuid::nil(),
                    console_recipe: None,
                    paired_terminal: None,
                    session: chat::Session {
                        native_continuity: Default::default(),
                        owned_codex_listener: None,
                        id: chat_id,
                        launch: chat::Launch {
                            provider: Provider::Claude,
                            recipe,
                            session_id: None,
                        },
                        proof: Some(proof),
                        terminal: None,
                    },
                    handle: handle.clone(),
                    source_process: None,
                    subscribers: BTreeMap::from([(1, false)]),
                    observations: BTreeMap::new(),
                    completion_observed: false,
                    returning_terminal: false,
                },
            );
            state.shutdown_preflight(Instant::now()).unwrap().capability
        };
        let command_handle = handle.clone();
        let command = thread::spawn(move || command_handle.command(Command::Interrupt));
        pending.recv_timeout(Duration::from_secs(2)).unwrap();
        let close_host = Arc::clone(&host);
        let close = thread::spawn(move || {
            process_request(&close_host, 1, ClientRequest::ChatClose { chat_id })
        });
        let gate = control_gate(&host);
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match gate.try_write() {
                Err(std::sync::TryLockError::WouldBlock) => break,
                Ok(guard) => drop(guard),
                Err(error) => panic!("control gate poisoned: {error}"),
            }
            assert!(Instant::now() < deadline, "chat close was not admitted");
            thread::yield_now();
        }

        // The actual close is joining the blocked provider worker. A separate
        // client's ordinary request must complete before that worker is released.
        let list_host = Arc::clone(&host);
        let (listed, listing) = crossbeam_channel::bounded(1);
        let list = thread::spawn(move || {
            listed
                .send(process_request(&list_host, 2, ClientRequest::List))
                .unwrap();
        });
        assert!(matches!(
            listing
                .recv_timeout(Duration::from_secs(1))
                .unwrap()
                .unwrap()
                .0,
            ServerResponse::Listed { .. }
        ));
        list.join().unwrap();

        let upgrade_host = Arc::clone(&host);
        let (upgraded, upgrading) = crossbeam_channel::bounded(1);
        let upgrade = thread::spawn(move || {
            upgraded
                .send(process_request(
                    &upgrade_host,
                    2,
                    ClientRequest::PrepareUpgrade {
                        capability,
                        relaunch_agents: false,
                    },
                ))
                .unwrap();
        });
        assert!(matches!(
            upgrading.recv_timeout(Duration::from_millis(100)),
            Err(crossbeam_channel::RecvTimeoutError::Timeout)
        ));
        release.send(()).unwrap();
        let _ = command.join().unwrap();
        assert!(matches!(
            close.join().unwrap().unwrap().0,
            ServerResponse::ChatClosed { chat_id: id, .. } if id == chat_id
        ));
        let error = upgrading
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .unwrap_err();
        assert!(error.message.contains("inventory changed"));
        upgrade.join().unwrap();
        assert!(!host.lock().unwrap().shutting_down);
        assert!(!host.lock().unwrap().chats.contains_key(&chat_id));

        // A successful exclusive shutdown commits the same admission fence,
        // without persisting an upgrade checkpoint in the user's runtime path.
        assert!(matches!(
            process_request(&host, 2, ClientRequest::Shutdown)
                .unwrap()
                .0,
            ServerResponse::ShutdownAccepted
        ));
        let error = process_request(&host, 2, ClientRequest::List).unwrap_err();
        assert_eq!(error.code, ProtocolErrorCode::TerminalUnavailable);
        assert!(!error.retryable);
        assert!(prepare_resume(&host, 2, Vec::new()).is_err());
    }

    #[test]
    fn daemon_activity_pushes_without_refresh_and_slow_probe_does_not_block_ipc() {
        fn send(
            stream: &mut UnixStream,
            proof: &HostSessionProof,
            request_id: u64,
            request: ClientRequest,
        ) {
            FrameCodec::new(MAX_CLIENT_FRAME_BYTES)
                .write(
                    stream,
                    &ClientEnvelope {
                        version: PROTOCOL_VERSION,
                        request_id,
                        proof: Some(proof.clone()),
                        request,
                    },
                )
                .unwrap();
        }

        fn response(stream: &mut UnixStream, proof: &HostSessionProof) -> ServerEnvelope {
            let mut codec = FrameCodec::new(MAX_FRAME_BYTES);
            loop {
                let envelope = codec.read::<ServerEnvelope>(stream).unwrap().unwrap();
                assert_eq!(&envelope.proof, proof);
                if envelope.request_id.is_some() {
                    return envelope;
                }
            }
        }

        fn wait_file(path: &std::path::Path) {
            let deadline = Instant::now() + Duration::from_secs(2);
            while !std::fs::metadata(path).is_ok_and(|metadata| metadata.len() > 0) {
                assert!(
                    Instant::now() < deadline,
                    "fixture did not start: {}",
                    path.display()
                );
                thread::sleep(Duration::from_millis(5));
            }
        }

        struct Cleanup {
            stream: UnixStream,
            server: Option<thread::JoinHandle<()>>,
            handle: Option<crate::terminal::TerminalHandle>,
        }
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = self.stream.shutdown(Shutdown::Both);
                if let Some(handle) = &self.handle {
                    let _ = handle.shutdown();
                }
                if let Some(server) = self.server.take() {
                    server.join().unwrap();
                }
            }
        }

        let directory = tempfile::tempdir().unwrap();
        let executable = directory.path().join("claude");
        // A real ELF foreground executable with an exact provider basename.
        // The copied shell reads our local `agents` script for the real probe;
        // no installed provider, credentials, network, or model is involved.
        std::fs::copy("/bin/sh", &executable).unwrap();
        std::fs::write(
            directory.path().join("agents"),
            "printf '%s' \"$$\" > probe-started\nexec /bin/sleep 30\n",
        )
        .unwrap();
        let launch = crate::model::LaunchRecipe {
            unset_environment: Default::default(),
            program: executable.to_string_lossy().into_owned(),
            arguments: vec![
                "-c".into(),
                "printf ready > terminal-ready; while IFS= read -r line; do printf '%s\\n' \"$line\"; done".into(),
            ],
            working_directory: directory.path().to_path_buf(),
            environment: [
                ("HOME".into(), directory.path().to_string_lossy().into_owned()),
                ("CLAUDE_CONFIG_DIR".into(), directory.path().to_string_lossy().into_owned()),
                ("PATH".into(), "/usr/bin:/bin".into()),
            ].into_iter().collect(),
        };
        let terminal_id = TerminalId::new();
        let session = crate::model::TerminalSession {
            owned_codex_listener: None,
            recovery_origin: None,
            predecessor: None,
            id: terminal_id,
            launch: launch.clone(),
            transcript_path: None,
            resume_hint: None,
            continuity: crate::model::TerminalContinuity::RelaunchAvailable {
                recipe: launch,
                last_exit: None,
            },
        };
        let proof = HostSessionProof {
            host_session_id: crate::model::HostSessionId::new("slow-probe"),
            boot_id: crate::model::BootId::new("slow-probe"),
            host_incarnation: crate::model::HostIncarnation::new("slow-probe"),
        };
        let host = Arc::new(crate::host::state::HostLock::new(
            super::super::state::HostState::new(proof.clone()),
        ));
        let (mut stream, server_stream) = UnixStream::pair().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let server_host = Arc::clone(&host);
        let mut cleanup = Cleanup {
            stream: stream.try_clone().unwrap(),
            server: Some(thread::spawn(move || serve(server_stream, 1, server_host))),
            handle: None,
        };
        send(
            &mut stream,
            &proof,
            1,
            ClientRequest::Connect {
                expected_host: Some(proof.clone()),
                resume: Vec::new(),
            },
        );
        assert!(matches!(
            response(&mut stream, &proof).message,
            ServerMessage::Response {
                response: ServerResponse::Connected { .. }
            }
        ));
        send(
            &mut stream,
            &proof,
            2,
            ClientRequest::Spawn {
                session,
                options: Default::default(),
            },
        );
        assert!(matches!(
            response(&mut stream, &proof).message,
            ServerMessage::Response {
                response: ServerResponse::Spawned { .. }
            }
        ));
        cleanup.handle = Some(host.lock().unwrap().handle(1, terminal_id).unwrap());
        wait_file(&directory.path().join("terminal-ready"));
        // The daemon samples without any refresh request. Process discovery is
        // separately owned by the PTY; force its initial tick for this fixture.
        assert!(
            cleanup
                .handle
                .as_ref()
                .unwrap()
                .observe_agent_process()
                .is_some()
        );
        let monitor = super::super::activity::Monitor::start(&host).unwrap();
        let started = directory.path().join("probe-started");
        wait_file(&started);
        let probe_pid: i32 = std::fs::read_to_string(&started).unwrap().parse().unwrap();

        // Compatibility refreshes read the cache immediately; none schedules a
        // probe or creates a per-connection worker. Input shares this socket.
        for request_id in 3..12 {
            send(
                &mut stream,
                &proof,
                request_id,
                ClientRequest::RefreshAgentActivity { terminal_id },
            );
        }
        send(
            &mut stream,
            &proof,
            12,
            ClientRequest::Input {
                terminal_id,
                bytes: b"responsive\n".to_vec(),
            },
        );
        send(
            &mut stream,
            &proof,
            13,
            ClientRequest::Resize {
                terminal_id,
                size: crate::terminal_ipc::WireTerminalSize {
                    columns: 90,
                    rows: 30,
                    cell_width_px: 9,
                    cell_height_px: 18,
                },
            },
        );
        send(
            &mut stream,
            &proof,
            14,
            ClientRequest::Focus {
                terminal_id,
                focused: true,
            },
        );
        send(
            &mut stream,
            &proof,
            15,
            ClientRequest::AcknowledgeRedraw {
                terminal_id,
                generation: 0,
            },
        );
        for request_id in 3..16 {
            let envelope = response(&mut stream, &proof);
            assert_eq!(envelope.request_id, Some(request_id));
            assert!(matches!(envelope.message, ServerMessage::Response { .. }));
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        while unsafe { libc::kill(probe_pid, 0) } == 0 {
            assert!(
                Instant::now() < deadline,
                "timed-out daemon probe must be reaped"
            );
            thread::sleep(Duration::from_millis(5));
        }

        // The next observation succeeds after failure backoff, with no client
        // refresh, and arrives as an ordinary semantic event.
        let process = cleanup
            .handle
            .as_ref()
            .unwrap()
            .cached_agent_process()
            .unwrap();
        let rows = serde_json::json!([{
            "kind": "interactive", "pid": process.pid,
            "cwd": directory.path(), "status": "busy", "title": "Activity fixture"
        }]);
        std::fs::write(
            directory.path().join("agents"),
            format!("printf '%s' '{}'\n", rows),
        )
        .unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(8)))
            .unwrap();
        let working = Some(crate::agent::Activity {
            provider: crate::agent::Provider::Claude,
            state: crate::agent::ActivityState::Working,
        });
        let mut codec = FrameCodec::new(MAX_FRAME_BYTES);
        loop {
            let envelope = codec.read::<ServerEnvelope>(&mut stream).unwrap().unwrap();
            if matches!(envelope.message, ServerMessage::Event {
                event: WireTerminalEvent::AgentActivityChanged { activity }, ..
            } if activity == working)
            {
                assert_eq!(envelope.request_id, None);
                break;
            }
        }
        assert_eq!(cleanup.handle.as_ref().unwrap().agent_activity(), working);

        // Closing a GUI is independent of the daemon monitor, and a replacement
        // connection receives current activity in its attach snapshot.
        let handle = cleanup.handle.take();
        drop(cleanup);
        let (mut stream, server_stream) = UnixStream::pair().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let server_host = Arc::clone(&host);
        let cleanup = Cleanup {
            stream: stream.try_clone().unwrap(),
            server: Some(thread::spawn(move || serve(server_stream, 2, server_host))),
            handle,
        };
        send(
            &mut stream,
            &proof,
            1,
            ClientRequest::Connect {
                expected_host: Some(proof.clone()),
                resume: Vec::new(),
            },
        );
        assert!(matches!(
            response(&mut stream, &proof).message,
            ServerMessage::Response {
                response: ServerResponse::Connected { .. }
            }
        ));
        send(
            &mut stream,
            &proof,
            2,
            ClientRequest::Attach {
                terminal_id,
                after_generation: 0,
                after_event_sequence: 0,
            },
        );
        let envelope = response(&mut stream, &proof);
        assert!(matches!(envelope.message, ServerMessage::Response {
            response: ServerResponse::Attached { terminal, .. }
        } if terminal.agent_activity == working));
        drop(cleanup);
        drop(monitor);
        assert!(!host.lock().unwrap().clients.contains_key(&2));
    }

    #[test]
    fn unsupported_shutdown_revision_does_not_mutate_host() {
        let proof = HostSessionProof {
            host_session_id: crate::model::HostSessionId::new("test"),
            boot_id: crate::model::BootId::new("test"),
            host_incarnation: crate::model::HostIncarnation::new("test"),
        };
        let host = Arc::new(crate::host::state::HostLock::new(
            super::super::state::HostState::new(proof),
        ));
        let error = process_request(
            &host,
            1,
            ClientRequest::ShutdownV1 {
                version: SHUTDOWN_VERSION + 1,
                operation: ShutdownOperation::Stop {
                    require_empty: false,
                    expected_generation: None,
                },
            },
        )
        .unwrap_err();
        assert_eq!(error.code, ProtocolErrorCode::UnsupportedVersion);
        assert!(
            host.lock()
                .unwrap_or_else(|error| error.into_inner())
                .reserve_spawn(crate::model::TerminalId::new())
                .is_ok()
        );
    }

    fn panic_test_host(name: &str) -> (HostSessionProof, SharedHost) {
        let proof = HostSessionProof {
            host_session_id: crate::model::HostSessionId::new(name),
            boot_id: crate::model::BootId::new(name),
            host_incarnation: crate::model::HostIncarnation::new(name),
        };
        let host = Arc::new(crate::host::state::HostLock::new(
            super::super::state::HostState::new(proof.clone()),
        ));
        (proof, host)
    }

    /// The host mutex panics when its holder locks it again, on the promise
    /// that only that request is lost. Its sender must hear that it failed and
    /// keep a connection that still answers.
    #[test]
    fn a_request_that_panics_is_answered_and_the_connection_keeps_serving() {
        static RELOCKED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        fn relocking_once(
            host: &SharedHost,
            client_id: ClientId,
            request: ClientRequest,
        ) -> Result<RequestOutcome, ProtocolError> {
            if !RELOCKED.swap(true, std::sync::atomic::Ordering::AcqRel) {
                let _held = host.lock().unwrap_or_else(|p| p.into_inner());
                let _ = host.lock();
            }
            process_request(host, client_id, request)
        }
        fn exchange(
            stream: &mut UnixStream,
            proof: &HostSessionProof,
            request_id: u64,
            request: ClientRequest,
        ) -> ServerMessage {
            FrameCodec::new(MAX_CLIENT_FRAME_BYTES)
                .write(
                    stream,
                    &ClientEnvelope {
                        version: PROTOCOL_VERSION,
                        request_id,
                        proof: Some(proof.clone()),
                        request,
                    },
                )
                .unwrap();
            let mut codec = FrameCodec::new(MAX_FRAME_BYTES);
            loop {
                let envelope = codec
                    .read::<ServerEnvelope>(stream)
                    .expect("the request was never answered")
                    .expect("the connection was closed");
                if envelope.request_id == Some(request_id) {
                    return envelope.message;
                }
            }
        }

        let (proof, host) = panic_test_host("panicking-request");
        let (mut stream, server_stream) = UnixStream::pair().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let server_host = Arc::clone(&host);
        let server =
            thread::spawn(move || serve_with(server_stream, 1, server_host, relocking_once));
        assert!(matches!(
            exchange(
                &mut stream,
                &proof,
                1,
                ClientRequest::Connect {
                    expected_host: Some(proof.clone()),
                    resume: Vec::new(),
                },
            ),
            ServerMessage::Response {
                response: ServerResponse::Connected { .. }
            }
        ));

        let ServerMessage::Error(error) = exchange(&mut stream, &proof, 2, ClientRequest::List)
        else {
            panic!("the panicking request must be answered with an error");
        };
        assert_eq!(error.code, ProtocolErrorCode::Internal);
        assert!(matches!(
            exchange(&mut stream, &proof, 3, ClientRequest::List),
            ServerMessage::Response { .. }
        ));

        drop(stream);
        server.join().expect("the client thread survives the panic");
        assert!(host.lock().unwrap().clients.is_empty());
    }

    #[test]
    fn a_client_thread_that_unwinds_unregisters_its_client_and_releases_its_writer() {
        let (_, host) = panic_test_host("unwinding-client");
        let (delivered, delivery) = std::sync::mpsc::channel();
        let client_host = Arc::clone(&host);
        let client = thread::spawn(move || {
            let registration = Registration::new(client_host, 7);
            delivered.send(Arc::clone(&registration.delivery)).unwrap();
            panic!("fixture client failure");
        });
        let delivery = delivery.recv().unwrap();
        assert!(client.join().is_err());
        assert!(!host.lock().unwrap().clients.contains_key(&7));
        // The writer blocks here until the queue is closed, holding the socket.
        assert!(delivery.pop().is_none());
    }

    #[test]
    fn handshake_timeout_tracks_one_absolute_deadline() {
        let now = Instant::now();
        let deadline = now + Duration::from_secs(5);
        assert_eq!(
            remaining_handshake_timeout(deadline, now + Duration::from_secs(2)).unwrap(),
            Duration::from_secs(3)
        );
        let error = remaining_handshake_timeout(deadline, deadline).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        let error =
            remaining_handshake_timeout(deadline, deadline + Duration::from_secs(1)).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    }

    /// A terminal starts existing when some client spawns it, and no redraw,
    /// event or snapshot edge says so. Without this announcement a connected
    /// view cannot learn that another client - the CLI - created a terminal.
    #[test]
    fn spawning_announces_the_terminal_to_observers_but_never_to_its_creator() {
        use crate::model::{BootId, HostIncarnation, HostSessionId, TerminalContinuity};

        let directory = tempfile::tempdir().unwrap();
        let recipe = crate::model::LaunchRecipe::shell(directory.path().to_owned()).unwrap();
        let session = crate::model::TerminalSession {
            owned_codex_listener: None,
            recovery_origin: None,
            predecessor: None,
            id: crate::model::TerminalId::new(),
            launch: recipe.clone(),
            transcript_path: None,
            resume_hint: None,
            continuity: TerminalContinuity::RelaunchAvailable {
                recipe,
                last_exit: None,
            },
        };
        let terminal_id = session.id;
        let host = Arc::new(crate::host::state::HostLock::new(
            super::super::state::HostState::new(HostSessionProof {
                host_session_id: HostSessionId::new("announce-session"),
                boot_id: BootId::new("boot"),
                host_incarnation: HostIncarnation::new("incarnation"),
            }),
        ));
        let (creator, observer, unsubscribed) = {
            let mut state = host.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            let creator = state.register_client(1);
            let observer = state.register_client(2);
            let unsubscribed = state.register_client(3);
            state.watch_terminals(1);
            state.watch_terminals(2);
            (creator, observer, unsubscribed)
        };

        let (response, _, _) = process_request(
            &host,
            1,
            ClientRequest::Spawn {
                session,
                options: crate::terminal_ipc::TerminalSpawnOptions::default(),
            },
        )
        .unwrap();
        assert!(matches!(response, ServerResponse::Spawned { .. }));

        let announced = |delivery: &Arc<DeliveryQueue>| {
            delivery.close_after_drain();
            std::iter::from_fn(|| delivery.pop())
                .filter_map(|queued| match queued.message {
                    ServerMessage::TerminalCreated { terminal } => Some(terminal),
                    _ => None,
                })
                .collect::<Vec<_>>()
        };
        let observed = announced(&observer);
        assert_eq!(observed.len(), 1);
        assert_eq!(observed[0].session.id, terminal_id);
        assert_eq!(
            observed[0].session.launch.working_directory,
            directory.path()
        );
        // The recipient is not attached, so it is owed the proof it needs to
        // attach rather than a claim that it already has.
        assert!(!observed[0].attached);
        assert!(matches!(
            observed[0].session.continuity,
            TerminalContinuity::DetachedSameBoot { .. }
        ));
        assert!(
            announced(&creator).is_empty(),
            "a spawning client already holds the descriptor its own spawn returned"
        );
        assert!(
            announced(&unsubscribed).is_empty(),
            "a client that never asked for inventory edges must not receive one"
        );

        // The terminal worker's wake callback takes the host lock, so joining
        // it while holding that lock would deadlock the shutdown.
        let handle = host
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .handle(1, terminal_id)
            .unwrap();
        handle.shutdown().unwrap();
    }

    #[test]
    fn transfer_requests_refuse_invalid_membership_without_acknowledging_or_mutating_it() {
        use crate::{
            model::{
                BootId, BrowserId, BrowserSession, CardId, CardKind, ChannelId, HostIncarnation,
                HostSessionId,
            },
            workspace_model::{
                ModelEdit,
                views::{Change, Pane, Scope},
            },
        };
        let directory = tempfile::tempdir().unwrap();
        let repository = directory.path().join("project");
        let other = directory.path().join("other");
        let host = Arc::new(crate::host::state::HostLock::new(
            super::super::state::HostState::new(HostSessionProof {
                host_session_id: HostSessionId::new("transfer-session"),
                boot_id: BootId::new("boot"),
                host_incarnation: HostIncarnation::new("incarnation"),
            }),
        ));
        {
            let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
            state.install_model(
                super::super::model::Registry::at(&directory.path().join("model.sqlite3")).unwrap(),
            );
            state.register_client(1);
        }
        let edit = |edit| process_request(&host, 1, ClientRequest::EditWorkspaceModel { edit });
        for path in [&repository, &other] {
            std::fs::create_dir(path).unwrap();
            edit(ModelEdit::OpenProject {
                repository: path.clone(),
                name: "project".into(),
                kind: Default::default(),
            })
            .unwrap();
        }
        let source = ChannelId::new();
        let destination = ChannelId::new();
        let foreign = ChannelId::new();
        for (id, repository) in [
            (source, repository.clone()),
            (destination, repository),
            (foreign, other),
        ] {
            edit(ModelEdit::CreateChannel {
                id,
                repository,
                title: "channel".into(),
                title_source: Default::default(),
            })
            .unwrap();
        }
        let pane = Pane {
            id: CardId::new(),
            title: "browser".into(),
            user_title: None,
            pending_terminal_start: false,
            width_fraction: 0.5,
            scope: None,
            kind: CardKind::Browser(BrowserSession {
                id: BrowserId::new(),
                current_url: "https://example.com".parse().unwrap(),
                history: vec![],
                history_index: 0,
                scroll_x: 0.,
                scroll_y: 0.,
                fidelity: crate::model::BrowserRestoreFidelity::UrlOnly,
            }),
        };
        edit(ModelEdit::SeedView {
            scope: Scope::Channel(source),
            panes: vec![pane.clone()],
            closed: vec![],
        })
        .unwrap();
        let before = host
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .read_model()
            .unwrap();
        let revision = before
            .views
            .iter()
            .find(|view| view.scope == Scope::Channel(source))
            .unwrap()
            .revision;
        for (scope, id, to) in [
            (Scope::Channel(ChannelId::new()), pane.id, destination),
            (Scope::Channel(source), pane.id, ChannelId::new()),
            (Scope::Channel(source), CardId::new(), destination),
            (Scope::Channel(source), pane.id, foreign),
            (Scope::Channel(source), CardId::new(), source),
        ] {
            let error = edit(ModelEdit::ChangeView {
                scope,
                expected_revision: revision,
                operation: uuid::Uuid::new_v4(),
                change: Change::Transfer {
                    id,
                    destination: Scope::Channel(to),
                    before: None,
                },
            })
            .unwrap_err();
            assert_eq!(error.code, ProtocolErrorCode::InvalidRequest);
            assert!(!error.retryable);
            assert_eq!(
                host.lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .read_model()
                    .unwrap(),
                before
            );
        }
        let valid = ModelEdit::ChangeView {
            scope: Scope::Channel(source),
            expected_revision: revision,
            operation: uuid::Uuid::new_v4(),
            change: Change::Transfer {
                id: pane.id,
                destination: Scope::Channel(destination),
                before: None,
            },
        };
        let moved = edit(valid.clone()).unwrap().0;
        let ServerResponse::WorkspaceModel { model: moved } = moved else {
            panic!("transfer must return its committed model")
        };
        let replayed = edit(valid).unwrap().0;
        let ServerResponse::WorkspaceModel { model: replayed } = replayed else {
            panic!("replay must return its committed model")
        };
        assert_eq!(
            moved, replayed,
            "successful replay must bypass the now-empty source pane check"
        );
    }

    #[test]
    fn stale_pane_source_updates_return_retryable_conflict_without_partial_success() {
        use crate::{
            model::{
                BootId, BrowserId, BrowserSession, CardId, CardKind, ChannelId, HostIncarnation,
                HostSessionId,
            },
            workspace_model::{
                ModelEdit,
                views::{Change, Pane, Scope},
            },
        };
        let directory = tempfile::tempdir().unwrap();
        let repository = directory.path().join("project");
        std::fs::create_dir(&repository).unwrap();
        let host = Arc::new(crate::host::state::HostLock::new(
            super::super::state::HostState::new(HostSessionProof {
                host_session_id: HostSessionId::new("source-update"),
                boot_id: BootId::new("boot"),
                host_incarnation: HostIncarnation::new("incarnation"),
            }),
        ));
        {
            let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
            state.install_model(
                super::super::model::Registry::at(&directory.path().join("model.sqlite3")).unwrap(),
            );
            state.register_client(1);
        }
        let edit = |edit| process_request(&host, 1, ClientRequest::EditWorkspaceModel { edit });
        edit(ModelEdit::OpenProject {
            repository: repository.clone(),
            name: "project".into(),
            kind: Default::default(),
        })
        .unwrap();
        let channel = ChannelId::new();
        edit(ModelEdit::CreateChannel {
            id: channel,
            repository,
            title: "channel".into(),
            title_source: Default::default(),
        })
        .unwrap();
        let original = Pane {
            id: CardId::new(),
            title: "browser".into(),
            user_title: None,
            pending_terminal_start: false,
            width_fraction: 0.5,
            scope: None,
            kind: CardKind::Browser(BrowserSession {
                id: BrowserId::new(),
                current_url: "https://example.com".parse().unwrap(),
                history: vec![],
                history_index: 0,
                scroll_x: 0.,
                scroll_y: 0.,
                fidelity: crate::model::BrowserRestoreFidelity::UrlOnly,
            }),
        };
        edit(ModelEdit::SeedView {
            scope: Scope::Channel(channel),
            panes: vec![original.clone()],
            closed: vec![],
        })
        .unwrap();
        let read = || {
            host.lock()
                .unwrap_or_else(|p| p.into_inner())
                .read_model()
                .unwrap()
        };
        let revision = |model: &crate::workspace_model::WorkspaceModel| {
            model
                .views
                .iter()
                .find(|view| view.scope == Scope::Channel(channel))
                .unwrap()
                .revision
        };
        let mut current = original.clone();
        if let CardKind::Browser(browser) = &mut current.kind {
            browser.current_url = "https://example.com/newer".parse().unwrap();
        }
        edit(ModelEdit::ChangeView {
            scope: Scope::Channel(channel),
            expected_revision: revision(&read()),
            operation: uuid::Uuid::new_v4(),
            change: Change::Update {
                before: original.clone(),
                pane: current,
            },
        })
        .unwrap();
        let mut stale = original.clone();
        stale.title = "must not partly commit".into();
        stale.scope = Some(directory.path().join("stale"));
        let before = read();
        let error = edit(ModelEdit::ChangeView {
            scope: Scope::Channel(channel),
            expected_revision: revision(&before),
            operation: uuid::Uuid::new_v4(),
            change: Change::Update {
                before: original,
                pane: stale,
            },
        })
        .unwrap_err();
        assert_eq!(error.code, ProtocolErrorCode::ViewConflict);
        assert!(error.retryable);
        assert_eq!(read(), before);
    }

    /// A shared-model edit reaches every window watching the model and never
    /// its originator, which already has the model the edit returned. A client
    /// that only reads - every CLI invocation - receives nothing at all.
    #[test]
    fn editing_the_model_announces_to_watchers_but_never_to_its_originator() {
        use crate::model::{BootId, HostIncarnation, HostSessionId};

        let directory = tempfile::tempdir().unwrap();
        let host = Arc::new(crate::host::state::HostLock::new(
            super::super::state::HostState::new(HostSessionProof {
                host_session_id: HostSessionId::new("model-session"),
                boot_id: BootId::new("boot"),
                host_incarnation: HostIncarnation::new("incarnation"),
            }),
        ));
        let (editor, watcher, unsubscribed) = {
            let mut state = host.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            state.install_model(
                super::super::model::Registry::at(&directory.path().join("model.sqlite3")).unwrap(),
            );
            let editor = state.register_client(1);
            let watcher = state.register_client(2);
            let unsubscribed = state.register_client(3);
            (editor, watcher, unsubscribed)
        };
        // Subscribing returns the model in the same response, so the watcher
        // cannot miss an edit applied between subscribing and reading.
        let (subscribed, _, _) =
            process_request(&host, 2, ClientRequest::WatchWorkspaceModel).unwrap();
        let ServerResponse::WorkspaceModel { model } = subscribed else {
            panic!("subscribing must answer with the model");
        };
        assert_eq!(model.sequence, 0);

        let repository = directory.path().join("repo");
        std::fs::create_dir(&repository).unwrap();
        let opened = process_request(
            &host,
            1,
            ClientRequest::EditWorkspaceModel {
                edit: crate::workspace_model::ModelEdit::OpenProject {
                    kind: Default::default(),
                    repository: repository.clone(),
                    name: "repo".into(),
                },
            },
        )
        .unwrap();
        assert!(matches!(opened.0, ServerResponse::WorkspaceModel { .. }));
        let channel = crate::model::ChannelId::new();
        process_request(
            &host,
            1,
            ClientRequest::EditWorkspaceModel {
                edit: crate::workspace_model::ModelEdit::CreateChannel {
                    title_source: Default::default(),
                    id: channel,
                    repository,
                    title: "shared".into(),
                },
            },
        )
        .unwrap();

        let edges = |delivery: &Arc<DeliveryQueue>| {
            delivery.close_after_drain();
            std::iter::from_fn(|| delivery.pop())
                .filter_map(|queued| match queued.message {
                    ServerMessage::WorkspaceModelEdited { sequence, edit } => {
                        Some((sequence, edit))
                    }
                    _ => None,
                })
                .collect::<Vec<_>>()
        };
        let observed = edges(&watcher);
        assert_eq!(observed.len(), 2, "both edits are edges");
        assert_eq!(observed[0].0, 1);
        assert_eq!(observed[1].0, 2, "the sequence advances once per edit");
        assert!(matches!(
            &observed[1].1,
            crate::workspace_model::ModelEdit::CreateChannel { id, .. } if *id == channel
        ));
        assert!(
            edges(&editor).is_empty(),
            "an editing client already holds the model its own edit returned"
        );
        assert!(
            edges(&unsubscribed).is_empty(),
            "a client that never asked for model edges must not receive one"
        );
    }
}
