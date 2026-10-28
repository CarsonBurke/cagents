//! Actual PTY readiness and bounded parser backpressure, without a child.
use super::*;

struct Reading {
    reader: File,
    observed: Sender<usize>,
}

impl Read for Reading {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        let result = self.reader.read(buffer);
        if let Ok(length) = &result {
            let _ = self.observed.try_send(*length);
        }
        result
    }
}

struct ReaderOwner {
    wake: Option<OwnedFd>,
    discard_pending: Arc<AtomicBool>,
    done: Receiver<()>,
    thread: Option<JoinHandle<()>>,
}

impl ReaderOwner {
    fn start(
        mut reader: impl Read + Send + 'static,
        readiness: OwnedFd,
        sender: Sender<PtyMessage>,
    ) -> Self {
        let (wake_rx, wake) = wake_pipe().unwrap();
        let (done_tx, done) = crossbeam_channel::bounded(0);
        let discard_pending = Arc::new(AtomicBool::new(false));
        let reader_discard_pending = Arc::clone(&discard_pending);
        let thread = thread::spawn(move || {
            let _done = done_tx;
            read_pty(
                &mut reader,
                &sender,
                readiness,
                wake_rx,
                &reader_discard_pending,
            );
        });
        Self {
            wake: Some(wake),
            discard_pending,
            done,
            thread: Some(thread),
        }
    }

    fn cancel(&mut self) {
        self.discard_pending.store(true, Ordering::Release);
        self.stop_for_carry();
    }

    fn stop_for_carry(&mut self) {
        drop(self.wake.take());
    }

    fn finish(&mut self) {
        assert!(
            matches!(
                self.done.recv_timeout(Duration::from_secs(2)),
                Err(RecvTimeoutError::Disconnected)
            ),
            "original PTY reader did not finish"
        );
        self.thread.take().unwrap().join().unwrap();
    }
}

impl Drop for ReaderOwner {
    fn drop(&mut self) {
        self.cancel();
        if self.thread.is_some()
            && matches!(
                self.done.recv_timeout(Duration::from_secs(2)),
                Err(RecvTimeoutError::Disconnected)
            )
        {
            let _ = self.thread.take().unwrap().join();
        }
    }
}

fn private_pty() -> (File, File) {
    let mut master = -1;
    let mut slave = -1;
    assert_eq!(
        unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null(),
                std::ptr::null(),
            )
        },
        0,
        "open private PTY: {}",
        std::io::Error::last_os_error()
    );
    let master = unsafe { File::from_raw_fd(master) };
    let slave = unsafe { File::from_raw_fd(slave) };
    for fd in [master.as_raw_fd(), slave.as_raw_fd()] {
        assert_eq!(
            unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) },
            0
        );
    }
    let mut attributes = unsafe { std::mem::zeroed() };
    assert_eq!(
        unsafe { libc::tcgetattr(slave.as_raw_fd(), &mut attributes) },
        0
    );
    unsafe { libc::cfmakeraw(&mut attributes) };
    assert_eq!(
        unsafe { libc::tcsetattr(slave.as_raw_fd(), libc::TCSANOW, &attributes) },
        0
    );
    (master, slave)
}

#[test]
fn original_reader_cancellation_escapes_a_full_output_queue() {
    let (reader, mut slave) = private_pty();
    let readiness = reader.try_clone().unwrap().into();
    let (observed, read) = crossbeam_channel::bounded(1);
    let (sender, output) = crossbeam_channel::bounded(1);
    sender
        .send(PtyMessage::Bytes(b"already queued".to_vec()))
        .unwrap();
    let mut owner = ReaderOwner::start(Reading { reader, observed }, readiness, sender);
    slave.write_all(b"blocked output").unwrap();
    assert!(read.recv_timeout(Duration::from_secs(2)).unwrap() > 0);
    // The actual PTY read has finished. Its output cannot enter the full
    // inbox, and closing the original wake pipe must still release it.
    owner.cancel();
    owner.finish();
    assert!(matches!(
        output.try_recv().unwrap(),
        PtyMessage::Bytes(bytes) if bytes == b"already queued"
    ));
    assert!(output.try_recv().is_err());
}

#[test]
fn actual_pty_output_survives_parser_backpressure_in_order_until_eof() {
    let (reader, mut slave) = private_pty();
    let readiness = reader.try_clone().unwrap().into();
    let (observed, read) = crossbeam_channel::bounded(1);
    let (sender, output) = crossbeam_channel::bounded(1);
    sender
        .send(PtyMessage::Bytes(b"earlier output".to_vec()))
        .unwrap();
    let mut owner = ReaderOwner::start(Reading { reader, observed }, readiness, sender);
    slave.write_all(b"later output").unwrap();
    assert!(read.recv_timeout(Duration::from_secs(2)).unwrap() > 0);
    assert!(matches!(
        output.recv_timeout(Duration::from_secs(2)).unwrap(),
        PtyMessage::Bytes(bytes) if bytes == b"earlier output"
    ));
    expect_bytes(&output, b"later output");
    drop(slave);
    assert!(matches!(
        output.recv_timeout(Duration::from_secs(2)).unwrap(),
        PtyMessage::Eof
    ));
    owner.finish();
}

#[test]
fn stopping_for_carry_keeps_already_read_bytes_behind_a_full_output_queue() {
    let (reader, mut slave) = private_pty();
    let mut successor = reader.try_clone().unwrap();
    let readiness = reader.try_clone().unwrap().into();
    let (observed, read) = crossbeam_channel::bounded(1);
    let (sender, output) = crossbeam_channel::bounded(1);
    sender
        .send(PtyMessage::Bytes(b"previous screen".to_vec()))
        .unwrap();
    let mut owner = ReaderOwner::start(Reading { reader, observed }, readiness, sender);
    slave.write_all(b"already read").unwrap();
    let consumed = read.recv_timeout(Duration::from_secs(2)).unwrap();
    assert!(consumed > 0 && consumed <= b"already read".len());
    // Carry closes the same original wake pipe but must collect this in-flight
    // read before the holder snapshots the screen and transfers the PTY.
    owner.stop_for_carry();
    assert!(matches!(owner.done.try_recv(), Err(TryRecvError::Empty)));
    expect_bytes(&output, b"previous screen");
    expect_bytes(&output, &b"already read"[..consumed]);
    owner.finish();
    assert!(output.try_recv().is_err());
    // The exact master duplicate keeps unread kernel output, then accepts
    // fresh output after the old reader ends. Neither path replays bytes.
    slave.write_all(b"successor").unwrap();
    let expected = [&b"already read"[consumed..], b"successor".as_slice()].concat();
    assert_eq!(read_private_pty(&mut successor, expected.len()), expected);
}

struct ConsumedReadiness {
    reader: File,
    raced: Option<Sender<()>>,
}

impl Read for ConsumedReadiness {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        if let Some(raced) = self.raced.take() {
            // Consume the bytes that made poll readable, just as another
            // duplicate reader can. Obtain EAGAIN from the genuine PTY fd.
            let mut consumed = Vec::new();
            loop {
                match self.reader.read(buffer) {
                    Ok(0) => panic!("private PTY closed before the readiness race"),
                    Ok(length) => consumed.extend_from_slice(&buffer[..length]),
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(error) => {
                        assert_eq!(error.kind(), std::io::ErrorKind::WouldBlock);
                        assert_eq!(consumed, b"stale");
                        raced.send(()).unwrap();
                        return Err(error);
                    }
                }
            }
        }
        self.reader.read(buffer)
    }
}

#[test]
fn shared_nonblocking_pty_readiness_race_rearms_without_losing_output() {
    let (reader, mut slave) = private_pty();
    let readiness: OwnedFd = reader.try_clone().unwrap().into();
    let flags = unsafe { libc::fcntl(reader.as_raw_fd(), libc::F_GETFL) };
    assert!(flags >= 0);
    assert_eq!(
        unsafe { libc::fcntl(reader.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) },
        0
    );
    // This is the same OFD, not an independently configured descriptor.
    assert_ne!(
        unsafe { libc::fcntl(readiness.as_raw_fd(), libc::F_GETFL) } & libc::O_NONBLOCK,
        0
    );
    let (raced, race) = crossbeam_channel::bounded(1);
    let (sender, output) = crossbeam_channel::bounded(1);
    let mut owner = ReaderOwner::start(
        ConsumedReadiness {
            reader,
            raced: Some(raced),
        },
        readiness,
        sender,
    );
    slave.write_all(b"stale").unwrap();
    race.recv_timeout(Duration::from_secs(2)).unwrap();
    slave.write_all(b"current").unwrap();
    expect_bytes(&output, b"current");
    drop(slave);
    assert!(matches!(
        output.recv_timeout(Duration::from_secs(2)).unwrap(),
        PtyMessage::Eof
    ));
    owner.finish();
}

fn expect_bytes(output: &Receiver<PtyMessage>, expected: &[u8]) {
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    let mut bytes = Vec::new();
    while bytes.len() < expected.len() {
        match output.recv_deadline(deadline).unwrap() {
            PtyMessage::Bytes(next) => bytes.extend_from_slice(&next),
            other => panic!("expected remaining PTY bytes, received {other:?}"),
        }
    }
    assert_eq!(bytes, expected);
}

fn read_private_pty(reader: &mut File, length: usize) -> Vec<u8> {
    let flags = unsafe { libc::fcntl(reader.as_raw_fd(), libc::F_GETFL) };
    assert!(flags >= 0);
    assert_eq!(
        unsafe { libc::fcntl(reader.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) },
        0
    );
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    let mut bytes = vec![0; length];
    let mut offset = 0;
    while offset < length {
        let remaining = deadline
            .checked_duration_since(std::time::Instant::now())
            .unwrap();
        let mut readiness = libc::pollfd {
            fd: reader.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let ready = unsafe {
            libc::poll(
                &mut readiness,
                1,
                remaining.as_millis().max(1).min(i32::MAX as u128) as i32,
            )
        };
        if ready < 0 && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
            continue;
        }
        assert!(ready > 0, "private successor PTY did not become readable");
        match reader.read(&mut bytes[offset..]) {
            Ok(0) => panic!("private successor PTY closed before expected output"),
            Ok(count) => offset += count,
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::Interrupted | std::io::ErrorKind::WouldBlock
                ) =>
            {
                continue;
            }
            Err(error) => panic!("read private successor PTY: {error}"),
        }
    }
    bytes
}
