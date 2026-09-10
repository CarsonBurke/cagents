//! Native chat connection to the same authenticated daemon as hosted terminals.
use super::{Command, Session, Snapshot};
use crate::{
    model::{HostSessionProof, TerminalId},
    terminal_ipc::{
        self as ipc, ClientEnvelope, ClientRequest, FrameCodec, PROTOCOL_VERSION, ServerEnvelope,
        ServerMessage, ServerResponse,
    },
};
use std::{
    collections::BTreeMap,
    io,
    net::Shutdown,
    os::unix::net::UnixStream,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

#[derive(Debug, Clone)]
pub enum Wake {
    Changed,
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
type Pending = Arc<Mutex<BTreeMap<u64, crossbeam_channel::Sender<Result<(), String>>>>>;
struct Inner {
    id: TerminalId,
    session: Session,
    proof: HostSessionProof,
    wire: Arc<Mutex<Wire>>,
    cancel: UnixStream,
    snapshot: Arc<Mutex<Arc<Snapshot>>>,
    next: Arc<AtomicU64>,
    alive: Arc<AtomicBool>,
    visible: Arc<AtomicBool>,
    wake: Arc<dyn Fn(Wake) + Send + Sync>,
    reader: Mutex<Option<JoinHandle<()>>>,
    pending: Pending,
    visibility_tx: crossbeam_channel::Sender<()>,
    visibility_worker: Mutex<Option<JoinHandle<()>>>,
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
    let envelope: ServerEnvelope = codec
        .read(stream)
        .map_err(|e| format!("Cannot decode matched daemon protocol: {e}"))?
        .ok_or("Daemon disconnected")?;
    if envelope.version != PROTOCOL_VERSION {
        return Err("Daemon version mismatch; use matched cagents executables".into());
    }
    if proof.is_some_and(|proof| proof != &envelope.proof) {
        return Err("Daemon proof changed".into());
    }
    if envelope.request_id != Some(request_id) {
        return Err("Unexpected daemon response ordering".into());
    }
    match envelope.message {
        ServerMessage::Response { response } => Ok(response),
        ServerMessage::Error(error) => Err(error.message),
        _ => Err("Unexpected daemon handshake frame".into()),
    }
}
impl ClientHandle {
    pub fn connect(
        session: Session,
        spawn: bool,
        source: Option<HostSessionProof>,
        terminate_source: bool,
        wake: impl Fn(Wake) + Send + Sync + 'static,
    ) -> Result<Self, String> {
        let wake: Arc<dyn Fn(Wake) + Send + Sync> = Arc::new(wake);
        if !spawn && session.proof.is_none() {
            return Err(
                "Exact chat attachment requires the saved daemon proof; explicitly recover instead"
                    .into(),
            );
        }
        let socket = if spawn {
            crate::terminal::ensure_daemon_ready().map_err(|e| e.to_string())?
        } else {
            ipc::daemon_socket_path().map_err(|e| e.to_string())?
        };
        let mut stream = UnixStream::connect(socket)
            .map_err(|e| format!("Cannot connect to the proven chat daemon: {e}"))?;
        stream
            .set_read_timeout(Some(Duration::from_secs(20)))
            .map_err(|e| e.to_string())?;
        stream
            .set_write_timeout(Some(Duration::from_secs(5)))
            .map_err(|e| e.to_string())?;
        let mut codec = FrameCodec::new(ipc::MAX_FRAME_BYTES);
        let expected = if spawn {
            source.clone()
        } else {
            session.proof.clone()
        };
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
        let id = session.id;
        let request = if spawn {
            ClientRequest::ChatStart {
                session,
                source,
                terminate_source,
            }
        } else {
            ClientRequest::ChatAttach { session }
        };
        codec
            .write(
                &mut stream,
                &ClientEnvelope {
                    version: PROTOCOL_VERSION,
                    request_id: 2,
                    proof: Some(proof.clone()),
                    request,
                },
            )
            .map_err(|e| e.to_string())?;
        let (session, initial) = match receive(&mut stream, &mut codec, 2, Some(&proof))? {
            ServerResponse::ChatAttached { session, snapshot } if session.id == id => {
                (session, snapshot)
            }
            _ => return Err("Daemon did not attach the exact native chat".into()),
        };
        stream.set_read_timeout(None).map_err(|e| e.to_string())?;
        let mut reader_stream = stream.try_clone().map_err(|e| e.to_string())?;
        let cancel = stream.try_clone().map_err(|error| error.to_string())?;
        let wire = Arc::new(Mutex::new(Wire {
            stream,
            codec: FrameCodec::new(ipc::MAX_CLIENT_FRAME_BYTES),
        }));
        let snapshot = Arc::new(Mutex::new(Arc::new(initial)));
        let next = Arc::new(AtomicU64::new(3));
        let alive = Arc::new(AtomicBool::new(true));
        let visible = Arc::new(AtomicBool::new(false));
        let pending: Pending = Arc::new(Mutex::new(BTreeMap::new()));
        let (visibility_tx, visibility_rx) = crossbeam_channel::bounded(1);
        let inner = Arc::new(Inner {
            id,
            session,
            proof: proof.clone(),
            wire: wire.clone(),
            cancel,
            snapshot: snapshot.clone(),
            next: next.clone(),
            alive: alive.clone(),
            wake: wake.clone(),
            visible: visible.clone(),
            reader: Mutex::new(None),
            pending: pending.clone(),
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
        let worker = thread::Builder::new().name(format!("chat-client-{id}")).spawn(move || {
            let mut read_pending = false;
            let mut read_again = false;
            let failure = loop {
                let envelope: ServerEnvelope = match codec.read(&mut reader_stream) {
                    Ok(Some(value)) => value,
                    Ok(None) => break "Chat daemon disconnected; exact session retained for reattachment".into(),
                    Err(error) => break format!("Chat daemon frame failed: {error}"),
                };
                if envelope.version != PROTOCOL_VERSION || envelope.proof != proof { break "Chat daemon identity/version changed; refusing replacement".into(); }
                if let ServerMessage::Response { response: ServerResponse::ChatClosed { chat_id,snapshot:final_snapshot } }=envelope.message {
                    if chat_id!=id { break "Daemon closed a different chat".into(); }
                    *snapshot.lock().unwrap_or_else(|p|p.into_inner())=Arc::new(final_snapshot);
                    alive.store(false,Ordering::Release);
                    let _=visibility_stop.try_send(());
                    let _=reader_stream.shutdown(Shutdown::Both);
                    let replies=std::mem::take(&mut *pending.lock().unwrap_or_else(|p|p.into_inner()));
                    for (request,reply) in replies {
                        let _=reply.try_send(if Some(request)==envelope.request_id { Ok(()) } else { Err("Chat was explicitly closed".into()) });
                    }
                    wake(Wake::Changed);
                    return;
                }
                if let Some(reply) = envelope.request_id.and_then(|id| pending.lock().unwrap_or_else(|p| p.into_inner()).remove(&id)) {
                    let result = match &envelope.message { ServerMessage::Response { response: ServerResponse::Accepted | ServerResponse::Detached } => Ok(()), ServerMessage::Error(error) => Err(error.message.clone()), _ => Err("Unexpected chat command response".into()) };
                    let _ = reply.try_send(result);
                }
                match envelope.message {
                    ServerMessage::ChatChanged { chat_id } if chat_id == id && visible.load(Ordering::Acquire) => {
                        if read_pending { read_again = true; continue; }
                        read_again = true;
                    },
                    ServerMessage::ChatChanged { chat_id } if chat_id == id => {},
                    ServerMessage::Response { response: ServerResponse::ChatSnapshot { chat_id, snapshot: update } } if chat_id == id => {
                        read_pending = false;
                        if let Some(update) = update {
                            *snapshot.lock().unwrap_or_else(|p| p.into_inner()) = Arc::new(update);
                            if visible.load(Ordering::Acquire) { wake(Wake::Changed); }
                        }
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
                    if let Err(error) = transmit(&wire, &proof, next.fetch_add(1, Ordering::Relaxed), ClientRequest::ChatRead { chat_id: id, after_generation: Some(generation) }) { break error; }
                    read_pending = true;
                }
            };
            alive.store(false, Ordering::Release);
            let _=visibility_stop.try_send(());
            for (_, reply) in std::mem::take(&mut *pending.lock().unwrap_or_else(|p| p.into_inner())) {
                let _ = reply.try_send(Err(failure.clone()));
            }
            wake(Wake::Disconnected(failure));
        }).map_err(|e| e.to_string())?;
        *inner.reader.lock().unwrap_or_else(|p| p.into_inner()) = Some(worker);
        Ok(Self { inner })
    }
    pub fn session(&self) -> Session {
        let mut session = self.inner.session.clone();
        session.launch.session_id = self.snapshot().session_id.clone();
        session
    }
    pub fn snapshot(&self) -> Arc<Snapshot> {
        self.inner
            .snapshot
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }
    pub fn proof(&self) -> Option<HostSessionProof> {
        Some(self.inner.proof.clone())
    }
    fn request(&self, request: ClientRequest) -> Result<(), String> {
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
