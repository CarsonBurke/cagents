//! Real private PTYs without children, accounts, daemons or signals.
use super::*;

struct Reading {
    master: File,
    slave: File,
    wake: Option<OwnedFd>,
    discard: Arc<AtomicBool>,
    task: Option<Task>,
    done: Receiver<()>,
    output: Receiver<PtyMessage>,
    observed: Receiver<(usize, std::thread::ThreadId)>,
    start: Option<tokio::sync::oneshot::Sender<()>>,
}

fn pty() -> (File, File) {
    let (mut master, mut slave) = (-1, -1);
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
        0
    );
    let master = unsafe { File::from_raw_fd(master) };
    let slave = unsafe { File::from_raw_fd(slave) };
    let mut attrs: libc::termios = unsafe { std::mem::zeroed() };
    assert_eq!(unsafe { libc::tcgetattr(slave.as_raw_fd(), &mut attrs) }, 0);
    unsafe { libc::cfmakeraw(&mut attrs) };
    assert_eq!(
        unsafe { libc::tcsetattr(slave.as_raw_fd(), libc::TCSANOW, &attrs) },
        0
    );
    let flags = unsafe { libc::fcntl(master.as_raw_fd(), libc::F_GETFL) };
    assert!(flags >= 0);
    assert_eq!(
        unsafe { libc::fcntl(master.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) },
        0
    );
    (master, slave)
}

impl Reading {
    fn prepare(blocked: bool) -> Self {
        let (master, slave) = pty();
        Self::at(master, slave, blocked)
    }

    fn at(master: File, slave: File, blocked: bool) -> Self {
        let (sender, output) = crossbeam_channel::bounded(1);
        if blocked {
            sender
                .send(PtyMessage::Bytes(b"queued-before-read".to_vec()))
                .unwrap();
        }
        let (wake_rx, wake) = wake_pipe().unwrap();
        let discard = Arc::new(AtomicBool::new(false));
        let (observed_tx, observed) = crossbeam_channel::bounded(64);
        let (task, start, done) = Task::prepare_observed(
            master.try_clone().unwrap(),
            wake_rx,
            sender,
            Arc::clone(&discard),
            move |count| {
                let _ = observed_tx.try_send((count, std::thread::current().id()));
            },
        )
        .unwrap();
        Self {
            master,
            slave,
            wake: Some(wake),
            discard,
            task: Some(task),
            done,
            output,
            observed,
            start: Some(start),
        }
    }

    fn activate(&mut self) {
        self.start.take().unwrap().send(()).unwrap();
    }
    fn carry(&mut self) {
        drop(self.wake.take());
    }
    fn cancel(&mut self) {
        self.discard.store(true, Ordering::Release);
        self.carry();
    }
    fn finish(&mut self) {
        assert!(
            matches!(
                self.done.recv_timeout(Duration::from_secs(2)),
                Err(RecvTimeoutError::Disconnected)
            ),
            "original shared reader must finish positively"
        );
        self.task.take().unwrap().join().unwrap();
    }
    fn bytes(&self) -> Vec<u8> {
        match self.output.recv_timeout(Duration::from_secs(2)).unwrap() {
            PtyMessage::Bytes(bytes) => bytes,
            _ => panic!("expected original PTY bytes"),
        }
    }
}

impl Drop for Reading {
    fn drop(&mut self) {
        self.cancel();
        // Positive task completion is required on successful paths. On panic,
        // cancellation retains all descriptors until the future is destroyed.
        if self.task.is_some()
            && matches!(
                self.done.recv_timeout(Duration::from_secs(2)),
                Err(RecvTimeoutError::Disconnected)
            )
        {
            let _ = self.task.take().unwrap().join();
        }
    }
}

#[test]
fn thirty_actual_ptys_read_on_one_shared_reactor_without_per_stream_threads() {
    let mut readers: Vec<_> = (0..30).map(|_| Reading::prepare(false)).collect();
    let mut threads = std::collections::HashSet::new();
    for (index, reading) in readers.iter_mut().enumerate() {
        reading.activate();
        reading
            .slave
            .write_all(format!("stream-{index}\n").as_bytes())
            .unwrap();
    }
    for (index, reading) in readers.iter_mut().enumerate() {
        assert_eq!(reading.bytes(), format!("stream-{index}\n").as_bytes());
        threads.insert(
            reading
                .observed
                .recv_timeout(Duration::from_secs(2))
                .unwrap()
                .1,
        );
        reading.cancel();
        reading.finish();
    }
    assert_eq!(
        threads.len(),
        1,
        "actual PTY read callbacks share one OS thread"
    );
}

#[test]
fn prepared_reader_consumes_nothing_until_original_adoption_commits() {
    let mut reading = Reading::prepare(false);
    reading.slave.write_all(b"original-before-commit").unwrap();
    assert!(matches!(
        reading.output.recv_timeout(Duration::from_millis(30)),
        Err(RecvTimeoutError::Timeout)
    ));
    assert!(reading.observed.is_empty());
    reading.activate();
    assert_eq!(reading.bytes(), b"original-before-commit");
    reading.cancel();
    reading.finish();
}

#[test]
fn carry_drains_consumed_full_inbox_bytes_before_same_master_successor_reads() {
    let mut reading = Reading::prepare(true);
    reading.activate();
    reading.slave.write_all(b"A").unwrap();
    assert_eq!(
        reading
            .observed
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .0,
        b"A".len()
    );
    reading.carry();
    assert!(matches!(
        reading.done.recv_timeout(Duration::from_millis(30)),
        Err(RecvTimeoutError::Timeout)
    ));
    assert_eq!(reading.bytes(), b"queued-before-read");
    assert_eq!(reading.bytes(), b"A");
    reading.finish();
    reading.slave.write_all(b"unread-for-successor").unwrap();
    let mut successor = Reading::at(
        reading.master.try_clone().unwrap(),
        reading.slave.try_clone().unwrap(),
        false,
    );
    successor.activate();
    assert_eq!(successor.bytes(), b"unread-for-successor");
    successor.slave.write_all(b"fresh-after-adoption").unwrap();
    assert_eq!(successor.bytes(), b"fresh-after-adoption");
    successor.cancel();
    successor.finish();
}

#[test]
fn destructive_cancel_of_a_full_reader_does_not_block_other_ptys() {
    let mut blocked = Reading::prepare(true);
    blocked.activate();
    blocked.slave.write_all(b"original-pending-read").unwrap();
    blocked
        .observed
        .recv_timeout(Duration::from_secs(2))
        .unwrap();
    let mut other = Reading::prepare(false);
    other.activate();
    other.slave.write_all(b"other-progress").unwrap();
    assert_eq!(other.bytes(), b"other-progress");
    blocked.cancel();
    blocked.finish();
    assert_eq!(blocked.bytes(), b"queued-before-read");
    assert!(
        blocked.output.try_recv().is_err(),
        "only destructive cancellation may discard the consumed pending message"
    );
    other.cancel();
    other.finish();
}
