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

pub(crate) fn serve(stream: UnixStream, client_id: ClientId, host: SharedHost) {
    let delivery = host
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .register_client(client_id);
    let writer_stream = match stream.try_clone() {
        Ok(stream) => stream,
        Err(_) => {
            disconnect_client(&host, client_id);
            return;
        }
    };
    if writer_stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .is_err()
    {
        disconnect_client(&host, client_id);
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
            disconnect_client(&host, client_id);
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
        let outcome = process_request(&host, client_id, envelope.request);
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

    disconnect_client(&host, client_id);
    delivery.close_after_drain();
    let _ = writer.join();
}

fn control_gate(host: &SharedHost) -> Arc<RwLock<()>> {
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

fn disconnect_client(host: &SharedHost, client_id: ClientId) {
    let gate = control_gate(host);
    let _control = gate.read().unwrap_or_else(|p| p.into_inner());
    let handles = {
        let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
        if state.shutting_down {
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

type RequestOutcome = (ServerResponse, Vec<ServerMessage>, bool);

pub(super) fn process_request(
    host: &SharedHost,
    client_id: ClientId,
    request: ClientRequest,
) -> Result<RequestOutcome, ProtocolError> {
    let gate = control_gate(host);
    // Inventory decisions and commit must wait for all previously admitted
    // effects, including provider closes that finish outside HostState.
    let exclusive = matches!(
        &request,
        ClientRequest::PrepareUpgrade { .. }
            | ClientRequest::Shutdown
            | ClientRequest::ShutdownV1 { .. }
            | ClientRequest::ChatReturnTerminal { .. }
    );
    let _exclusive = exclusive.then(|| gate.write().unwrap_or_else(|p| p.into_inner()));
    let _shared = (!exclusive).then(|| gate.read().unwrap_or_else(|p| p.into_inner()));
    ensure_control_open(host)?;
    let notification_watch = matches!(&request, ClientRequest::WatchNotifications { .. });
    match request {
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
        ClientRequest::WatchNotifications { presence }
        | ClientRequest::NotificationPresence { presence } => {
            crate::instance::validate_window_slot(&presence.window).map_err(|error| {
                ProtocolError::new(ProtocolErrorCode::InvalidRequest, error.to_string(), false)
            })?;
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
                        .map(|session| ServerMessage::NotificationActivated { session })
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
        ClientRequest::NotificationPreferencesChanged { preferences, test } => {
            if let Some(notifications) =
                &host.lock().unwrap_or_else(|p| p.into_inner()).notifications
            {
                notifications.reload(preferences, test);
            }
            Ok((ServerResponse::Accepted, Vec::new(), false))
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
        ClientRequest::ResolveUpgradeChat { source } => {
            let session = super::chat::resolve_upgrade(host, &source)?;
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
        | ClientRequest::ChatList) => Ok((
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
            let model = host
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .watch_model(client_id)?;
            Ok((ServerResponse::WorkspaceModel { model }, Vec::new(), false))
        }
        ClientRequest::ReadWorkspaceModel => {
            let model = host
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .read_model()?;
            Ok((ServerResponse::WorkspaceModel { model }, Vec::new(), false))
        }
        ClientRequest::EditWorkspaceModel { edit } => {
            let model = host
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .edit_model(client_id, edit)?;
            Ok((ServerResponse::WorkspaceModel { model }, Vec::new(), false))
        }
        ClientRequest::Spawn { session, options } => {
            let terminal = spawn_terminal(host, client_id, session, options)?;
            let snapshot = {
                let mut state = host.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                let snapshot = state.snapshot(client_id, terminal.session.id, 0)?;
                // Every other observer learns that this terminal exists here,
                // and only here: a spawn is the one inventory edge that no
                // redraw, event or snapshot broadcast implies.
                state.announce_created(client_id, terminal.session.id);
                snapshot
            };
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
            let (terminal, events, snapshot) = {
                let mut state = host.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                let terminal = state.attach(client_id, terminal_id)?;
                let events = state.replay_events(terminal_id, after_event_sequence)?;
                let snapshot = state.snapshot(client_id, terminal_id, after_generation)?;
                (terminal, events, snapshot)
            };
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
            let handle = host
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .focus_handle(client_id, terminal_id)?;
            if let Some(handle) = handle {
                match handle.focus(focused) {
                    Ok(()) | Err(crate::terminal::TerminalSendError::Closed) => {}
                    Err(error) => return Err(send_error(error)),
                }
            }
            Ok((ServerResponse::Accepted, Vec::new(), false))
        }
        ClientRequest::SetTheme { terminal_id, theme } => {
            let handle = host
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .surface_handle(client_id, terminal_id)?;
            handle.set_theme(theme).map_err(send_error)?;
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
            let handle = host
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .surface_handle(client_id, terminal_id)?;
            match scroll {
                TerminalScroll::Lines(delta) => {
                    let delta = isize::try_from(delta).map_err(|_| {
                        ProtocolError::new(
                            ProtocolErrorCode::InvalidRequest,
                            "terminal scroll delta is outside supported bounds",
                            false,
                        )
                    })?;
                    handle.scroll_lines(delta)
                }
                TerminalScroll::Top => handle.scroll_to_top(),
                TerminalScroll::Bottom => handle.scroll_to_bottom(),
                TerminalScroll::Row(row) => {
                    let row = usize::try_from(row).map_err(|_| {
                        ProtocolError::new(
                            ProtocolErrorCode::InvalidRequest,
                            "terminal scroll row is outside supported bounds",
                            false,
                        )
                    })?;
                    handle.scroll_to_row(row)
                }
            }
            .map_err(send_error)?;
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
            let handle = host
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .surface_handle(client_id, terminal_id)?;
            handle.resize(size.into()).map_err(send_error)?;
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
            let snapshot = host
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .snapshot(client_id, terminal_id, after_generation)?;
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
            handle.acknowledge_redraw(generation);
            Ok((ServerResponse::Accepted, Vec::new(), false))
        }
        ClientRequest::ShutdownTerminal { terminal_id } => {
            let handle = host
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .handle(client_id, terminal_id)?;
            handle.shutdown().map_err(send_error)?;
            host.lock()
                .unwrap_or_else(|p| p.into_inner())
                .close_terminal_attention(terminal_id, Some(&handle));
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
            let (handle, paired) = {
                let state = host.lock().unwrap_or_else(|p| p.into_inner());
                let handle = match state.terminals.get(&session.id) {
                    None => None,
                    Some(record)
                        if record.session.launch == session.launch
                            && record.handle.is_completed() =>
                    {
                        Some(record.handle.clone())
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
                        handle.as_ref().is_some_and(|actual| actual.same_worker(expected))
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
                (handle, paired)
            };
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
            if let Some(paired) = paired {
                super::chat::close_terminal_actor(host, session.id, &paired)?;
            }
            if let Some(handle) = &handle {
                handle.shutdown().map_err(send_error)?;
            }
            host.lock()
                .unwrap_or_else(|p| p.into_inner())
                .close_terminal_attention(session.id, handle.as_ref());
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
            let handle = host
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .handle(client_id, terminal_id)?;
            let outcome = handle.close_checked(allow_active);
            if outcome == crate::terminal_ipc::TerminalCloseResult::Closed {
                host.lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .close_terminal_attention(terminal_id, Some(&handle));
                super::chat::close_for_terminal(host, terminal_id)?;
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
    ensure_control_open(host)?;
    let mut resumed = Vec::new();
    let mut replay = Vec::new();
    for cursor in cursors {
        let prepared = {
            let mut state = host.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            let Ok(terminal) = state.attach(client_id, cursor.terminal_id) else {
                continue;
            };
            let events = state
                .replay_events(cursor.terminal_id, cursor.event_sequence)
                .unwrap_or_default();
            let snapshot = state
                .snapshot(client_id, cursor.terminal_id, cursor.generation)
                .unwrap_or_default();
            let messages = replay_messages(
                cursor.terminal_id,
                events,
                snapshot,
                terminal.generation,
                terminal.event_sequence,
            );
            (terminal, messages)
        };
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
    use std::sync::Mutex;

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
        let host = Arc::new(Mutex::new(super::super::state::HostState::new(proof)));
        let mut recipe = LaunchRecipe::shell("/".into()).unwrap();
        recipe.program = "/bin/cat".into();
        recipe.arguments.clear();
        let mut session = TerminalSession {
            id: TerminalId::new(),
            launch: recipe.clone(),
            owned_codex_listener: None,
            recovery_origin: None,
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
        assert!(
            process_request(
                &host,
                1,
                ClientRequest::ReleaseCompletedTerminal {
                    session,
                    allow_active: false
                }
            )
            .is_ok()
        );
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
        let host = Arc::new(Mutex::new(super::super::state::HostState::new(
            proof.clone(),
        )));
        let mut recipe = crate::model::LaunchRecipe::shell("/".into()).unwrap();
        recipe.program = "/bin/true".into();
        recipe.arguments.clear();
        let session = TerminalSession {
            id: TerminalId::new(),
            launch: recipe.clone(),
            owned_codex_listener: None,
            recovery_origin: None,
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
            process_request(&host, 1, request(true)).unwrap().0,
            ServerResponse::TerminalCloseChecked {
                outcome: crate::terminal_ipc::TerminalCloseResult::Closed,
                ..
            }
        ));
        assert!(actor.is_completed());
        assert!(!host.lock().unwrap().chats.contains_key(&session.id));
        assert!(
            host.lock()
                .unwrap()
                .notification_closed
                .contains(&session.id)
        );
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
        let host = Arc::new(Mutex::new(super::super::state::HostState::new(
            proof.clone(),
        )));
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
        let host = Arc::new(Mutex::new(super::super::state::HostState::new(
            proof.clone(),
        )));
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
        let host = Arc::new(std::sync::Mutex::new(super::super::state::HostState::new(
            proof,
        )));
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
        let host = Arc::new(Mutex::new(super::super::state::HostState::new(
            HostSessionProof {
                host_session_id: HostSessionId::new("announce-session"),
                boot_id: BootId::new("boot"),
                host_incarnation: HostIncarnation::new("incarnation"),
            },
        )));
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

    /// A shared-model edit reaches every window watching the model and never
    /// its originator, which already has the model the edit returned. A client
    /// that only reads - every CLI invocation - receives nothing at all.
    #[test]
    fn editing_the_model_announces_to_watchers_but_never_to_its_originator() {
        use crate::model::{BootId, HostIncarnation, HostSessionId};

        let directory = tempfile::tempdir().unwrap();
        let host = Arc::new(Mutex::new(super::super::state::HostState::new(
            HostSessionProof {
                host_session_id: HostSessionId::new("model-session"),
                boot_id: BootId::new("boot"),
                host_incarnation: HostIncarnation::new("incarnation"),
            },
        )));
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
