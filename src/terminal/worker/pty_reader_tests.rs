//! Actual PTY readiness and bounded parser backpressure, without a child.
use super::*;
use std::{io::Read, os::fd::FromRawFd};

const PROMPT: Duration = Duration::from_secs(5);

fn private_pty() -> (File, File) {
    let mut master = -1;
    let mut slave = -1;
    assert_eq!(
        unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                // Linux takes these as const, Darwin as mut; either accepts mut.
                std::ptr::null_mut(),
                std::ptr::null_mut(),
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
    nonblocking_writer(&master).unwrap();
    (master, slave)
}

fn activate(reader: &mut Reader) {
    reader.task.start.take().unwrap().send(()).unwrap();
}

fn stop(reader: &mut Reader) {
    reader.task.stop();
    assert_eq!(
        reader.task.done.recv_timeout(PROMPT),
        Err(RecvTimeoutError::Disconnected),
        "reader did not release its PTY"
    );
}

#[test]
fn a_full_parser_inbox_does_not_block_another_pty_or_lose_carry_bytes() {
    let (master, mut slave) = private_pty();
    let mut successor = master.try_clone().unwrap();
    let mut reader = Reader::prepare_file(master, 1, b"earlier output".to_vec()).unwrap();
    activate(&mut reader);
    slave.write_all(b"unread output").unwrap();

    // The other stream completes on the same reactor while this one has no
    // room for another message.
    let (other_master, mut other_slave) = private_pty();
    let mut other = Reader::prepare_file(other_master, 1, Vec::new()).unwrap();
    activate(&mut other);
    other_slave.write_all(b"another terminal").unwrap();
    expect_bytes(&other, b"another terminal");

    stop(&mut reader);
    expect_bytes(&reader, b"earlier output");
    assert!(reader.output.try_recv().is_err());
    // The former reader did not pull an extra chunk out of the kernel. The
    // exact successor receives it without losing or repeating bytes.
    slave.write_all(b" successor").unwrap();
    let expected = b"unread output successor";
    assert_eq!(read_private_pty(&mut successor, expected.len()), expected);
    stop(&mut other);
}

#[test]
fn parser_capacity_resumes_the_reader_and_preserves_output_order_through_eof() {
    let (master, mut slave) = private_pty();
    let mut reader = Reader::prepare_file(master, 1, b"earlier output".to_vec()).unwrap();
    activate(&mut reader);
    slave.write_all(b"later output").unwrap();

    expect_bytes(&reader, b"earlier output");
    expect_bytes(&reader, b"later output");
    drop(slave);
    assert!(matches!(
        reader.output.recv_timeout(PROMPT).unwrap(),
        PtyMessage::Eof
    ));
    stop(&mut reader);
}

#[test]
fn a_parked_reader_releases_the_pty_without_consuming_output() {
    let (master, mut slave) = private_pty();
    let mut successor = master.try_clone().unwrap();
    let mut reader = Reader::prepare_file(master, 1, b"retained prefix".to_vec()).unwrap();
    slave.write_all(b"still in the PTY").unwrap();
    stop(&mut reader);
    expect_bytes(&reader, b"retained prefix");
    assert!(reader.output.try_recv().is_err());
    assert_eq!(read_private_pty(&mut successor, 16), b"still in the PTY");
}

#[cfg(target_os = "macos")]
#[test]
fn destructive_shutdown_drains_a_full_pty_output_inbox() {
    let (master, mut slave) = private_pty();
    let watch = master.try_clone().unwrap();
    let mut reader = Reader::prepare_file(master, 1, b"already queued".to_vec()).unwrap();
    activate(&mut reader);
    slave
        .write_all(b"drain while the renderer is closing")
        .unwrap();
    reader.task.discard_pending();
    let deadline = Instant::now() + PROMPT;
    loop {
        let mut readable = libc::pollfd {
            fd: watch.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let ready = unsafe { libc::poll(&mut readable, 1, 0) };
        assert!(ready >= 0);
        if ready == 0 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "destructive drain stalled at a full inbox"
        );
        thread::sleep(Duration::from_millis(2));
    }
    stop(&mut reader);
    expect_bytes(&reader, b"already queued");
    assert!(reader.output.try_recv().is_err());
}

#[test]
fn a_consumed_readiness_edge_rearms_the_original_pty() {
    use tokio::io::{Interest, unix::AsyncFd};

    let (master, mut slave) = private_pty();
    let mut duplicate = master.try_clone().unwrap();
    let runtime = super::super::reactor::runtime().unwrap();
    let (started, ready) = crossbeam_channel::bounded(1);
    let (raced, rearmed) = crossbeam_channel::bounded(1);
    let handle = runtime.spawn(async move {
        let master = AsyncFd::with_interest(master, Interest::READABLE).unwrap();
        let mut edge = master.readable().await.unwrap();
        let mut consumed = [0; 5];
        duplicate.read_exact(&mut consumed).unwrap();
        assert_eq!(&consumed, b"stale");
        assert!(
            edge.try_io(|fd| pty_reader::read_message(fd.get_ref()))
                .is_err()
        );
        started.send(()).unwrap();
        let mut edge = master.readable().await.unwrap();
        let output = edge
            .try_io(|fd| pty_reader::read_message(fd.get_ref()))
            .unwrap()
            .unwrap();
        assert!(matches!(output, PtyMessage::Bytes(bytes) if bytes == b"fresh"));
        raced.send(()).unwrap();
    });
    slave.write_all(b"stale").unwrap();
    ready.recv_timeout(PROMPT).unwrap();
    slave.write_all(b"fresh").unwrap();
    rearmed.recv_timeout(PROMPT).unwrap();
    runtime.block_on(handle).unwrap();
}

fn expect_bytes(reader: &Reader, expected: &[u8]) {
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    let mut bytes = Vec::new();
    while bytes.len() < expected.len() {
        let message = reader.output.recv_deadline(deadline).unwrap();
        reader.task.consumed();
        match message {
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
