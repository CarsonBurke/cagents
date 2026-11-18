//! Codex's existing shared daemon speaks WebSocket text messages over its Unix socket.
use super::*;
use crate::agent::observe::socket::connect;
#[cfg(test)]
use std::os::unix::fs::MetadataExt;
use std::{io, path::Path};
use tungstenite::{
    Error as WsError, HandshakeError, Message,
    client::client_with_config,
    protocol::{
        WebSocketConfig,
        frame::{FrameHeader, coding::OpCode},
    },
};

// Limit work per actor iteration even if a peer continuously supplies fragments.
struct Stream {
    socket: UnixStream,
    budget: usize,
    progress: ReadProgress,
}
impl Read for Stream {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        let limit = bytes.len().min(self.budget);
        if limit == 0 {
            return Err(io::ErrorKind::WouldBlock.into());
        }
        let count = self.socket.read(&mut bytes[..limit])?;
        self.budget -= count;
        self.progress.receive(&bytes[..count])?;
        Ok(count)
    }
}
impl Write for Stream {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.socket.write(bytes)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.socket.flush()
    }
}

// Tungstenite may read ahead into a partial next frame. Track only boundaries
// using its header parser (never payloads), so a complete message or interleaved
// ping cannot hide a stalled fragmented message inside the library's buffer.
#[derive(Default)]
struct ReadProgress {
    http_end: u32,
    upgraded: bool,
    header: [u8; 14],
    header_len: usize,
    remaining: u64,
    final_data: bool,
    frame_started: Option<Instant>,
    message_started: Option<Instant>,
}
impl ReadProgress {
    fn receive(&mut self, mut bytes: &[u8]) -> io::Result<()> {
        while !bytes.is_empty() {
            if !self.upgraded {
                self.http_end = (self.http_end << 8) | u32::from(bytes[0]);
                self.upgraded = self.http_end == 0x0d0a0d0a;
                bytes = &bytes[1..];
                continue;
            }
            if self.remaining != 0 {
                let count = self.remaining.min(bytes.len() as u64) as usize;
                self.remaining -= count as u64;
                bytes = &bytes[count..];
                if self.remaining == 0 {
                    self.complete_frame();
                }
                continue;
            }
            self.frame_started.get_or_insert_with(Instant::now);
            if self.header_len == self.header.len() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Oversized WebSocket header",
                ));
            }
            self.header[self.header_len] = bytes[0];
            self.header_len += 1;
            bytes = &bytes[1..];
            let mut cursor = io::Cursor::new(&self.header[..self.header_len]);
            if let Some((header, length)) = FrameHeader::parse(&mut cursor)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?
            {
                if length > FRAME_LIMIT as u64 {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "WebSocket frame exceeds 32 MiB",
                    ));
                }
                let data = matches!(header.opcode, OpCode::Data(_));
                if data && self.message_started.is_none() {
                    self.message_started = self.frame_started;
                }
                self.final_data = data && header.is_final;
                self.header_len = 0;
                self.remaining = length;
                if length == 0 {
                    self.complete_frame();
                }
            }
        }
        Ok(())
    }
    fn complete_frame(&mut self) {
        self.frame_started = None;
        if self.final_data {
            self.message_started = None;
        }
    }
    fn partial_started(&self) -> Option<Instant> {
        self.message_started.or(self.frame_started)
    }
}

fn would_block(error: &WsError) -> bool {
    matches!(error, WsError::Io(error) if error.kind() == io::ErrorKind::WouldBlock)
}

fn poll(
    socket: &UnixStream,
    wake: &mut UnixStream,
    write: bool,
    timeout: i32,
) -> Result<(), String> {
    let mut fds = [
        libc::pollfd {
            fd: socket.as_raw_fd(),
            events: if write { libc::POLLOUT } else { libc::POLLIN },
            revents: 0,
        },
        libc::pollfd {
            fd: wake.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        },
    ];
    if unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as _, timeout) } < 0 {
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error.to_string());
        }
    }
    if fds[1].revents != 0 {
        let mut bytes = [0; 256];
        while wake.read(&mut bytes).is_ok_and(|n| n != 0) {}
    }
    Ok(())
}

/// How long a launched server may spend bringing itself up before it accepts
/// at its own endpoint. Codex 0.169 binds about 0.1 s after exec with an empty
/// home and about 1 s against multi-gigabyte production state, so this leaves
/// more than ten times the slow case for a busy machine.
const LISTENER_TIMEOUT: Duration = Duration::from_secs(15);

/// How long a launched server may additionally wait blocked on a file lock
/// another process holds. Codex serializes every app-server startup on one
/// `CODEX_HOME` behind a blocking lock it keeps for about a second each with
/// production state, so restarting many conversations after an update, or
/// any other Codex starting at the same time, queues later servers through no
/// fault of their own. A minute admits dozens of such startups ahead of this
/// one, yet still ends a wait on a holder that never lets go.
const LOCK_QUEUE_LIMIT: Duration = Duration::from_secs(60);

/// How often the launched server's lock waits are sampled. Each sample reads
/// `/proc/locks`, and a coarser estimate only shifts either limit by this much.
const LOCK_SAMPLE: Duration = Duration::from_millis(250);

/// The bounds [`join_launched`] holds a launched server to.
#[derive(Clone, Copy)]
struct ListenerLimits {
    own: Duration,
    queued: Duration,
    sample: Duration,
}

const LISTENER_LIMITS: ListenerLimits = ListenerLimits {
    own: LISTENER_TIMEOUT,
    queued: LOCK_QUEUE_LIMIT,
    sample: LOCK_SAMPLE,
};

/// A process of the launched server blocked waiting for a file lock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LockWait {
    /// The process holding the lock, when the kernel reports one.
    holder: Option<i32>,
}

/// Find a process of `group` that `locks`, in the format of `/proc/locks`,
/// shows blocked behind another lock. Each blocked request follows the lock
/// blocking it, marked `->`.
fn lock_wait_in(locks: &str, mut in_group: impl FnMut(i32) -> bool) -> Option<LockWait> {
    let mut holder = None;
    for line in locks.lines() {
        let mut fields = line.split_whitespace().skip(1).peekable();
        let blocked = fields.next_if_eq(&"->").is_some();
        let pid = fields.nth(3).and_then(|pid| pid.parse::<i32>().ok());
        if !blocked {
            holder = pid.filter(|pid| *pid > 0);
        } else if pid.is_some_and(&mut in_group) {
            return Some(LockWait { holder });
        }
    }
    None
}

/// Whether a process of the launched server whose process group is `group`
/// is blocked on a file lock another process holds.
fn launched_lock_wait(group: i32) -> Option<LockWait> {
    let locks = std::fs::read_to_string("/proc/locks").ok()?;
    lock_wait_in(&locks, |pid| {
        std::fs::read(format!("/proc/{pid}/stat"))
            .ok()
            .and_then(|stat| crate::model::agent_recovery::process_identity(&stat))
            .is_some_and(|(actual, _)| actual == group)
    })
}

/// Join the endpoint of a server this conversation launched, once it accepts.
///
/// The socket exists from the moment the server binds it, which is before it
/// listens, so a refusal there is a launch still in progress rather than a
/// failure. `halted` reports why waiting any longer is pointless.
///
/// Time the server spends blocked behind another process's file lock is
/// queueing rather than its own startup: `lock_wait` reports it, and it is
/// charged to `limits.queued` instead of `limits.own` and published in
/// `queued` for callers that bound the whole startup.
fn join_launched(
    path: &Path,
    limits: ListenerLimits,
    queued: &AtomicU64,
    mut lock_wait: impl FnMut() -> Option<LockWait>,
    mut halted: impl FnMut() -> Option<&'static str>,
) -> Result<UnixStream, String> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    let started = Instant::now();
    let mut sampled = started;
    let mut waited = Duration::ZERO;
    loop {
        if let Ok(metadata) = std::fs::metadata(path)
            && metadata.file_type().is_socket()
        {
            match connect(path, metadata.dev(), metadata.ino()) {
                Ok(socket) => return Ok(socket),
                Err(error) if error.not_listening => {}
                Err(error) => return Err(error.into()),
            }
        }
        if let Some(reason) = halted() {
            return Err(reason.into());
        }
        let now = Instant::now();
        if now.duration_since(sampled) >= limits.sample {
            if let Some(wait) = lock_wait() {
                waited += now.duration_since(sampled);
                queued.store(
                    u64::try_from(waited.as_millis()).unwrap_or(u64::MAX),
                    Ordering::Release,
                );
                if waited >= limits.queued {
                    return Err(format!(
                        "Codex server waited more than {} seconds for {} to release a lock it needs to start",
                        limits.queued.as_secs(),
                        wait.holder.map_or_else(
                            || "another process".to_owned(),
                            |pid| format!("process {pid}")
                        ),
                    ));
                }
            }
            sampled = now;
        }
        if now.duration_since(started).saturating_sub(waited) >= limits.own {
            return Err(format!(
                "Codex server did not open its local socket within {} seconds",
                limits.own.as_secs()
            ));
        }
        thread::sleep(Duration::from_millis(20));
    }
}

fn startup_halt_reason(child: Option<&ChildGuard>, cancelled: bool) -> Option<&'static str> {
    if cancelled {
        return Some("Provider startup cancelled");
    }
    match child.map(ChildGuard::observed_exit_status) {
        Some(Ok(Some(_))) => Some("Codex server exited before opening its local socket"),
        Some(Err(_)) => Some("Cannot verify Codex server process identity during startup"),
        Some(Ok(None)) | None => None,
    }
}

/// Whether the server a native holder launched is gone or a zombie its holder
/// has not reaped yet.
fn held_server_exited(server: &crate::daemon::holder_registry::Process) -> bool {
    let Ok(stat) = std::fs::read(format!("/proc/{}/stat", server.pid)) else {
        return true;
    };
    let zombie = std::str::from_utf8(&stat)
        .ok()
        .and_then(|stat| stat.rsplit_once(')'))
        .and_then(|(_, fields)| fields.split_whitespace().next())
        .is_some_and(|state| state == "Z");
    zombie
        || crate::model::agent_recovery::process_identity(&stat)
            .is_none_or(|(_, start)| start != server.start)
}

impl Actor {
    /// Serve the listener this conversation's own server is bringing up.
    pub(super) fn run_launched_websocket(
        &mut self,
        initial: Step,
        path: &Path,
    ) -> Result<(), String> {
        // The provider leads its own process group whether this actor or a
        // native holder launched it.
        let (group, held) = match (&self.child, &self.native_holder) {
            (Some(child), _) => (Some(child.child.id() as i32), None),
            (None, Some(holder)) => (Some(holder.server().pid), Some(holder.server().clone())),
            (None, None) => (None, None),
        };
        let socket = join_launched(
            path,
            LISTENER_LIMITS,
            &self.startup_lock_wait,
            || group.and_then(launched_lock_wait),
            || {
                startup_halt_reason(self.child.as_ref(), self.stop.load(Ordering::Acquire)).or_else(
                    || {
                        held.as_ref()
                            .is_some_and(held_server_exited)
                            .then_some("Codex server exited before opening its local socket")
                    },
                )
            },
        )?;
        self.serve_websocket(initial, socket)
    }

    pub(super) fn run_websocket(
        &mut self,
        initial: Step,
        path: &Path,
        device: u64,
        inode: u64,
    ) -> Result<(), String> {
        let socket = connect(path, device, inode)?;
        self.serve_websocket(initial, socket)
    }

    fn serve_websocket(&mut self, initial: Step, socket: UnixStream) -> Result<(), String> {
        if self.native_holder.is_some() {
            let mut credentials: libc::ucred = unsafe { std::mem::zeroed() };
            let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
            if unsafe {
                libc::getsockopt(
                    socket.as_raw_fd(),
                    libc::SOL_SOCKET,
                    libc::SO_PEERCRED,
                    (&mut credentials as *mut libc::ucred).cast(),
                    &mut length,
                )
            } != 0
                || length as usize != std::mem::size_of::<libc::ucred>()
            {
                return Err("Cannot verify owned app-server peer".into());
            }
            let peer = crate::daemon::holder_registry::Process::read(credentials.pid)
                .map_err(|error| error.to_string())?;
            if self
                .expected_native_peer
                .as_ref()
                .is_some_and(|expected| expected != &peer)
            {
                return Err(
                    "Retained Codex endpoint belongs to a different app-server process".into(),
                );
            }
            self.native_peer = Some(peer);
        }
        let config = WebSocketConfig::default()
            .read_buffer_size(8192)
            .write_buffer_size(0)
            .max_write_buffer_size(FRAME_LIMIT + 1024)
            .max_message_size(Some(FRAME_LIMIT))
            .max_frame_size(Some(FRAME_LIMIT));
        let stream = Stream {
            socket,
            budget: 64 * 1024,
            progress: ReadProgress::default(),
        };
        let mut handshake = client_with_config("ws://localhost/", stream, Some(config));
        let mut socket = loop {
            match handshake {
                Ok((socket, _)) => break socket,
                Err(HandshakeError::Failure(error)) => {
                    return Err(format!("Codex WebSocket handshake: {error}"));
                }
                Err(HandshakeError::Interrupted(mut pending)) => {
                    if self.stop.load(Ordering::Acquire) {
                        return Ok(());
                    }
                    if self.started.elapsed() > IO_TIMEOUT {
                        return Err("Codex WebSocket handshake timed out after 30 seconds".into());
                    }
                    // A short bounded wait also handles interrupted writes without
                    // polling a writable socket in a busy loop while awaiting HTTP.
                    let stream = pending.get_mut().get_mut();
                    poll(&stream.socket, &mut self.wake, false, 50)?;
                    stream.budget = 64 * 1024;
                    handshake = pending.handshake();
                }
            }
        };
        self.step(initial)?;
        let mut flushing = false;
        let mut closing = None;
        let mut close_sent = false;
        let mut peer_closed = false;
        let mut interrupt_sent = false;
        let mut interrupt_error = String::new();
        loop {
            if self.stop.load(Ordering::Acquire)
                && self
                    .native_holder
                    .as_ref()
                    .is_some_and(|holder| holder.is_durable())
                && (self.preserve_native.load(Ordering::Acquire)
                    || !self.native_installed.load(Ordering::Acquire))
            {
                // A proved owner keeps the server and current turn. Closing
                // this connection must not send an interrupt or flush an
                // uncertain queued prompt/approval during daemon replacement.
                return Ok(());
            }
            if self.stop.load(Ordering::Acquire) && closing.is_none() {
                closing = Some(Instant::now());
            }
            let busy = self.state.lock().unwrap_or_else(|p| p.into_inner()).busy;
            if closing.is_some()
                && !close_sent
                && !peer_closed
                && self.pending.is_empty()
                && !flushing
            {
                if busy && !interrupt_sent {
                    match self.adapter.command(Command::Interrupt) {
                        Ok(step) => {
                            self.step(step)?;
                            interrupt_sent = true;
                        }
                        Err(error) => interrupt_error = error,
                    }
                } else if !busy {
                    match socket.close(None) {
                        Ok(()) => {}
                        Err(error) if would_block(&error) => {}
                        Err(error) => return Err(format!("Codex WebSocket close: {error}")),
                    }
                    close_sent = true;
                    flushing = true;
                    self.write_started.get_or_insert_with(Instant::now);
                }
            }
            if closing.is_some_and(|at: Instant| at.elapsed() > crate::chat::INTERRUPT_TIMEOUT) {
                return Err(format!(
                    "Codex interruption/close was not confirmed within 30 seconds ({interrupt_error}); backend work may still be active. Inspect exact provider session before recovery"
                ));
            }
            if !flushing && !close_sent && !peer_closed {
                self.drain_commands(closing.is_some())?;
                if let Some(bytes) = self.pending.pop_front() {
                    self.queued -= bytes.len();
                    let text = String::from_utf8(bytes).map_err(|error| error.to_string())?;
                    // WouldBlock means tungstenite has accepted and buffered this
                    // frame. Never send it again; flush before accepting another.
                    match socket.write(Message::Text(text.into())) {
                        Ok(()) => {}
                        Err(error) if would_block(&error) => {}
                        Err(error) => return Err(format!("Codex WebSocket write: {error}")),
                    }
                    flushing = true;
                }
            }
            if flushing {
                match socket.flush() {
                    Ok(()) => {
                        flushing = false;
                        self.write_started = if self.pending.is_empty() {
                            None
                        } else {
                            Some(Instant::now())
                        };
                    }
                    Err(WsError::ConnectionClosed) => return self.websocket_closed(busy),
                    Err(error) if would_block(&error) => {}
                    Err(error) => return Err(format!("Codex WebSocket flush: {error}")),
                }
            }
            let stream = socket.get_mut();
            stream.budget = 64 * 1024;
            let mut read_again = false;
            // Keep adapter ordering: all requests from one Step are sent before
            // processing the next application message. Control replies are
            // driven by tungstenite and share the bounded flush deadline.
            if self.pending.is_empty() && !flushing {
                match socket.read() {
                    Ok(Message::Text(text)) => {
                        self.first_frame = true;
                        let frame = serde_json::from_str(&text).map_err(|error| {
                            format!("Codex WebSocket text is not JSON: {error}")
                        })?;
                        let mut step = self.adapter.receive(frame)?;
                        if close_sent || peer_closed {
                            // An in-flight reply may request another catalog
                            // or history page. After Close no application
                            // writes can be sent: queuing one would also block
                            // reading the close acknowledgement. Keep its
                            // final events, but admit no further requests.
                            step.writes.clear();
                        }
                        self.provider_step(step)?;
                        read_again = true;
                    }
                    Ok(Message::Ping(_)) => {
                        flushing = true;
                        self.write_started.get_or_insert_with(Instant::now);
                        read_again = true;
                    }
                    Ok(Message::Pong(_)) => {
                        read_again = true;
                    }
                    Ok(Message::Close(_)) => {
                        peer_closed = true;
                        closing.get_or_insert_with(Instant::now);
                        flushing = true;
                        self.write_started.get_or_insert_with(Instant::now);
                        read_again = true;
                    }
                    Ok(_) => {
                        return Err("Codex WebSocket sent a non-text application message".into());
                    }
                    Err(WsError::ConnectionClosed) => return self.websocket_closed(busy),
                    Err(WsError::Protocol(
                        tungstenite::error::ProtocolError::ResetWithoutClosingHandshake,
                    )) if close_sent && !busy => {
                        // Codex's shared server drops the socket after our
                        // close frame instead of echoing it. Work was already
                        // confirmed idle and our queue flushed; EOF completes
                        // detachment, not an unconfirmed interruption.
                        return self.websocket_closed(busy);
                    }
                    Err(error) if would_block(&error) => {
                        read_again = socket.get_ref().budget == 0;
                    }
                    Err(error) => return Err(format!("Codex WebSocket read: {error}")),
                }
            }
            let partial_started = socket.get_ref().progress.partial_started();
            if partial_started.is_some_and(|at| at.elapsed() > IO_TIMEOUT) {
                return Err("Codex WebSocket message remained incomplete for 30 seconds".into());
            }
            if !self.first_frame && self.started.elapsed() > IO_TIMEOUT {
                return Err(
                    "Provider initialization timed out without a JSON protocol frame".into(),
                );
            }
            if self
                .write_started
                .is_some_and(|at| at.elapsed() > IO_TIMEOUT)
            {
                return Err("Codex WebSocket write stalled for 30 seconds".into());
            }
            if self.dirty
                && self.visible.load(Ordering::Acquire)
                && self.last_present.elapsed() >= PRESENT_INTERVAL
            {
                self.dirty = false;
                self.last_present = Instant::now();
                (self.notify)(super::Update::Presentation);
            }
            let timeout = if read_again
                || (!flushing && (!self.pending.is_empty() || !self.incoming.is_empty()))
            {
                0
            } else if self.dirty && self.visible.load(Ordering::Acquire) {
                50
            } else if !self.first_frame
                || partial_started.is_some()
                || self.write_started.is_some()
                || closing.is_some()
            {
                250
            } else {
                -1
            };
            poll(&socket.get_ref().socket, &mut self.wake, flushing, timeout)?;
        }
    }

    fn websocket_closed(&self, busy: bool) -> Result<(), String> {
        if !self.first_frame || busy || !self.pending.is_empty() {
            Err("Codex shared connection closed before pending work was confirmed; inspect the exact thread before recovery".into())
        } else {
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;

    /// Join without lock waits, under the production limits.
    fn join(
        path: &Path,
        halted: impl FnMut() -> Option<&'static str>,
    ) -> Result<UnixStream, String> {
        join_launched(path, LISTENER_LIMITS, &AtomicU64::new(0), || None, halted)
    }

    #[test]
    fn in_flight_response_after_close_keeps_final_events_without_queuing_new_requests() {
        use serde_json::{Value, json};

        struct LateResponse {
            ready: crossbeam_channel::Sender<()>,
        }
        impl Adapter for LateResponse {
            fn initialize(&mut self) -> Result<Step, String> {
                Ok(Step {
                    writes: vec![json!({"method":"initialize"})],
                    ..Step::default()
                })
            }

            fn command(&mut self, _: Command) -> Result<Step, String> {
                Err("fixture does not accept provider commands".into())
            }

            fn receive(&mut self, frame: Value) -> Result<Step, String> {
                if frame["ready"] == true {
                    self.ready.send(()).unwrap();
                    return Ok(Step {
                        events: vec![Event::Ready(true)],
                        ..Step::default()
                    });
                }
                assert_eq!(frame, json!({"late":true}));
                // Catalog/history replies can finish setup or start a next
                // page even after the transport has sent its Close frame.
                Ok(Step {
                    writes: vec![json!({"method":"follow-up"})],
                    events: vec![Event::Session {
                        id: "retained-thread".into(),
                        title: Some("Final provider title".into()),
                    }],
                    ..Step::default()
                })
            }
        }

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("late-response.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let metadata = std::fs::metadata(&path).unwrap();
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            stream
                .set_write_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut socket = tungstenite::accept(stream).unwrap();
            assert!(matches!(socket.read().unwrap(), Message::Text(_)));
            socket
                .send(Message::Text(json!({"ready":true}).to_string().into()))
                .unwrap();
            assert!(matches!(socket.read().unwrap(), Message::Close(_)));
            // Model an application reply already in flight when the client
            // closes. Write its server frame directly because tungstenite's
            // server API forbids starting Text after observing the Close.
            let response = br#"{"late":true}"#;
            let mut frames = vec![0x81, response.len() as u8];
            frames.extend_from_slice(response);
            frames.extend_from_slice(&[0x88, 0]);
            socket.get_mut().write_all(&frames).unwrap();
        });
        let spec = ProcessSpec {
            execution: None,
            messages: None,
            stdout_noise: None,
            transport: TransportSpec::UnixWebSocket {
                socket: path,
                device: metadata.dev(),
                inode: metadata.ino(),
            },
            program: "/nonexistent/late-response-fixture".into(),
            arguments: Vec::new(),
            working_directory: directory.path().into(),
            environment: BTreeMap::new(),
            unset_environment: Default::default(),
        };
        let (ready, initialized) = crossbeam_channel::bounded(1);
        let mut adapter: Box<dyn Adapter> = Box::new(LateResponse { ready });
        let mut initial = adapter.initialize().unwrap();
        let store = preflight(&spec, &mut initial).unwrap();
        let handle = LocalHandle::start(spec, adapter, initial, store, None, |_| {}).unwrap();
        initialized.recv_timeout(Duration::from_secs(5)).unwrap();
        let (closed, settled) = crossbeam_channel::bounded(1);
        let closing = handle.clone();
        let closer = thread::spawn(move || closed.send(closing.close()).unwrap());
        let timely = settled.recv_timeout(Duration::from_secs(5));
        // Join even on failure: the old pending-write deadlock times out in
        // its own close bound, and must not leak a worker into another test.
        closer.join().unwrap();
        server.join().unwrap();
        timely
            .expect("a late response wedged the WebSocket close handshake")
            .unwrap();
        assert_eq!(handle.failure(), None);
        assert!(!handle.owns_work());
        assert_eq!(
            handle.snapshot(None).unwrap().title.as_deref(),
            Some("Final provider title"),
            "closing must retain semantic events from the in-flight response"
        );
    }

    #[test]
    fn partial_message_deadline_survives_read_ahead_and_interleaved_ping() {
        let mut progress = ReadProgress::default();
        progress
            .receive(b"HTTP/1.1 101 Switching Protocols\r\n\r\n")
            .unwrap();
        // Complete text, fragmented text start, and ping in one socket read.
        progress
            .receive(&[0x81, 1, b'x', 0x01, 1, b'a', 0x89, 1, b'p'])
            .unwrap();
        let started = progress.partial_started().unwrap();
        // An incomplete final continuation must keep the original deadline.
        progress.receive(&[0x80, 2, b'b']).unwrap();
        assert_eq!(progress.partial_started(), Some(started));
        progress.receive(b"c").unwrap();
        assert!(progress.partial_started().is_none());
        // A complete message must not conceal a read-ahead partial next header.
        progress.receive(&[0x81, 1, b'x', 0x81]).unwrap();
        assert!(progress.partial_started().is_some());
        progress.receive(&[0]).unwrap();
        assert!(progress.partial_started().is_none());
    }

    #[test]
    fn replaced_socket_is_not_joined() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("codex.sock");
        let original = UnixListener::bind(&path).unwrap();
        let metadata = std::fs::metadata(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        let replacement = UnixListener::bind(&path).unwrap();
        replacement.set_nonblocking(true).unwrap();
        assert!(connect(&path, metadata.dev(), metadata.ino()).is_err());
        assert_eq!(
            replacement.accept().unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        drop(original);
    }

    /// A socket bound at `path` that accepts nothing until it is told to
    /// listen, which is how a starting server first leaves its endpoint.
    fn bound(path: &Path) -> std::os::fd::OwnedFd {
        use std::os::fd::{FromRawFd, OwnedFd};
        let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
        address.sun_family = libc::AF_UNIX as _;
        let bytes = path.as_os_str().as_encoded_bytes();
        assert!(bytes.len() < address.sun_path.len());
        for (target, byte) in address.sun_path.iter_mut().zip(bytes) {
            *target = *byte as _;
        }
        let socket =
            unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0) };
        assert!(socket >= 0, "{}", io::Error::last_os_error());
        let socket = unsafe { OwnedFd::from_raw_fd(socket) };
        let result = unsafe {
            libc::bind(
                socket.as_raw_fd(),
                (&address as *const libc::sockaddr_un).cast(),
                std::mem::size_of::<libc::sockaddr_un>() as _,
            )
        };
        assert_eq!(result, 0, "{}", io::Error::last_os_error());
        socket
    }

    #[test]
    fn a_launched_server_is_joined_once_it_starts_listening() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("codex.sock");
        let server = bound(&path);
        let metadata = std::fs::metadata(&path).unwrap();
        // Joining an endpoint nobody launched stays a single, final attempt.
        assert!(
            connect(&path, metadata.dev(), metadata.ino())
                .unwrap_err()
                .not_listening
        );
        let mut polls = 0;
        let joined = join(&path, || {
            polls += 1;
            if polls == 3 {
                assert_eq!(unsafe { libc::listen(server.as_raw_fd(), 1) }, 0);
            }
            None
        });
        assert!(joined.is_ok(), "{joined:?}");
        assert_eq!(polls, 3);
    }

    #[test]
    fn launched_server_exit_observation_preserves_original_child_ownership() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("missing.sock");
        let mut provider = ChildGuard::spawn(
            Process::new("/bin/sh")
                .args(["-c", "exit 7"])
                .process_group(0),
        );
        assert_eq!(
            join(&path, || startup_halt_reason(Some(&provider), false)).unwrap_err(),
            "Codex server exited before opening its local socket"
        );
        assert_eq!(
            provider.observed_exit_status().unwrap().unwrap().code(),
            Some(7)
        );
        provider.terminate().unwrap();
        assert_eq!(provider.child.try_wait().unwrap().unwrap().code(), Some(7));

        let mut provider = ChildGuard::spawn(Process::new("/bin/true").process_group(0));
        provider.child.wait().unwrap();
        assert_eq!(
            join(&path, || startup_halt_reason(Some(&provider), false)).unwrap_err(),
            "Cannot verify Codex server process identity during startup"
        );
        assert_eq!(
            provider.terminate().unwrap_err().raw_os_error(),
            Some(libc::ECHILD)
        );
    }

    #[test]
    fn waiting_for_a_launched_server_ends_when_it_cannot_arrive() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("codex.sock");
        assert_eq!(
            join(&path, || Some("Provider startup cancelled")).unwrap_err(),
            "Provider startup cancelled"
        );
        let _server = bound(&path);
        assert_eq!(
            join(&path, || Some(
                "Codex server exited before opening its local socket"
            ))
            .unwrap_err(),
            "Codex server exited before opening its local socket"
        );
    }

    #[test]
    fn lock_waits_are_found_only_for_the_launched_servers_group() {
        let locks = "\
1: FLOCK  ADVISORY  WRITE 400 00:1e:77 0 EOF
1: -> FLOCK  ADVISORY  WRITE 401 00:1e:77 0 EOF
1: -> FLOCK  ADVISORY  WRITE 402 00:1e:77 0 EOF
2: POSIX  ADVISORY  WRITE 500 00:1e:78 0 EOF
3: OFDLCK ADVISORY  WRITE -1 00:1e:79 0 EOF
3: -> OFDLCK ADVISORY  WRITE 403 00:1e:79 0 EOF
";
        // A blocked request names the lock printed above it.
        assert_eq!(
            lock_wait_in(locks, |pid| pid == 402),
            Some(LockWait { holder: Some(400) })
        );
        // An open file description lock has no owning process to name.
        assert_eq!(
            lock_wait_in(locks, |pid| pid == 403),
            Some(LockWait { holder: None })
        );
        // Holding a lock, or waiting outside the group, is not queueing.
        assert_eq!(lock_wait_in(locks, |pid| pid == 400 || pid == 500), None);
        assert_eq!(lock_wait_in("", |_| true), None);
    }

    #[test]
    fn a_launched_server_blocked_on_a_held_lock_is_observed() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("startup.lock");
        let lock = std::fs::File::create(&path).unwrap();
        assert_eq!(unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) }, 0);
        let name = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
        let lock_descriptor = lock.as_raw_fd();
        // As with Codex behind its launcher, a child of the group's leader
        // blocks, so the wait is attributed through the process group. Only
        // async-signal-safe calls run after fork.
        let leader = unsafe { libc::fork() };
        assert!(leader >= 0);
        if leader == 0 {
            unsafe {
                // flock follows the inherited open file description. Neither
                // child may retain the parent's lock while waiting for it.
                libc::close(lock_descriptor);
                libc::setpgid(0, 0);
                let blocked = libc::fork();
                if blocked == 0 {
                    let descriptor = libc::open(name.as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC);
                    libc::_exit(i32::from(
                        descriptor < 0 || libc::flock(descriptor, libc::LOCK_EX) != 0,
                    ));
                }
                let mut status = 0;
                libc::waitpid(blocked, &mut status, 0);
                libc::_exit(i32::from(
                    blocked < 0 || !libc::WIFEXITED(status) || libc::WEXITSTATUS(status) != 0,
                ));
            }
        }
        let deadline = Instant::now() + Duration::from_secs(10);
        let wait = loop {
            if let Some(wait) = launched_lock_wait(leader) {
                break wait;
            }
            assert!(Instant::now() < deadline, "the server never blocked");
            thread::sleep(Duration::from_millis(20));
        };
        assert_eq!(wait.holder, Some(std::process::id() as i32));
        assert_eq!(launched_lock_wait(i32::MAX), None);
        drop(lock);
        let mut status = 0;
        assert_eq!(unsafe { libc::waitpid(leader, &mut status, 0) }, leader);
        assert!(libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0);
        assert_eq!(launched_lock_wait(leader), None);
    }

    #[test]
    fn time_queued_behind_another_lock_holder_is_not_charged_to_startup() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("codex.sock");
        let limits = ListenerLimits {
            own: Duration::from_millis(300),
            queued: Duration::from_secs(5),
            sample: Duration::from_millis(20),
        };
        let queued = AtomicU64::new(0);
        let started = Instant::now();
        let mut server = None;
        let joined = join_launched(
            &path,
            limits,
            &queued,
            || {
                (started.elapsed() < Duration::from_millis(900))
                    .then_some(LockWait { holder: Some(7) })
            },
            || {
                if started.elapsed() >= Duration::from_millis(1000) && server.is_none() {
                    let bound = bound(&path);
                    assert_eq!(unsafe { libc::listen(bound.as_raw_fd(), 1) }, 0);
                    server = Some(bound);
                }
                None
            },
        );
        assert!(joined.is_ok(), "{joined:?}");
        // Three times the startup budget passed, nearly all of it queued.
        assert!(started.elapsed() >= Duration::from_millis(1000));
        assert!(
            Duration::from_millis(queued.load(Ordering::Acquire)) >= Duration::from_millis(700)
        );

        // Without the queue the same wait exceeds the server's own budget.
        let absent = directory.path().join("absent.sock");
        let error = join_launched(&absent, limits, &AtomicU64::new(0), || None, || None);
        assert!(
            error
                .unwrap_err()
                .starts_with("Codex server did not open its local socket within")
        );
    }

    #[test]
    fn a_lock_that_is_never_released_ends_the_wait_naming_its_holder() {
        let directory = tempfile::tempdir().unwrap();
        let limits = ListenerLimits {
            own: Duration::from_secs(5),
            queued: Duration::from_millis(200),
            sample: Duration::from_millis(20),
        };
        let started = Instant::now();
        let error = join_launched(
            &directory.path().join("codex.sock"),
            limits,
            &AtomicU64::new(0),
            || Some(LockWait { holder: Some(4242) }),
            || None,
        )
        .unwrap_err();
        assert!(
            error.starts_with("Codex server waited more than")
                && error.ends_with("for process 4242 to release a lock it needs to start"),
            "{error}"
        );
        assert!(started.elapsed() < Duration::from_secs(5));
    }
}
