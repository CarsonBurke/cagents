use std::{
    io::{self, Read},
    net::Shutdown,
    os::unix::net::UnixStream,
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

use crate::{
    model::HostSessionProof,
    terminal_ipc::{
        ClientEnvelope, ClientRequest, MAX_CLIENT_FRAME_BYTES, MAX_INPUT_BYTES,
        MAX_RESUME_TERMINALS, PROTOCOL_VERSION, ProtocolError, ProtocolErrorCode, ResumeCursor,
        ServerEnvelope, ServerMessage, ServerResponse, TerminalScroll, TerminalSnapshot,
        TerminalVisibility, WireTerminalEvent, read_frame_with_limit, validate_proof, write_frame,
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
) -> Result<Option<ClientEnvelope>, crate::terminal_ipc::FrameError> {
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
            let _ = host
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .unregister_client(client_id);
            return;
        }
    };
    if writer_stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .is_err()
    {
        let _ = host
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .unregister_client(client_id);
        return;
    }
    let writer_delivery = Arc::clone(&delivery);
    let writer = match thread::Builder::new()
        .name(format!("cagentsd-writer-{client_id}"))
        .spawn(move || writer_loop(writer_stream, writer_delivery))
    {
        Ok(writer) => writer,
        Err(_) => {
            delivery.abort();
            let _ = host
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .unregister_client(client_id);
            return;
        }
    };

    let mut reader = stream;
    let handshake_deadline = Instant::now() + HANDSHAKE_TIMEOUT;
    let mut connected = false;
    loop {
        let frame = if connected {
            read_frame_with_limit::<ClientEnvelope>(&mut reader, MAX_CLIENT_FRAME_BYTES)
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
            let (resumed, replay) = prepare_resume(&host, client_id, resume);
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
                if push_response(&delivery, &proof, request_id, response).is_err() {
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
                if shutdown {
                    host.lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .request_shutdown();
                    // This connection stays open, so the accept loop would not
                    // otherwise observe the flag until the client disconnects.
                    super::platform::wake();
                }
            }
            Err(error) => {
                if push_error(&delivery, &host, request_id, error).is_err() {
                    break;
                }
            }
        }
    }

    let handles = host
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .unregister_client(client_id);
    // A GUI may disconnect after receiving an edge but before acknowledging
    // it. Clear the process-global edge so a later attaching GUI is not starved
    // of future redraw notifications; immutable snapshots preserve the state.
    for handle in handles {
        handle.acknowledge_redraw(handle.generation());
    }
    delivery.close_after_drain();
    let _ = writer.join();
}

type RequestOutcome = (ServerResponse, Vec<ServerMessage>, bool);

fn process_request(
    host: &SharedHost,
    client_id: ClientId,
    request: ClientRequest,
) -> Result<RequestOutcome, ProtocolError> {
    match request {
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
        ClientRequest::Spawn { session, options } => {
            let terminal = spawn_terminal(host, client_id, session, options)?;
            let snapshot = host
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .snapshot(client_id, terminal.session.id, 0)?;
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
                .handle(client_id, terminal_id)?;
            handle.input(bytes).map_err(send_error)?;
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
                .handle(client_id, terminal_id)?;
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
                .handle(client_id, terminal_id)?;
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
                .handle(client_id, terminal_id)?;
            handle.focus(focused).map_err(send_error)?;
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
                .handle(client_id, terminal_id)?;
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
                .handle(client_id, terminal_id)?;
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
        ClientRequest::AcknowledgeRedraw {
            terminal_id,
            generation,
        } => {
            let handle = host
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .handle(client_id, terminal_id)?;
            if generation > handle.generation() {
                return Err(ProtocolError::new(
                    ProtocolErrorCode::InvalidRequest,
                    "cannot acknowledge a terminal generation that was not published",
                    false,
                ));
            }
            handle.acknowledge_redraw(generation);
            Ok((ServerResponse::Accepted, Vec::new(), false))
        }
        ClientRequest::ShutdownTerminal { terminal_id } => {
            let handle = host
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .handle(client_id, terminal_id)?;
            handle.shutdown().map_err(send_error)?;
            Ok((ServerResponse::Accepted, Vec::new(), false))
        }
        ClientRequest::Shutdown => Ok((ServerResponse::ShutdownAccepted, Vec::new(), true)),
    }
}

fn prepare_resume(
    host: &SharedHost,
    client_id: ClientId,
    cursors: Vec<ResumeCursor>,
) -> (
    Vec<crate::terminal_ipc::TerminalDescriptor>,
    Vec<ServerMessage>,
) {
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
    (resumed, replay)
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
    while let Some(message) = delivery.pop() {
        if write_frame(&mut stream, &message).is_err() {
            delivery.abort();
            break;
        }
    }
    let _ = stream.shutdown(Shutdown::Both);
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
