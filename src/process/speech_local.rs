//! Trusted local speech jobs reuse process-group identity and the sole-reaper
//! guard without weakening the ordinary remote-workspace executor contract.
//! No result/error type here logs microphone bytes or recognition output.
use super::*;
use std::{fs::File, os::unix::process::CommandExt};

type KeepAlive = Arc<dyn Send + Sync>;
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Failure {
    Cancelled,
    Limit,
    Timeout,
    Spawn,
    Io,
    Exit,
    CleanupPending,
}

pub(crate) fn capture(
    spec: &CommandSpec,
    stop: &CancellationToken,
    cancel: &CancellationToken,
    keep_alive: KeepAlive,
    on_pcm: impl FnMut(),
) -> Result<Vec<u8>, Failure> {
    let expected = [
        "--raw",
        "--rate",
        "16000",
        "--channels",
        "1",
        "--format",
        "s16",
        "-",
    ];
    if spec.program != Path::new("/usr/bin/pw-record")
        || spec.arguments.len() != expected.len()
        || spec
            .arguments
            .iter()
            .zip(expected)
            .any(|(argument, expected)| argument.as_os_str() != std::ffi::OsStr::new(expected))
    {
        return Err(Failure::Spawn);
    }
    if spec
        .timeout
        .is_none_or(|timeout| timeout > Duration::from_secs(60))
    {
        return Err(Failure::Timeout);
    }
    run(spec, Some(stop), cancel, keep_alive, on_pcm)
}
pub(crate) fn transcribe(
    spec: &CommandSpec,
    cancel: &CancellationToken,
    keep_alive: KeepAlive,
) -> Result<Vec<u8>, Failure> {
    let valid = spec
        .program
        .to_str()
        .and_then(|path| path.strip_prefix("/proc/self/fd/"))
        .and_then(|fd| fd.parse::<i32>().ok())
        .is_some_and(|fd| {
            spec.inherited_files
                .iter()
                .any(|file| file.0.as_raw_fd() == fd)
        });
    if !valid {
        return Err(Failure::Spawn);
    }
    if spec
        .timeout
        .is_none_or(|timeout| timeout > Duration::from_secs(45))
        || spec.stdout_limit > 64 * 1024
    {
        return Err(Failure::Limit);
    }
    run(spec, None, cancel, keep_alive, || {})
}
/// Only the provisioner constructs this fixed system tar invocation. Its input
/// is a privately owned, checksum-verified archive FD, never a shell command.
pub(crate) fn provision(
    spec: &CommandSpec,
    cancel: &CancellationToken,
    keep_alive: KeepAlive,
) -> Result<Vec<u8>, Failure> {
    if spec.program != Path::new("/usr/bin/tar") {
        return Err(Failure::Spawn);
    }
    run(spec, None, cancel, keep_alive, || {})
}

/// Only the two fixed public model/runtime curl commands may stream to disk.
/// stdout has a retained bounded file sink instead of a release-sized Vec.
pub(crate) fn download(
    spec: &CommandSpec,
    output: File,
    cancel: &CancellationToken,
    keep_alive: KeepAlive,
    progress: impl FnMut(u64),
) -> Result<(), Failure> {
    let archive = [
        crate::speech::model::RUNTIME,
        crate::speech::model::PARAKEET,
    ]
    .into_iter()
    .find(|archive| {
        spec.arguments.last().is_some_and(|url| url == archive.url)
            && spec.stdout_limit as u64 == archive.bytes
    })
    .ok_or(Failure::Spawn)?;
    let mut arguments: Vec<OsString> = [
        "-q",
        "--fail",
        "--silent",
        "--show-error",
        "--location",
        "--proto",
        "=https",
        "--proto-redir",
        "=https",
        "--max-redirs",
        "5",
        "--connect-timeout",
        "15",
        "--max-time",
        "900",
        "--max-filesize",
    ]
    .into_iter()
    .map(OsString::from)
    .collect();
    arguments.extend([
        archive.bytes.to_string().into(),
        "--url".into(),
        archive.url.into(),
    ]);
    if spec.program != Path::new("/usr/bin/curl")
        || spec.arguments != arguments
        || spec.timeout != Some(Duration::from_secs(900))
        || output
            .metadata()
            .map_or(true, |metadata| !metadata.is_file() || metadata.len() != 0)
    {
        return Err(Failure::Spawn);
    }
    run_with_sink(
        spec,
        None,
        cancel,
        keep_alive,
        || {},
        Some(output),
        progress,
    )
    .map(|_| ())
}

struct Owner {
    waiter: GroupWaiter,
    keep_alive: KeepAlive,
}
impl Owner {
    fn finish(&mut self, observed: bool) -> Result<(), Failure> {
        let failed = self.waiter.finish(observed).is_some();
        if !observed {
            self.retain_until_reaped();
            return Err(Failure::CleanupPending);
        }
        if failed { Err(Failure::Io) } else { Ok(()) }
    }
    fn retain_until_reaped(&mut self) {
        let Some(worker) = self.waiter.worker.take() else {
            return;
        };
        let keep_alive = self.keep_alive.clone();
        let fallback = keep_alive.clone();
        if thread::Builder::new()
            .name("speech-cleanup".into())
            .spawn(move || {
                let _ = worker.join();
                drop(keep_alive);
            })
            .is_err()
        {
            // If the cleanup owner cannot be created, fail closed: retaining
            // this one lease prevents another microphone from starting. The
            // existing sole reaper still owns/cleans the original child.
            std::mem::forget(fallback);
        }
    }
}
impl Drop for Owner {
    fn drop(&mut self) {
        if self.waiter.release.is_some() {
            self.waiter.signal(SIGKILL);
            self.waiter.finish(false);
            self.retain_until_reaped();
        }
    }
}
struct Capture<R> {
    pipe: Option<R>,
    bytes: Vec<u8>,
    file: Option<File>,
    count: usize,
    limit: usize,
    failed: Option<Failure>,
}
impl<R: Read + AsFd> Capture<R> {
    fn new(pipe: R, limit: usize) -> Self {
        Self {
            pipe: Some(pipe),
            bytes: Vec::new(),
            file: None,
            count: 0,
            limit,
            failed: None,
        }
    }
    fn fd(&self) -> i32 {
        self.pipe
            .as_ref()
            .map_or(-1, |pipe| pipe.as_fd().as_raw_fd())
    }
    fn drain(&mut self) {
        let Some(pipe) = &mut self.pipe else {
            return;
        };
        let mut buffer = [0u8; 8192];
        let mut turn = 0;
        while turn < 256 * 1024 {
            match pipe.read(&mut buffer) {
                Ok(0) => {
                    self.pipe.take();
                    break;
                }
                Ok(read) => {
                    turn += read;
                    let remaining = self.limit.saturating_sub(self.count);
                    let accepted = read.min(remaining);
                    if let Some(file) = &mut self.file {
                        if file.write_all(&buffer[..accepted]).is_err() {
                            self.failed = Some(Failure::Io);
                            self.pipe.take();
                            break;
                        }
                    } else {
                        self.bytes.extend_from_slice(&buffer[..accepted]);
                    }
                    self.count += accepted;
                    if read > remaining {
                        self.failed = Some(Failure::Limit);
                        self.pipe.take();
                        break;
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(_) => {
                    self.failed = Some(Failure::Io);
                    self.pipe.take();
                    break;
                }
            }
        }
    }
}
fn wake_descriptor() -> io::Result<Arc<OwnedFd>> {
    // SAFETY: eventfd returns one fresh close-on-exec descriptor, adopted once.
    let fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
    if fd < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(Arc::new(unsafe { OwnedFd::from_raw_fd(fd) }))
    }
}
fn drain_wake(wake: &OwnedFd) {
    let mut value = 0u64;
    // SAFETY: owned nonblocking eventfd and a correctly sized writable buffer.
    let _ = unsafe { libc::read(wake.as_raw_fd(), (&mut value as *mut u64).cast(), 8) };
}
fn parent_death(command: &mut Command, parent: u32) {
    // The fixed recorder/recognizer do not fork background helpers. The
    // provisioner's tar may fork its fixed decompressor; ordinary group teardown
    // covers it. PDEATHSIG covers direct children, not arbitrary descendant trees.
    // SAFETY: only async-signal-safe Linux syscalls run between fork and exec.
    unsafe {
        command.pre_exec(move || {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 {
                return Err(io::Error::last_os_error());
            }
            if libc::getppid() != parent as libc::pid_t {
                return Err(io::Error::from_raw_os_error(libc::ECHILD));
            }
            Ok(())
        });
    }
}

fn run(
    spec: &CommandSpec,
    stop: Option<&CancellationToken>,
    cancel: &CancellationToken,
    keep_alive: KeepAlive,
    on_pcm: impl FnMut(),
) -> Result<Vec<u8>, Failure> {
    run_with_sink(spec, stop, cancel, keep_alive, on_pcm, None, |_| {})
}
fn run_with_sink(
    spec: &CommandSpec,
    stop: Option<&CancellationToken>,
    cancel: &CancellationToken,
    keep_alive: KeepAlive,
    mut on_pcm: impl FnMut(),
    file: Option<File>,
    mut progress: impl FnMut(u64),
) -> Result<Vec<u8>, Failure> {
    let download = file.is_some();
    let timeout = spec
        .timeout
        .filter(|timeout| {
            !timeout.is_zero() && *timeout <= Duration::from_secs(if download { 900 } else { 120 })
        })
        .ok_or(Failure::Timeout)?;
    let max_stdout = if download {
        512 * 1024 * 1024
    } else if stop.is_some() {
        1_920_000
    } else {
        128 * 1024
    };
    if spec.stdout_limit == 0
        || spec.stdout_limit > max_stdout
        || spec.stderr_limit > 16 * 1024
        || spec.stdin.is_some()
        || spec.current_dir.is_some()
    {
        return Err(Failure::Limit);
    }
    if cancel.is_cancelled() {
        return Err(Failure::Cancelled);
    }
    let wake = wake_descriptor().map_err(|_| Failure::Io)?;
    let _cancel_registration =
        cancel.register_notifier(CancellationNotifier::Descriptor(wake.clone()));
    let _stop_registration =
        stop.map(|stop| stop.register_notifier(CancellationNotifier::Descriptor(wake.clone())));
    let mut command = spec.to_command();
    parent_death(&mut command, std::process::id());
    let mut child = command.spawn().map_err(|_| Failure::Spawn)?;
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let identity = child_identity(&child);
    let mut owner = Owner {
        waiter: GroupWaiter::new(child, identity),
        keep_alive,
    };
    let stdout = stdout.ok_or(Failure::Io)?;
    let stderr = stderr.ok_or(Failure::Io)?;
    set_nonblocking(stdout.as_fd()).map_err(|_| Failure::Io)?;
    set_nonblocking(stderr.as_fd()).map_err(|_| Failure::Io)?;
    if owner.waiter.identity.is_none() {
        return Err(Failure::Spawn);
    }
    let mut stdout = Capture::new(stdout, spec.stdout_limit);
    stdout.file = file;
    let mut stderr = Capture::new(stderr, spec.stderr_limit);
    let deadline = Instant::now() + timeout;
    let mut failure = None;
    let mut stopped = false;
    let mut recording = false;
    let mut terminating = false;
    let mut term_deadline = None;
    let mut cleanup_deadline = None;
    let mut status = None;
    loop {
        let now = Instant::now();
        if cancel.is_cancelled() {
            failure = Some(Failure::Cancelled);
        }
        if let Some(error) = stdout.failed.or(stderr.failed) {
            failure.get_or_insert(error);
        }
        if now >= deadline {
            failure.get_or_insert(Failure::Timeout);
        }
        stopped |= stop.is_some_and(CancellationToken::is_cancelled);
        if (failure.is_some() || stopped) && !terminating {
            terminating = true;
            if owner.waiter.signal(SIGTERM) {
                term_deadline = Some(now + TERMINATION_GRACE);
            } else {
                cleanup_deadline = Some(now + CLEANUP_GRACE);
            }
        }
        if term_deadline.is_some_and(|at| now >= at || status.is_some()) {
            owner.waiter.signal(SIGKILL);
            term_deadline = None;
            cleanup_deadline = Some(now + CLEANUP_GRACE);
        }
        if status.is_some()
            && stdout.pipe.is_none()
            && stderr.pipe.is_none()
            && term_deadline.is_none()
        {
            break;
        }
        if cleanup_deadline.is_some_and(|at| now >= at) {
            break;
        }
        let mut watched = [
            libc::pollfd {
                fd: stdout.fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: stderr.fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: if status.is_none() {
                    owner.waiter.identity.as_ref().unwrap().as_raw_fd()
                } else {
                    -1
                },
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: wake.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        let next = term_deadline.or(cleanup_deadline).map_or(deadline, |at| {
            if now < deadline { at.min(deadline) } else { at }
        });
        let millis = next
            .saturating_duration_since(Instant::now())
            .as_millis()
            .saturating_add(1)
            .min(i32::MAX as u128) as i32;
        // SAFETY: descriptors remain owned throughout this readiness wait.
        let ready =
            unsafe { libc::poll(watched.as_mut_ptr(), watched.len() as libc::nfds_t, millis) };
        if ready < 0 {
            if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                continue;
            }
            failure.get_or_insert(Failure::Io);
            owner.waiter.signal(SIGKILL);
            break;
        }
        if watched[0].revents != 0 {
            let before = stdout.count;
            stdout.drain();
            if download && before != stdout.count {
                progress(stdout.count as u64);
            }
        }
        if watched[1].revents != 0 {
            stderr.drain();
        }
        if !recording && !stdout.bytes.is_empty() && stop.is_some() {
            recording = true;
            on_pcm();
        }
        if watched[2].revents != 0 {
            match observe_child_exit(
                owner.waiter.identity.as_ref().unwrap(),
                owner.waiter.group,
                true,
            ) {
                Ok(exit) => status = exit,
                Err(_) => {
                    failure.get_or_insert(Failure::Io);
                    owner.waiter.signal(SIGKILL);
                    break;
                }
            }
        }
        if watched[3].revents != 0 {
            drain_wake(&wake);
        }
    }
    if let Some(error) = stdout.failed.or(stderr.failed) {
        failure.get_or_insert(error);
    }
    // A held-open stream after bounded cleanup cannot be promoted to a
    // complete recording, even when the group leader itself has exited.
    if stdout.pipe.is_some() || stderr.pipe.is_some() {
        failure.get_or_insert(Failure::Io);
    }
    if let Err(error) = owner.finish(status.is_some()) {
        return Err(error);
    }
    if cancel.is_cancelled() {
        return Err(Failure::Cancelled);
    }
    if let Some(error) = failure {
        return Err(error);
    }
    if stop.is_some()
        && (!stopped
            || !status.as_ref().is_some_and(|status| {
                status.success()
                    || (terminating && matches!(exit_signal(status), Some(SIGTERM | SIGKILL)))
            }))
    {
        return Err(Failure::Exit);
    }
    if stop.is_none() && !status.is_some_and(|status: ExitStatus| status.success()) {
        return Err(Failure::Exit);
    }
    Ok(stdout.bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn producer(script: &str) -> CommandSpec {
        CommandSpec::new("/usr/bin/python3")
            .args(["-c", script])
            .timeout(Duration::from_secs(3))
            .output_limits(1_920_000, 16 * 1024)
    }
    #[test]
    fn download_sink_streams_beyond_memory_excerpt_with_exact_monotonic_progress() {
        let mut output = tempfile::tempfile().unwrap();
        let spec = producer("import os;os.write(1,b'x'*2097152)")
            .output_limits(2 * 1024 * 1024, 16 * 1024);
        let mut progress = Vec::new();
        let bytes = run_with_sink(
            &spec,
            None,
            &CancellationToken::new(),
            Arc::new(()),
            || {},
            Some(output.try_clone().unwrap()),
            |bytes| progress.push(bytes),
        )
        .unwrap();
        assert!(
            bytes.is_empty(),
            "download bytes must never occupy the stdout Vec"
        );
        assert_eq!(output.metadata().unwrap().len(), 2 * 1024 * 1024);
        assert_eq!(progress.last(), Some(&(2 * 1024 * 1024)));
        assert!(progress.windows(2).all(|pair| pair[0] < pair[1]));
        use std::io::{Seek, SeekFrom};
        output.seek(SeekFrom::Start(0)).unwrap();
        let mut excerpt = [0; 32];
        output.read_exact(&mut excerpt).unwrap();
        assert_eq!(excerpt, [b'x'; 32]);
    }
    #[test]
    fn download_sink_cap_and_cancel_cannot_promote_partial_archive() {
        for cancel_after_first in [false, true] {
            let output = tempfile::tempfile().unwrap();
            let cancellation = CancellationToken::new();
            let spec = producer("import os,time;os.write(1,b'x'*262144);time.sleep(30)")
                .output_limits(128 * 1024, 16 * 1024);
            let result = run_with_sink(
                &spec,
                None,
                &cancellation,
                Arc::new(()),
                || {},
                Some(output.try_clone().unwrap()),
                |_| {
                    if cancel_after_first {
                        cancellation.cancel();
                    }
                },
            );
            assert_eq!(
                result.err(),
                Some(if cancel_after_first {
                    Failure::Cancelled
                } else {
                    Failure::Limit
                })
            );
            assert!(output.metadata().unwrap().len() <= 128 * 1024);
        }
    }

    #[test]
    fn creator_thread_death_kills_the_direct_speech_child() {
        let mut child = thread::spawn(|| {
            let mut command = producer("import time;time.sleep(30)").to_command();
            parent_death(&mut command, std::process::id());
            command.spawn().unwrap()
        })
        .join()
        .unwrap();
        let status = child.wait().unwrap();
        assert_eq!(exit_signal(&status), Some(SIGKILL));
    }
    #[test]
    fn parent_death_setup_rejects_a_parent_that_changed_before_exec() {
        let mut command = producer("import time;time.sleep(30)").to_command();
        parent_death(&mut command, std::process::id().wrapping_add(1));
        assert!(command.spawn().is_err());
    }

    #[test]
    fn intentional_stop_retains_pcm_beyond_the_generic_failure_excerpt() {
        let stop = CancellationToken::new();
        let cancel = CancellationToken::new();
        let directory = tempfile::tempdir().unwrap();
        let ready = directory.path().join("all-written");
        let command = producer("import os,time\nos.write(1,b'\\x12\\x34'*65536)\nopen(os.environ['READY_FILE'],'w').close()\ntime.sleep(30)")
            .env("READY_FILE", ready.as_os_str());
        let stopping = {
            let stop = stop.clone();
            thread::spawn(move || {
                let deadline = Instant::now() + Duration::from_secs(2);
                while !ready.exists() && Instant::now() < deadline {
                    thread::sleep(Duration::from_millis(2));
                }
                assert!(ready.exists(), "producer finished its complete test clip");
                stop.cancel();
            })
        };
        let mut progress = 0;
        let bytes = run(&command, Some(&stop), &cancel, Arc::new(()), || {
            progress += 1;
        });
        stopping.join().unwrap();
        assert_eq!(bytes.unwrap(), vec![0x12, 0x34].repeat(65536));
        assert_eq!(progress, 1);
    }
    #[test]
    fn cancel_remains_observable_after_stop_begins_term_cleanup() {
        let stop = CancellationToken::new();
        let cancel = CancellationToken::new();
        let command = producer(
            "import os,time,signal\nsignal.signal(signal.SIGTERM,lambda a,b:None)\nos.write(1,b'\\0\\1'*65536)\ntime.sleep(30)",
        );
        let mut cancelling = None;
        let result = run(&command, Some(&stop), &cancel, Arc::new(()), || {
            stop.cancel();
            let cancel = cancel.clone();
            cancelling = Some(thread::spawn(move || {
                thread::sleep(Duration::from_millis(30));
                cancel.cancel();
            }));
        });
        cancelling.unwrap().join().unwrap();
        assert!(matches!(result, Err(Failure::Cancelled)));
    }
    #[test]
    fn stop_does_not_hide_capture_output_limits() {
        let stop = CancellationToken::new();
        let cancel = CancellationToken::new();
        let command = producer("import os,time\nos.write(1,b'x'*8192)\ntime.sleep(30)")
            .output_limits(4096, 16 * 1024);
        assert!(matches!(
            run(&command, Some(&stop), &cancel, Arc::new(()), || stop
                .cancel()),
            Err(Failure::Limit)
        ));
    }
    #[test]
    fn stop_never_promotes_an_already_failed_recorder() {
        let directory = tempfile::tempdir().unwrap();
        let pid_file = directory.path().join("pid");
        let command = producer("import os\nopen(os.environ['PID_FILE'],'w').write(str(os.getpid()))\nos.write(1,b'x'*1024)\nos._exit(7)")
            .env("PID_FILE", pid_file.as_os_str());
        let stop = CancellationToken::new();
        let cancel = CancellationToken::new();
        let outcome = run(&command, Some(&stop), &cancel, Arc::new(()), || {
            let pid: u32 = std::fs::read_to_string(&pid_file).unwrap().parse().unwrap();
            // Observe the original child exit before Stop, so this regression
            // exercises a failed recorder rather than a timing-dependent kill.
            let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) as i32 };
            assert!(fd >= 0);
            let fd = unsafe { OwnedFd::from_raw_fd(fd) };
            let mut watch = libc::pollfd {
                fd: fd.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            assert_eq!(unsafe { libc::poll(&mut watch, 1, 1000) }, 1);
            stop.cancel();
        });
        assert!(matches!(outcome, Err(Failure::Exit)));
    }
    #[test]
    fn held_open_descendant_pipe_cannot_be_returned_as_a_complete_clip() {
        struct Escaped(Option<OwnedFd>);
        impl Drop for Escaped {
            fn drop(&mut self) {
                if let Some(fd) = &self.0 {
                    // The fixture owns this original pidfd, not a reusable PID.
                    unsafe {
                        libc::syscall(
                            libc::SYS_pidfd_send_signal,
                            fd.as_raw_fd(),
                            SIGKILL,
                            std::ptr::null::<libc::siginfo_t>(),
                            0,
                        );
                    }
                }
            }
        }
        let directory = tempfile::tempdir().unwrap();
        let pid_file = directory.path().join("escaped");
        let script = r#"import os,time
pid=os.fork()
if pid==0:
 os.setsid()
 with open(os.environ['PID_FILE']+'.tmp','w') as f: f.write(str(os.getpid()))
 os.replace(os.environ['PID_FILE']+'.tmp',os.environ['PID_FILE'])
 time.sleep(5)
 os._exit(0)
else:
 while not os.path.exists(os.environ['PID_FILE']): time.sleep(0.005)
 os.write(1,b'x'*131072)
 time.sleep(30)
"#;
        let command = producer(script).env("PID_FILE", pid_file.as_os_str());
        let stop = CancellationToken::new();
        let cancel = CancellationToken::new();
        let mut escaped = Escaped(None);
        let outcome = run(&command, Some(&stop), &cancel, Arc::new(()), || {
            let pid: u32 = std::fs::read_to_string(&pid_file).unwrap().parse().unwrap();
            let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) as i32 };
            assert!(fd >= 0);
            escaped.0 = Some(unsafe { OwnedFd::from_raw_fd(fd) });
            stop.cancel();
        });
        drop(escaped);
        assert!(matches!(outcome, Err(Failure::Io)));
    }

    #[test]
    fn trusted_entry_points_reject_ambient_helpers_and_unbounded_capture() {
        let stop = CancellationToken::new();
        let cancel = CancellationToken::new();
        assert!(matches!(
            transcribe(&CommandSpec::new("whisper"), &cancel, Arc::new(())),
            Err(Failure::Spawn)
        ));
        let mut spec = CommandSpec::new("/usr/bin/pw-record").args([
            "--raw",
            "--rate",
            "16000",
            "--channels",
            "1",
            "--format",
            "s16",
            "-",
        ]);
        spec.timeout = None;
        assert!(matches!(
            capture(&spec, &stop, &cancel, Arc::new(()), || {}),
            Err(Failure::Timeout)
        ));
    }
}
