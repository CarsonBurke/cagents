//! Codex's existing shared daemon speaks WebSocket text messages over its Unix socket.
use super::*;
use std::{
    io,
    os::{
        fd::FromRawFd,
        unix::fs::{FileTypeExt, MetadataExt},
    },
    path::Path,
};
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

fn verify_socket(path: &Path, device: u64, inode: u64) -> Result<(), String> {
    let metadata = std::fs::metadata(path).map_err(|error| {
        format!(
            "Cannot verify exact Codex socket {}: {error}",
            path.display()
        )
    })?;
    if !metadata.file_type().is_socket() || metadata.dev() != device || metadata.ino() != inode {
        return Err(
            "Codex socket identity changed; reconnect explicitly without selecting another daemon"
                .into(),
        );
    }
    Ok(())
}

fn connect(path: &Path, device: u64, inode: u64) -> Result<UnixStream, String> {
    verify_socket(path, device, inode)?;
    let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    address.sun_family = libc::AF_UNIX as _;
    let bytes = path.as_os_str().as_encoded_bytes();
    if bytes.is_empty() || bytes.len() >= address.sun_path.len() || bytes.contains(&0) {
        return Err("Invalid Codex Unix socket path".into());
    }
    for (target, byte) in address.sun_path.iter_mut().zip(bytes) {
        *target = *byte as _;
    }
    let fd = unsafe {
        libc::socket(
            libc::AF_UNIX,
            libc::SOCK_STREAM | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
            0,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error().to_string());
    }
    // Take ownership immediately, including every connection/identity error path.
    let stream = unsafe { UnixStream::from_raw_fd(fd) };
    let result = unsafe {
        libc::connect(
            fd,
            (&address as *const libc::sockaddr_un).cast(),
            (std::mem::offset_of!(libc::sockaddr_un, sun_path) + bytes.len() + 1) as _,
        )
    };
    if result < 0 {
        // Linux Unix sockets connect immediately or return EAGAIN for a full
        // backlog. Do not retry against a replaced endpoint or block the host.
        return Err(format!(
            "Cannot join existing Codex socket {}: {}",
            path.display(),
            io::Error::last_os_error()
        ));
    }
    verify_socket(path, device, inode)?;
    Ok(stream)
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

impl Actor {
    pub(super) fn run_websocket(
        &mut self,
        initial: Step,
        path: &Path,
        device: u64,
        inode: u64,
    ) -> Result<(), String> {
        let socket = connect(path, device, inode)?;
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
            if closing.is_some_and(|at: Instant| at.elapsed() > IO_TIMEOUT) {
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
                        let step = self.adapter.receive(frame)?;
                        self.step(step)?;
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
                (self.notify)();
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
}
