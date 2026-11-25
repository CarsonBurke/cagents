//! Interrupting or terminating the CLI must also cancel its detached update.
use super::*;
use std::{
    os::unix::net::UnixStream,
    sync::{OnceLock, atomic::AtomicI32},
};

static REQUESTED: AtomicBool = AtomicBool::new(false);
static WAKE: OnceLock<(UnixStream, UnixStream)> = OnceLock::new();
static WAKE_WRITER: AtomicI32 = AtomicI32::new(-1);

extern "C" fn interrupt(_: libc::c_int) {
    REQUESTED.store(true, Ordering::Release);
    let writer = WAKE_WRITER.load(Ordering::Relaxed);
    if writer >= 0 {
        // The process-lifetime socket stays open and nonblocking. A full
        // buffer already contains a wake; write is async-signal-safe.
        let saved = errno::errno();
        unsafe { libc::write(writer, b"i".as_ptr().cast(), 1) };
        errno::set_errno(saved);
    }
}

pub(super) fn wake_fd() -> i32 {
    WAKE.get().map_or(-1, |(reader, _)| reader.as_raw_fd())
}

pub(super) fn drain_wake() {
    if let Some((reader, _)) = WAKE.get() {
        let mut reader = reader;
        let mut bytes = [0; 64];
        while reader.read(&mut bytes).is_ok_and(|count| count > 0) {}
    }
}

pub(super) struct Interrupt {
    previous_int: libc::sigaction,
    previous_term: libc::sigaction,
}

impl Interrupt {
    pub(super) fn install() -> io::Result<Self> {
        if WAKE.get().is_none() {
            let (reader, writer) = UnixStream::pair()?;
            reader.set_nonblocking(true)?;
            writer.set_nonblocking(true)?;
            let _ = WAKE.set((reader, writer));
        }
        WAKE_WRITER.store(WAKE.get().unwrap().1.as_raw_fd(), Ordering::Relaxed);
        REQUESTED.store(false, Ordering::Relaxed);
        drain_wake();
        // SAFETY: initialized sigactions. The handler touches lock-free
        // atomics and writes to the process's nonblocking wake.
        unsafe {
            let mut action: libc::sigaction = std::mem::zeroed();
            let mut previous_int = std::mem::zeroed();
            let mut previous_term = std::mem::zeroed();
            libc::sigemptyset(&mut action.sa_mask);
            action.sa_sigaction = interrupt as *const () as usize;
            if libc::sigaction(libc::SIGINT, &action, &mut previous_int) == -1 {
                return Err(io::Error::last_os_error());
            }
            if libc::sigaction(libc::SIGTERM, &action, &mut previous_term) == -1 {
                let error = io::Error::last_os_error();
                libc::sigaction(libc::SIGINT, &previous_int, std::ptr::null_mut());
                return Err(error);
            }
            Ok(Self {
                previous_int,
                previous_term,
            })
        }
    }
}

impl Drop for Interrupt {
    fn drop(&mut self) {
        // SAFETY: restore the exact dispositions saved by install.
        unsafe {
            libc::sigaction(libc::SIGTERM, &self.previous_term, std::ptr::null_mut());
            libc::sigaction(libc::SIGINT, &self.previous_int, std::ptr::null_mut());
        }
    }
}

pub(super) fn requested() -> bool {
    REQUESTED.load(Ordering::Acquire)
}

/// Read only available bytes instead of stdio, which can retry an interrupted
/// read indefinitely. Avoid buffering past the newline so a pasted second
/// answer remains visible to poll on the next prompt.
pub(super) fn read_line(answer: &mut String) -> io::Result<usize> {
    // Reopen instead of dup: O_NONBLOCK on a duplicate would also change the
    // parent shell's shared stdin description. An owned description prevents
    // a terminal SIGINT input flush between poll and read from blocking us.
    let mut input = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open("/proc/self/fd/0")?;
    let mut bytes = Vec::new();
    loop {
        if requested() {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "Update cancellation requested",
            ));
        }
        let mut poll = [input.as_raw_fd(), wake_fd()].map(|fd| libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        });
        // SAFETY: two initialized descriptors retained through the call.
        let ready = unsafe { libc::poll(poll.as_mut_ptr(), poll.len() as libc::nfds_t, -1) };
        if ready < 0 {
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
        } else if ready > 0 {
            if poll[1].revents != 0 {
                drain_wake();
            }
            if requested() {
                continue;
            }
            let mut byte = [0u8];
            match input.read(&mut byte) {
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock
                    ) =>
                {
                    continue;
                }
                Err(error) => return Err(error),
                Ok(read) => {
                    if read > 0 {
                        bytes.push(byte[0]);
                    }
                    if read == 0 || byte[0] == b'\n' {
                        answer.push_str(
                            std::str::from_utf8(&bytes).map_err(|error| {
                                io::Error::new(io::ErrorKind::InvalidData, error)
                            })?,
                        );
                        return Ok(bytes.len());
                    }
                }
            }
        }
    }
}

#[cfg(all(test, not(any(feature = "host", feature = "ui"))))]
mod tests {
    use super::*;

    #[test]
    #[cfg_attr(
        not(target_os = "linux"),
        ignore = "self-update is Linux-only: it relies on memfd seals and /proc"
    )]
    fn interrupt_signals_cancel_review_without_waiting_for_stdin() {
        const CHILD: &str = "CAGENTS_TEST_UPDATE_INTERRUPT";
        if let Some(ready) = std::env::var_os(CHILD) {
            // Different prior actions make restoring the wrong signal's saved
            // disposition observable, without changing the parent test process.
            let prior_actions = [libc::SIGINT, libc::SIGTERM].map(|signal| unsafe {
                let mut action: libc::sigaction = std::mem::zeroed();
                libc::sigemptyset(&mut action.sa_mask);
                libc::sigaddset(&mut action.sa_mask, libc::SIGUSR1);
                action.sa_sigaction = if signal == libc::SIGINT {
                    libc::SIG_IGN
                } else {
                    interrupt as *const () as usize
                };
                action.sa_flags = libc::SA_RESTART;
                assert_eq!(libc::sigaction(signal, &action, std::ptr::null_mut()), 0);
                // Query the kernel representation, which may add flags.
                assert_eq!(libc::sigaction(signal, std::ptr::null(), &mut action), 0);
                (signal, action)
            });
            let interrupt = Interrupt::install().unwrap();
            let cancellation = CancellationToken::for_terminal();
            let stdin_flags = unsafe { libc::fcntl(libc::STDIN_FILENO, libc::F_GETFL) };
            fs::write(ready, b"ready").unwrap();
            let error = read_line(&mut String::new()).unwrap_err();
            assert_eq!(
                unsafe { libc::fcntl(libc::STDIN_FILENO, libc::F_GETFL) },
                stdin_flags
            );
            assert_eq!(error.kind(), io::ErrorKind::Interrupted);
            assert!(requested());
            assert!(
                cancellation.cancelled(),
                "terminal discovery must honor cancellation signals too"
            );
            assert!(
                !CancellationToken::new().cancelled(),
                "unrelated jobs have their own cancellation"
            );
            drop(interrupt);
            for (signal, previous) in prior_actions {
                let restored = unsafe {
                    let mut action: libc::sigaction = std::mem::zeroed();
                    assert_eq!(libc::sigaction(signal, std::ptr::null(), &mut action), 0);
                    action
                };
                assert_eq!(restored.sa_sigaction, previous.sa_sigaction);
                assert_eq!(restored.sa_flags, previous.sa_flags);
                for masked in [libc::SIGINT, libc::SIGTERM, libc::SIGUSR1] {
                    assert_eq!(
                        unsafe { libc::sigismember(&restored.sa_mask, masked) },
                        unsafe { libc::sigismember(&previous.sa_mask, masked) }
                    );
                }
            }
            return;
        }
        struct Child(std::process::Child);
        impl Drop for Child {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        for signal in [libc::SIGINT, libc::SIGTERM] {
            let directory = tempfile::tempdir().unwrap();
            let ready = directory.path().join("ready");
            let mut fixture = Child(Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "update::cancellation::tests::interrupt_signals_cancel_review_without_waiting_for_stdin",
                    "--nocapture",
                ])
                .env(CHILD, &ready)
                .stdin(Stdio::piped())
                .spawn()
                .unwrap());
            let child = &mut fixture.0;
            let deadline = Instant::now() + Duration::from_secs(5);
            while !ready.exists() {
                assert!(
                    Instant::now() < deadline,
                    "child did not install cancellation handlers"
                );
                assert!(child.try_wait().unwrap().is_none());
                thread::sleep(Duration::from_millis(10));
            }
            // Only this private child receives the signal. Its stdin remains
            // open and empty throughout the test.
            assert_eq!(unsafe { libc::kill(child.id() as i32, signal) }, 0);
            loop {
                if let Some(status) = child.try_wait().unwrap() {
                    assert!(status.success(), "signal {signal}: {status}");
                    break;
                }
                if Instant::now() >= deadline {
                    child.kill().unwrap();
                    child.wait().unwrap();
                    panic!("signal {signal} did not interrupt the empty stdin wait");
                }
                thread::sleep(Duration::from_millis(10));
            }
        }
    }
}
