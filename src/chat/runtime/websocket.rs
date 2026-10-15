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

/// How long a launched server has to start accepting at its own endpoint.
const LISTENER_TIMEOUT: Duration = Duration::from_secs(15);

/// Join the endpoint of a server this conversation launched, once it accepts.
///
/// The socket exists from the moment the server binds it, which is before it
/// listens, so a refusal there is a launch still in progress rather than a
/// failure. `halted` reports why waiting any longer is pointless.
fn join_launched(
    path: &Path,
    mut halted: impl FnMut() -> Option<&'static str>,
) -> Result<UnixStream, String> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    let started = Instant::now();
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
        if started.elapsed() >= LISTENER_TIMEOUT {
            return Err(format!(
                "Codex server did not open its local socket within {} seconds",
                LISTENER_TIMEOUT.as_secs()
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

impl Actor {
    /// Serve the listener this conversation's own server is bringing up.
    pub(super) fn run_launched_websocket(
        &mut self,
        initial: Step,
        path: &Path,
    ) -> Result<(), String> {
        let socket = join_launched(path, || {
            startup_halt_reason(self.child.as_ref(), self.stop.load(Ordering::Acquire))
        })?;
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
        self.close_input_fd=Some(socket.get_ref().socket.as_raw_fd());
        self.close_input_drained=false;
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
            self.close_input_drained=false;
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
                        let step = self.adapter.receive(frame)?;
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
                        self.close_input_drained=!read_again && socket.get_ref().progress.partial_started().is_none();
                    }
                    Err(error) => return Err(format!("Codex WebSocket read: {error}")),
                }
            }
            if !flushing && !close_sent && !peer_closed {
                self.drain_commands(closing.is_some())?;
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
            Err("Codex connection closed before pending work was confirmed; restart the session to continue".into())
        } else {
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;

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
        let joined = join_launched(&path, || {
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
        let child = Process::new("/bin/sh")
            .args(["-c", "exit 7"])
            .process_group(0)
            .spawn()
            .unwrap();
        let mut provider = ChildGuard::new(child).unwrap();
        assert_eq!(
            join_launched(&path, || startup_halt_reason(Some(&provider), false)).unwrap_err(),
            "Codex server exited before opening its local socket"
        );
        assert_eq!(
            provider.observed_exit_status().unwrap().unwrap().code(),
            Some(7)
        );
        provider.terminate().unwrap();
        assert_eq!(provider.child.try_wait().unwrap().unwrap().code(), Some(7));

        let child = Process::new("/bin/true").process_group(0).spawn().unwrap();
        let mut provider = ChildGuard::new(child).unwrap();
        provider.child.wait().unwrap();
        assert_eq!(
            join_launched(&path, || startup_halt_reason(Some(&provider), false)).unwrap_err(),
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
            join_launched(&path, || Some("Provider startup cancelled")).unwrap_err(),
            "Provider startup cancelled"
        );
        let _server = bound(&path);
        assert_eq!(
            join_launched(&path, || Some(
                "Codex server exited before opening its local socket"
            ))
            .unwrap_err(),
            "Codex server exited before opening its local socket"
        );
    }
}
