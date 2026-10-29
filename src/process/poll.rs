//! Run a child on its caller's thread, with one readiness wait for all I/O.

use super::*;
use std::process::{Child, ChildStdin};

const IO_TURN_BYTES: usize = 256 * 1024;

/// The caller is the only reaper. Retain the original pidfd and the unreaped
/// leader until the final possible group signal, including output-limit cleanup.
struct PolledChild {
    child: Option<Child>,
    identity: Option<OwnedFd>,
    group: u32,
    owned: bool,
}

impl PolledChild {
    fn new(child: Child, identity: OwnedFd) -> Self {
        Self {
            group: child.id(),
            child: Some(child),
            identity: Some(identity),
            owned: true,
        }
    }

    fn observe(&mut self) -> io::Result<Option<ExitStatus>> {
        let result = observe_child_exit(
            self.identity.as_ref().expect("original child identity"),
            self.group,
            true,
        );
        if result.is_err() {
            self.owned = false;
        }
        result
    }

    fn signal(&mut self, signal: i32) -> bool {
        if !self.owned || self.observe().is_err() {
            return false;
        }
        signal_process_group(self.group, signal);
        true
    }

    fn descriptor(&self) -> i32 {
        if self.owned {
            self.identity
                .as_ref()
                .expect("original child identity")
                .as_raw_fd()
        } else {
            -1
        }
    }

    fn finish(&mut self, observed_exit: bool) -> Option<String> {
        let child = self.child.take().expect("owned child");
        let identity = self.identity.take().expect("original child identity");
        self.owned = false;
        if observed_exit {
            // An observed WNOWAIT exit cannot block here. Numeric PID reuse is
            // impossible until this final reap, and no further group signal runs.
            match child_exit_status(&identity, self.group, libc::WEXITED | libc::WNOHANG) {
                Ok(Some(_)) => None,
                Ok(None) => {
                    let mut waiter = GroupWaiter::new(child, Ok(identity));
                    waiter.finish(false);
                    Some("final child reap returned no status".into())
                }
                Err(error) => Some(format!("final child reap failed: {error}")),
            }
        } else {
            // SIGKILL cannot immediately reap an uninterruptible kernel task.
            // Only this exceptional cleanup retains one waiter, rather than
            // parking the calling service or leaking a future zombie.
            let mut waiter = GroupWaiter::new(child, Ok(identity));
            waiter.finish(false);
            None
        }
    }
}

impl Drop for PolledChild {
    fn drop(&mut self) {
        if self.child.is_some() {
            self.signal(SIGKILL);
            self.finish(false);
        }
    }
}

struct PipeCapture<R> {
    pipe: Option<R>,
    captured: PipeRead,
    error: Option<String>,
    limit: usize,
    listing: Option<record_capture::NulPrefix>,
}

impl<R: Read + AsFd> PipeCapture<R> {
    fn new(pipe: Option<R>, limit: usize, records: Option<usize>) -> Self {
        Self {
            pipe,
            captured: PipeRead {
                output: Vec::with_capacity(limit.min(8192)),
                limit_exceeded: false,
                stdout_truncated: false,
            },
            error: None,
            limit,
            listing: records.map(|records| record_capture::NulPrefix::new(records, limit)),
        }
    }

    fn finish(self) -> PipeRead {
        PipeRead::finish(
            self.captured.output,
            self.listing,
            self.captured.limit_exceeded,
        )
    }

    fn descriptor(&self) -> i32 {
        self.pipe
            .as_ref()
            .map_or(-1, |pipe| pipe.as_fd().as_raw_fd())
    }

    fn drain_ready(&mut self) {
        let Some(pipe) = self.pipe.as_mut() else {
            return;
        };
        let mut buffer = [0_u8; 8192];
        let mut drained = 0;
        while drained < IO_TURN_BYTES {
            match pipe.read(&mut buffer) {
                Ok(0) => {
                    self.pipe.take();
                    break;
                }
                Ok(read) => {
                    drained += read;
                    if let Some(listing) = &mut self.listing {
                        listing.push(&buffer[..read]);
                        continue;
                    }
                    let remaining = self.limit.saturating_sub(self.captured.output.len());
                    self.captured
                        .output
                        .extend_from_slice(&buffer[..read.min(remaining)]);
                    if read > remaining {
                        self.captured.limit_exceeded = true;
                        self.pipe.take();
                        break;
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) => {
                    self.error = Some(error.to_string());
                    self.pipe.take();
                    break;
                }
            }
        }
    }
}

fn write_ready(
    stdin: &mut Option<ChildStdin>,
    bytes: &[u8],
    written: &mut usize,
) -> Option<String> {
    let pipe = stdin.as_mut()?;
    let end = bytes.len().min(written.saturating_add(IO_TURN_BYTES));
    while *written < end {
        match pipe.write(&bytes[*written..end]) {
            Ok(0) => {
                stdin.take();
                return Some("stdin: failed to write whole input".into());
            }
            Ok(count) => *written += count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => return None,
            Err(error) => {
                stdin.take();
                return Some(format!("stdin: {error}"));
            }
        }
    }
    if *written == bytes.len() {
        let error = pipe.flush().err().map(|error| format!("stdin: {error}"));
        stdin.take();
        return error;
    }
    None
}

fn cancellation_descriptor() -> io::Result<Arc<OwnedFd>> {
    // SAFETY: eventfd returns a fresh, close-on-exec owned descriptor. Its
    // nonblocking counter coalesces notifications and cannot stall cancel().
    let raw = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
    if raw < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(Arc::new(unsafe { OwnedFd::from_raw_fd(raw) }))
    }
}

fn timeout_milliseconds(deadline: Option<Instant>, now: Instant) -> i32 {
    deadline.map_or(-1, |deadline| {
        let remaining = deadline.saturating_duration_since(now);
        let millis = remaining.as_millis() + u128::from(remaining.subsec_nanos() % 1_000_000 != 0);
        millis.min(i32::MAX as u128) as i32
    })
}

pub(super) fn execute(
    spec: &CommandSpec,
    cancellation: &CancellationToken,
) -> Result<ProcessOutput, ProcessFailure> {
    let mut child = spec.to_command().spawn().map_err(|error| {
        process_failure(spec, ProcessFailureKind::Spawn, &error.to_string(), false)
    })?;
    let identity = child_identity(&child);
    let setup = (|| {
        let stdout = match child.stdout.take() {
            Some(pipe) => Some(pipe),
            None if spec.stdout_file.is_some() => None,
            None => return Err(io::Error::other("stdout pipe was unavailable")),
        };
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| io::Error::other("stderr pipe was unavailable"))?;
        let stdin = child.stdin.take();
        if spec.stdin.is_some() && stdin.is_none() {
            return Err(io::Error::other("stdin pipe was unavailable"));
        }
        if let Some(stdout) = &stdout {
            set_nonblocking(stdout.as_fd())?;
        }
        set_nonblocking(stderr.as_fd())?;
        if let Some(stdin) = &stdin {
            set_nonblocking(stdin.as_fd())?;
        }
        Ok::<_, io::Error>((stdout, stderr, stdin, cancellation_descriptor()?))
    })();
    let (stdout, stderr, mut stdin, wake) = match setup {
        Ok(pipes) => pipes,
        Err(error) => {
            terminate_process_tree(child, identity);
            return Err(process_failure(
                spec,
                ProcessFailureKind::Io,
                &error.to_string(),
                true,
            ));
        }
    };
    let identity = match identity {
        Ok(identity) => identity,
        Err(error) => {
            let message = format!("child identity was unavailable: {error}");
            terminate_process_tree(child, Err(error));
            return Err(process_failure(
                spec,
                ProcessFailureKind::Io,
                &message,
                true,
            ));
        }
    };
    let mut child = PolledChild::new(child, identity);
    let _registration =
        cancellation.register_notifier(CancellationNotifier::Descriptor(Arc::clone(&wake)));
    // A retained stdout file has no pipe and ignores byte/record capture caps.
    let records = stdout.as_ref().and(spec.stdout_record_limit);
    let mut stdout = PipeCapture::new(stdout, spec.stdout_limit, records);
    let mut stderr = PipeCapture::new(Some(stderr), spec.stderr_limit, None);
    let input = spec.stdin.as_deref().unwrap_or_default();
    let mut written = 0;
    if input.is_empty() {
        stdin.take();
    }
    let deadline = spec.timeout.map(|timeout| Instant::now() + timeout);
    let mut terminal_kind = None;
    let mut term_deadline = None;
    let mut cleanup_deadline = None;
    let mut termination_started = false;
    let mut status = None;
    let mut stdin_error = None;
    let mut wait_error = None;
    loop {
        let now = Instant::now();
        if terminal_kind.is_none() {
            terminal_kind = if cancellation.is_cancelled() {
                Some(ProcessFailureKind::Cancelled)
            } else if stdout.captured.limit_exceeded || stderr.captured.limit_exceeded {
                Some(ProcessFailureKind::OutputLimit)
            } else if deadline.is_some_and(|deadline| now >= deadline) {
                Some(ProcessFailureKind::TimedOut)
            } else {
                None
            };
        }
        if terminal_kind.is_some() && !termination_started {
            termination_started = true;
            // Stop feeding a cancelled command. Pipe reads still drain during
            // termination, so a TERM handler cannot deadlock on its diagnostics.
            stdin.take();
            if child.signal(SIGTERM) {
                term_deadline = Some(now + TERMINATION_GRACE);
            } else {
                cleanup_deadline = Some(now + CLEANUP_GRACE);
            }
        }
        if term_deadline.is_some_and(|deadline| now >= deadline || status.is_some()) {
            child.signal(SIGKILL);
            term_deadline = None;
            cleanup_deadline = Some(now + CLEANUP_GRACE);
        }
        if status.is_some()
            && stdout.pipe.is_none()
            && stderr.pipe.is_none()
            && stdin.is_none()
            && term_deadline.is_none()
        {
            break;
        }
        if cleanup_deadline.is_some_and(|deadline| now >= deadline) {
            break;
        }
        let descriptor = |fd, events| libc::pollfd {
            fd,
            events,
            revents: 0,
        };
        let mut watched = [
            descriptor(stdout.descriptor(), libc::POLLIN),
            descriptor(stderr.descriptor(), libc::POLLIN),
            descriptor(stdin.as_ref().map_or(-1, AsRawFd::as_raw_fd), libc::POLLOUT),
            descriptor(
                if status.is_none() {
                    child.descriptor()
                } else {
                    -1
                },
                libc::POLLIN,
            ),
            descriptor(
                if terminal_kind.is_none() {
                    wake.as_raw_fd()
                } else {
                    -1
                },
                libc::POLLIN,
            ),
        ];
        let next_deadline = if termination_started {
            term_deadline.or(cleanup_deadline)
        } else {
            deadline
        };
        // SAFETY: all five descriptors are owned throughout this call. -1
        // disables completed streams; readiness uses no timer when unbounded.
        let ready = unsafe {
            libc::poll(
                watched.as_mut_ptr(),
                watched.len() as libc::nfds_t,
                timeout_milliseconds(next_deadline, Instant::now()),
            )
        };
        if ready < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            wait_error = Some(format!("process readiness wait failed: {error}"));
            terminal_kind.get_or_insert(ProcessFailureKind::Io);
            child.signal(SIGKILL);
            break;
        }
        // Bound work on each stream so an always-readable stdout cannot starve
        // stderr, input, cancellation or a command's deadline.
        if watched[0].revents != 0 {
            stdout.drain_ready();
        }
        if watched[1].revents != 0 {
            stderr.drain_ready();
        }
        if watched[2].revents != 0 {
            stdin_error = write_ready(&mut stdin, input, &mut written).or(stdin_error);
        }
        if watched[3].revents != 0 {
            match child.observe() {
                Ok(exit) => status = exit,
                Err(error) => {
                    wait_error = Some(error.to_string());
                    terminal_kind.get_or_insert(ProcessFailureKind::Io);
                }
            }
        }
    }
    if let Some(error) = child.finish(status.is_some()) {
        wait_error = Some(error);
        terminal_kind.get_or_insert(ProcessFailureKind::Io);
    }
    let pipe_error = match (stdout.error.take(), stderr.error.take()) {
        (Some(out), Some(err)) => Some(format!("stdout: {out}; stderr: {err}")),
        (Some(error), None) => Some(format!("stdout: {error}")),
        (None, Some(error)) => Some(format!("stderr: {error}")),
        (None, None) => None,
    };
    let stdout_read = stdout.finish();
    let stderr_read = stderr.finish();
    if let Some(kind) = terminal_kind {
        let message = match kind {
            ProcessFailureKind::Cancelled => "operation was cancelled".into(),
            ProcessFailureKind::TimedOut => format!(
                "operation timed out after {:?}",
                spec.timeout.expect("timeout deadline")
            ),
            ProcessFailureKind::OutputLimit => output_limit_message(
                stdout_read.limit_exceeded,
                spec.stdout_limit,
                stderr_read.limit_exceeded,
                spec.stderr_limit,
            ),
            ProcessFailureKind::Io => match (wait_error, pipe_error) {
                (Some(wait), Some(pipe)) => format!("{wait}; {pipe}"),
                (Some(error), None) | (None, Some(error)) => error,
                (None, None) => "process wait failed".into(),
            },
            _ => unreachable!(),
        };
        return Err(ProcessFailure {
            kind,
            program: spec.program.clone(),
            exit_code: status.as_ref().and_then(ExitStatus::code),
            signal: status.as_ref().and_then(exit_signal),
            stdout: failure_excerpt(stdout_read.output),
            stderr: failure_excerpt(stderr_read.output),
            message,
            started: true,
        });
    }
    let status = status.ok_or_else(|| {
        process_failure(
            spec,
            ProcessFailureKind::Io,
            "process ended without an exit status",
            true,
        )
    })?;
    if let Some(message) = stdin_error.or(pipe_error) {
        return Err(ProcessFailure {
            kind: ProcessFailureKind::Io,
            program: spec.program.clone(),
            exit_code: status.code(),
            signal: exit_signal(&status),
            stdout: failure_excerpt(stdout_read.output),
            stderr: failure_excerpt(stderr_read.output),
            message,
            started: true,
        });
    }
    let mut output = ProcessOutput::from_parts(status, stdout_read.output, stderr_read.output);
    output.stdout_truncated = stdout_read.stdout_truncated;
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn readiness_wait_uses_the_deadline_instead_of_a_polling_interval() {
        let now = Instant::now();
        assert_eq!(timeout_milliseconds(None, now), -1);
        for milliseconds in [15, 5_000, 120_000] {
            let deadline = now + Duration::from_millis(milliseconds);
            assert_eq!(
                timeout_milliseconds(Some(deadline), now),
                milliseconds as i32
            );
        }
    }

    #[test]
    fn readiness_wait_rounds_remaining_time_up_and_expires_at_zero() {
        let now = Instant::now();
        for (nanoseconds, expected) in [
            (0, 0),
            (1, 1),
            (999_999, 1),
            (1_000_000, 1),
            (1_000_001, 2),
            (999_999_999, 1_000),
            (1_000_000_000, 1_000),
            (1_000_000_001, 1_001),
        ] {
            let deadline = now + Duration::from_nanos(nanoseconds);
            assert_eq!(timeout_milliseconds(Some(deadline), now), expected);
        }
        assert_eq!(
            timeout_milliseconds(Some(now), now + Duration::from_nanos(1)),
            0
        );
    }

    #[test]
    fn readiness_wait_saturates_without_turning_into_an_unbounded_wait() {
        let now = Instant::now();
        let maximum = Duration::from_millis(i32::MAX as u64);
        for remaining in [
            maximum,
            maximum + Duration::from_nanos(1),
            maximum + Duration::from_millis(1),
        ] {
            assert_eq!(timeout_milliseconds(Some(now + remaining), now), i32::MAX);
        }
    }

    #[test]
    fn polled_io_uses_the_retained_directory_after_its_path_is_replaced() {
        let temporary = tempfile::tempdir().unwrap();
        let selected = temporary.path().join("selected");
        fs::create_dir(&selected).unwrap();
        fs::write(selected.join("payload"), b"original directory\n").unwrap();
        let root = crate::fs_safety::Root::open(&selected).unwrap();
        let retained = Arc::new(root.retained_directory().unwrap());
        let descriptor = retained.as_raw_fd();
        let input = vec![b'x'; 2 * 1024 * 1024];
        let command = CommandSpec::new("/bin/sh")
            .args([
                "-c",
                "cat \"$1\"; printf 'retained stderr\\n' >&2; cat",
                "fixture",
            ])
            .arg(format!("/proc/self/fd/{descriptor}/payload"))
            .inherit_file(retained.clone())
            .unwrap()
            .stdin(input.clone())
            .output_limits(3 * 1024 * 1024, 1024)
            .timeout(Duration::from_secs(5));
        drop(retained);
        drop(root);
        let original = temporary.path().join("original");
        fs::rename(&selected, &original).unwrap();
        fs::create_dir(&selected).unwrap();
        fs::write(selected.join("payload"), b"replacement must survive\n").unwrap();

        let output = SystemProcessExecutor
            .execute(&command, &CancellationToken::new())
            .unwrap();
        assert!(output.success);
        assert_eq!(output.stderr, b"retained stderr\n");
        assert_eq!(
            output.stdout.len(),
            b"original directory\n".len() + input.len()
        );
        assert!(output.stdout.starts_with(b"original directory\n"));
        assert_eq!(&output.stdout[b"original directory\n".len()..], input);
        assert_eq!(
            fs::read(original.join("payload")).unwrap(),
            b"original directory\n"
        );
        assert_eq!(
            fs::read(selected.join("payload")).unwrap(),
            b"replacement must survive\n"
        );
    }

    #[test]
    fn polled_nul_prefix_drains_output_while_feeding_large_stdin() {
        let directory = tempfile::tempdir().unwrap();
        let count = directory.path().join("stdin-count");
        let input = vec![b'x'; 2 * 1024 * 1024];
        let command = CommandSpec::new("/bin/sh")
            .args([
                "-c",
                "head -c 2097152 /dev/zero; printf warning >&2; wc -c > \"$COUNT\"",
            ])
            .env("COUNT", count.as_os_str())
            .stdin(input.clone())
            .stdout_record_limit(7)
            .output_limits(32, 7)
            .timeout(Duration::from_secs(5));
        let output = SystemProcessExecutor
            .execute(&command, &CancellationToken::new())
            .unwrap();
        assert!(output.success);
        assert_eq!(output.stdout, [0; 7]);
        assert!(output.stdout_truncated);
        assert_eq!(output.stderr, b"warning");
        assert_eq!(
            fs::read_to_string(count)
                .unwrap()
                .trim()
                .parse::<usize>()
                .unwrap(),
            input.len()
        );
    }

    #[test]
    fn polled_stdout_file_keeps_stdin_stderr_and_ignores_capture_caps() {
        use std::os::unix::fs::FileExt;
        let file = Arc::new(tempfile::tempfile().unwrap());
        let input = vec![b'x'; 2 * 1024 * 1024];
        let command = CommandSpec::new("/bin/sh")
            .args(["-c", "cat; printf warning >&2"])
            .stdin(input.clone())
            .stdout_record_limit(0)
            .output_limits(1, 7)
            .stdout_file(file.clone(), input.len() as u64 + 1)
            .unwrap()
            .timeout(Duration::from_secs(5));
        let output = SystemProcessExecutor
            .execute(&command, &CancellationToken::new())
            .unwrap();
        assert!(output.success);
        assert!(output.stdout.is_empty());
        assert!(!output.stdout_truncated);
        assert_eq!(output.stderr, b"warning");
        assert_eq!(file.metadata().unwrap().len(), input.len() as u64);
        let mut actual = vec![0; input.len()];
        file.read_exact_at(&mut actual, 0).unwrap();
        assert_eq!(actual, input);
    }

    #[test]
    fn git_calls_do_not_create_helper_threads() {
        const CHILD: &str = "CAGENTS_PROCESS_THREAD_CENSUS";
        if std::env::var_os(CHILD).is_none() {
            // Isolate from other parallel tests so their threads cannot skew
            // the census. This is a real Git invocation, not a source assertion.
            let output = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "process::poll::tests::git_calls_do_not_create_helper_threads",
                    "--test-threads=1",
                    "--nocapture",
                ])
                .env(CHILD, "1")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(String::from_utf8_lossy(&output.stdout).contains("git_call_threads="));
            print!("{}", String::from_utf8_lossy(&output.stdout));
            return;
        }
        let baseline = fs::read_dir("/proc/self/task").unwrap().count();
        let command = CommandSpec::new("git")
            .args([
                "-c",
                "alias.census=!find /proc/\"$CENSUS_PARENT\"/task -mindepth 1 -maxdepth 1 | wc -l",
                "census",
            ])
            .env("CENSUS_PARENT", std::process::id().to_string())
            .timeout(Duration::from_secs(5));
        for _ in 0..5 {
            let output = SystemProcessExecutor
                .execute(&command, &CancellationToken::new())
                .unwrap();
            assert!(
                output.success,
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            let measured: usize = String::from_utf8(output.stdout)
                .unwrap()
                .trim()
                .parse()
                .unwrap();
            println!("baseline_threads={baseline} git_call_threads={measured}");
            assert!(
                measured <= baseline,
                "Git invocation added {} helper threads",
                measured.saturating_sub(baseline)
            );
        }
    }

    #[test]
    fn full_stdout_stderr_and_large_stdin_make_progress_together() {
        let input: Vec<u8> = (0..2 * 1024 * 1024)
            .map(|index| (index % 251) as u8)
            .collect();
        let command = CommandSpec::new("/bin/sh")
            .args([
                "-c",
                "head -c 262144 /dev/zero; head -c 262144 /dev/zero >&2; cat",
            ])
            .stdin(input.clone())
            .output_limits(3 * 1024 * 1024, 512 * 1024)
            .timeout(Duration::from_secs(10));
        let output = SystemProcessExecutor
            .execute(&command, &CancellationToken::new())
            .unwrap();
        assert!(output.success);
        assert_eq!(output.stderr, vec![0; 262144]);
        assert_eq!(output.stdout.len(), 262144 + input.len());
        assert!(output.stdout[..262144].iter().all(|byte| *byte == 0));
        assert_eq!(&output.stdout[262144..], input);
    }

    #[test]
    fn cancellation_wakes_poll_when_child_will_not_read_large_input() {
        let directory = tempfile::tempdir().unwrap();
        let ready = directory.path().join("ready");
        let cancellation = CancellationToken::new();
        let canceller = cancellation.clone();
        let command = CommandSpec::new("/bin/sh")
            .args(["-c", "printf ready > \"$READY\"; exec sleep 300"])
            .env("READY", ready.as_os_str())
            .stdin(vec![b'x'; 2 * 1024 * 1024])
            .timeout(Duration::from_secs(30));
        let worker = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(5);
            while !ready.exists() {
                assert!(Instant::now() < deadline, "child never started");
                thread::sleep(Duration::from_millis(1));
            }
            canceller.cancel();
        });
        let start = Instant::now();
        let failure = SystemProcessExecutor
            .execute(&command, &cancellation)
            .unwrap_err();
        worker.join().unwrap();
        assert_eq!(failure.kind, ProcessFailureKind::Cancelled);
        assert!(failure.started);
        assert!(start.elapsed() < Duration::from_secs(2));
        assert!(lock_wakers(&cancellation.0).is_empty());
    }

    #[test]
    fn externally_reaped_leader_cannot_authorize_group_signals() {
        let mut child = Command::new("/bin/sh")
            .args(["-c", "exit 0"])
            .spawn()
            .unwrap();
        let identity = child_identity(&child).unwrap();
        child.wait().unwrap();
        let mut retained = PolledChild::new(child, identity);
        assert!(
            !retained.signal(SIGKILL),
            "reaped original process is no longer an owned group leader"
        );
        assert!(!retained.owned);
        assert!(retained.finish(true).is_some());
    }

    #[test]
    fn cancellation_descriptor_registration_is_nonblocking_and_deregistered() {
        let cancellation = CancellationToken::new();
        let descriptor = cancellation_descriptor().unwrap();
        let registration = cancellation
            .register_notifier(CancellationNotifier::Descriptor(Arc::clone(&descriptor)));
        cancellation.cancel();
        let mut watched = libc::pollfd {
            fd: descriptor.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: this borrowed eventfd remains owned by descriptor.
        assert_eq!(unsafe { libc::poll(&mut watched, 1, 0) }, 1);
        drop(registration);
        assert!(lock_wakers(&cancellation.0).is_empty());
    }

    #[test]
    fn cancellation_and_timeout_keep_both_failure_excerpts_bounded() {
        for cancel in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let ready = directory.path().join("output-written");
            let cancellation = CancellationToken::new();
            let canceller = cancellation.clone();
            let written = ready.clone();
            let worker = cancel.then(|| {
                thread::spawn(move || {
                    let deadline = Instant::now() + Duration::from_secs(5);
                    while !written.exists() {
                        assert!(
                            Instant::now() < deadline,
                            "both output streams never completed"
                        );
                        thread::sleep(Duration::from_millis(1));
                    }
                    canceller.cancel();
                })
            });
            let command = CommandSpec::new("/bin/sh")
                .args([
                    "-c",
                    "head -c 262144 /dev/zero; head -c 262144 /dev/zero >&2; \
                     printf ready > \"$READY\"; exec sleep 300",
                ])
                .env("READY", ready.as_os_str())
                .output_limits(512 * 1024, 512 * 1024)
                .timeout(Duration::from_secs(if cancel { 10 } else { 2 }));
            let outcome = SystemProcessExecutor.execute(&command, &cancellation);
            if let Some(worker) = worker {
                worker.join().unwrap();
            }
            let failure = outcome.unwrap_err();
            assert!(
                ready.exists(),
                "the actual child filled both capture streams"
            );
            assert_eq!(
                failure.kind,
                if cancel {
                    ProcessFailureKind::Cancelled
                } else {
                    ProcessFailureKind::TimedOut
                }
            );
            assert_eq!(failure.stdout.len(), FAILURE_EXCERPT_LIMIT);
            assert_eq!(failure.stderr.len(), FAILURE_EXCERPT_LIMIT);
            assert!(failure.stdout.iter().all(|byte| *byte == 0));
            assert!(failure.stderr.iter().all(|byte| *byte == 0));
        }
    }

    #[test]
    fn closed_stdin_keeps_both_io_failure_excerpts_bounded() {
        let command = CommandSpec::new("/bin/sh")
            .args([
                "-c",
                "exec 0<&-; head -c 262144 /dev/zero; head -c 262144 /dev/zero >&2",
            ])
            .stdin(vec![b'x'; 2 * 1024 * 1024])
            .output_limits(512 * 1024, 512 * 1024)
            .timeout(Duration::from_secs(5));
        let failure = SystemProcessExecutor
            .execute(&command, &CancellationToken::new())
            .unwrap_err();
        assert_eq!(failure.kind, ProcessFailureKind::Io);
        assert_eq!(failure.exit_code, Some(0));
        assert!(failure.message.starts_with("stdin:"));
        assert_eq!(failure.stdout.len(), FAILURE_EXCERPT_LIMIT);
        assert_eq!(failure.stderr.len(), FAILURE_EXCERPT_LIMIT);
        assert!(failure.stdout.iter().all(|byte| *byte == 0));
        assert!(failure.stderr.iter().all(|byte| *byte == 0));
    }
}
