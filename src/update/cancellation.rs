//! Ctrl-C belongs to the detached update, not only its observing CLI.
use super::*;

static REQUESTED: AtomicBool = AtomicBool::new(false);

extern "C" fn interrupt(_: libc::c_int) {
    REQUESTED.store(true, Ordering::Relaxed);
}

pub(super) struct Interrupt(libc::sigaction);

impl Interrupt {
    pub(super) fn install() -> io::Result<Self> {
        REQUESTED.store(false, Ordering::Relaxed);
        // SAFETY: valid initialized sigactions; the handler only touches a
        // lock-free atomic. Omitting SA_RESTART also wakes a blocked poll.
        unsafe {
            let mut action: libc::sigaction = std::mem::zeroed();
            let mut previous = std::mem::zeroed();
            libc::sigemptyset(&mut action.sa_mask);
            action.sa_sigaction = interrupt as *const () as usize;
            if libc::sigaction(libc::SIGINT, &action, &mut previous) == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(Self(previous))
        }
    }
}

impl Drop for Interrupt {
    fn drop(&mut self) {
        // SAFETY: restore the exact disposition saved by install.
        unsafe { libc::sigaction(libc::SIGINT, &self.0, std::ptr::null_mut()) };
    }
}

pub(super) fn requested() -> bool {
    REQUESTED.load(Ordering::Relaxed)
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
        let mut poll = libc::pollfd {
            fd: input.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one initialized pollfd retained through the call.
        let ready = unsafe { libc::poll(&mut poll, 1, 100) };
        if ready < 0 {
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
        } else if ready > 0 {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sigint_interrupts_review_without_waiting_for_stdin() {
        const CHILD: &str = "CAGENTS_TEST_UPDATE_INTERRUPT";
        if let Some(ready) = std::env::var_os(CHILD) {
            let _interrupt = Interrupt::install().unwrap();
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
                "terminal discovery must honor Ctrl-C too"
            );
            assert!(
                !CancellationToken::new().cancelled(),
                "unrelated jobs have their own cancellation"
            );
            return;
        }
        let directory = tempfile::tempdir().unwrap();
        let ready = directory.path().join("ready");
        struct Child(std::process::Child);
        impl Drop for Child {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let mut fixture = Child(Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "update::cancellation::tests::sigint_interrupts_review_without_waiting_for_stdin",
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
                "child did not install SIGINT handler"
            );
            assert!(child.try_wait().unwrap().is_none());
            thread::sleep(Duration::from_millis(10));
        }
        // Only this private child receives a real Ctrl-C equivalent. Its stdin
        // remains open and empty throughout the test.
        assert_eq!(unsafe { libc::kill(child.id() as i32, libc::SIGINT) }, 0);
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                assert!(status.success());
                break;
            }
            if Instant::now() >= deadline {
                child.kill().unwrap();
                child.wait().unwrap();
                panic!("Ctrl-C did not interrupt the empty stdin wait");
            }
            thread::sleep(Duration::from_millis(10));
        }
    }
}
