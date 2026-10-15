//! Private-XDG exact-child harness; original Child status is not family proof.
use std::{fs,io,thread,fs::File,os::fd::{AsRawFd,FromRawFd},os::unix::fs::{OpenOptionsExt,PermissionsExt},path::Path,process::{Child,Command,Stdio},time::{Duration,Instant}};
const ROOT: &str = "CAGENTS_STOP_AUTHORITY_FIXTURE_ROOT";
struct TestChild {
    child: Option<Child>,
    pin: Option<File>,
    reservation: Option<crate::daemon::child_reaper::Reservation>,
    signal_allowed: bool,
}
impl TestChild {
    fn spawn(command: &mut Command) -> io::Result<Self> {
        let reservation = crate::daemon::child_reaper::reserve()?;
        let child = command.spawn()?;
        let mut owner = Self {
            child: Some(child),
            pin: None,
            reservation: Some(reservation),
            signal_allowed: false,
        };
        let pid = owner.child.as_ref().unwrap().id() as i32;
        let before = crate::daemon::holder_registry::Process::read(pid)?;
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        owner.pin = Some(unsafe { File::from_raw_fd(fd as i32) });
        if crate::daemon::holder_registry::Process::read(pid)? != before {
            return Err(io::Error::other(
                "fixture child changed before pidfd capture",
            ));
        }
        // A retained original Child plus a live kernel-child pidfd establishes
        // authority; no late numeric PID is used for cancellation or waiting.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        if unsafe {
            libc::waitid(
                libc::P_PIDFD,
                fd as libc::id_t,
                &raw mut info,
                libc::WEXITED | libc::WNOWAIT | libc::WNOHANG,
            )
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
        owner.signal_allowed = true;
        Ok(owner)
    }
    fn status(&mut self) -> io::Result<Option<std::process::ExitStatus>> {
        let status = self
            .child
            .as_mut()
            .expect("original fixture child is owned")
            .try_wait()?;
        if status.is_some() {
            self.child.take();
            self.pin.take();
            self.reservation.take();
        }
        Ok(status)
    }
}
impl Drop for TestChild {
    fn drop(&mut self) {
        let Some(mut child) = self.child.take() else {
            return;
        };
        if matches!(child.try_wait(), Ok(Some(_))) {
            return;
        }
        let reservation = self.reservation.take().expect("reserved before spawn");
        let Some(pin) = self.pin.take() else {
            reservation.preserve_suspended(child, None, "fixture pidfd capture failed");
            return;
        };
        if !self.signal_allowed {
            reservation.preserve_suspended(
                child,
                Some(pin),
                "fixture pidfd kernel-child verification failed",
            );
            return;
        }
        unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                pin.as_raw_fd(),
                libc::SIGKILL,
                std::ptr::null::<libc::siginfo_t>(),
                0,
            );
        }
        // Never block the test thread in wait(). Failed transfer retains every
        // original object in its preallocated suspended slot.
        if let Err((error, child, pin, reservation)) = reservation.retain(child, pin) {
            reservation.preserve_suspended(child, Some(pin), &error.to_string());
        }
    }
}

struct FixtureDirectory(Option<tempfile::TempDir>);
impl Drop for FixtureDirectory {
    fn drop(&mut self) {
        if let Some(directory) = self.0.take() {
            let _ = directory.keep();
        }
    }
}
const LOG_LIMIT: usize = 1024 * 1024;
struct Captured<R> {
    pipe: R,
    log: File,
    bytes: Vec<u8>,
    truncated: bool,
}
impl<R: io::Read + AsRawFd> Captured<R> {
    fn new(pipe: R, path: &Path) -> io::Result<Self> {
        let flags = unsafe { libc::fcntl(pipe.as_raw_fd(), libc::F_GETFL) };
        if flags < 0
            || unsafe { libc::fcntl(pipe.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0
        {
            return Err(io::Error::last_os_error());
        }
        let log = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
            .open(path)?;
        Ok(Self {
            pipe,
            log,
            bytes: Vec::new(),
            truncated: false,
        })
    }
    fn drain(&mut self) -> io::Result<()> {
        use io::Write;
        let mut chunk = [0u8; 8192];
        // Output cannot monopolize the parent deadline. Excess is consumed and
        // discarded without increasing disk or memory residency.
        for _ in 0..16 {
            match self.pipe.read(&mut chunk) {
                Ok(0) => break,
                Ok(count) => {
                    let kept = count.min(LOG_LIMIT - self.bytes.len());
                    self.log.write_all(&chunk[..kept])?;
                    self.bytes.extend_from_slice(&chunk[..kept]);
                    self.truncated |= kept != count;
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }
    fn text(&self) -> std::borrow::Cow<'_, str> {
        String::from_utf8_lossy(&self.bytes)
    }
}
pub(super) fn enter(test: &str) -> Option<std::path::PathBuf> {
    let Some(root) = std::env::var_os(ROOT) else {
        let directory = FixtureDirectory(Some(tempfile::tempdir_in("/var/tmp").unwrap()));
        let root = directory.0.as_ref().unwrap().path().canonicalize().unwrap();
        for name in ["home", "run", "data", "config", "cache"] {
            fs::create_dir(root.join(name)).unwrap();
            fs::set_permissions(root.join(name), fs::Permissions::from_mode(0o700)).unwrap();
        }
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut child = TestChild::spawn(
            Command::new(std::env::current_exe().unwrap())
                .args(["--exact", test, "--test-threads=1", "--color=never"])
                .env_clear()
                .env(ROOT, &root)
                .env("HOME", root.join("home"))
                .env("XDG_RUNTIME_DIR", root.join("run"))
                .env("XDG_DATA_HOME", root.join("data"))
                .env("XDG_CONFIG_HOME", root.join("config"))
                .env("XDG_CACHE_HOME", root.join("cache"))
                .env("PATH", "/usr/bin:/bin")
                .stdout(Stdio::piped())
                .stderr(Stdio::piped()),
        )
        .unwrap();
        let mut stdout = Captured::new(
            child.child.as_mut().unwrap().stdout.take().unwrap(),
            &root.join("test.stdout"),
        )
        .unwrap();
        let mut stderr = Captured::new(
            child.child.as_mut().unwrap().stderr.take().unwrap(),
            &root.join("test.stderr"),
        )
        .unwrap();
        let status = loop {
            stdout.drain().unwrap();
            stderr.drain().unwrap();
            if let Some(status) = child.status().unwrap() {
                break Some(status);
            }
            if Instant::now() >= deadline {
                break None;
            }
            thread::sleep(
                deadline
                    .saturating_duration_since(Instant::now())
                    .min(Duration::from_millis(5)),
            );
        };
        // If timed out, Drop signals only the prevalidated original pidfd and
        // transfers the original Child into its already-reserved reaper slot.
        // No blocking wait or unbounded reader-thread join follows a deadline.
        drop(child);
        stdout.drain().unwrap();
        stderr.drain().unwrap();
        let output = stdout.text();
        assert!(
            status.is_some_and(|status| status.success()),
            "private Stop authority fixture failed or exceeded30s; retained {}:\n{}\n{}",
            root.display(),
            output,
            stderr.text()
        );
        assert!(
            !stdout.truncated && !stderr.truncated,
            "fixture output exceeded bounded logs at {}",
            root.display()
        );
        assert_eq!(
            output
                .lines()
                .filter(|line| *line == "running 1 test")
                .count(),
            1,
            "{output}"
        );
        let passed = format!("test {test} ... ok");
        assert_eq!(
            output.lines().filter(|line| *line == passed).count(),
            1,
            "{output}"
        );
        assert_eq!(
            output
                .lines()
                .filter(|line| line
                    .starts_with("test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured;"))
                .count(),
            1,
            "{output}"
        );
        // Keep the bounded private artifacts until the external strict wrapper
        // independently proves whole-family settlement; Child exit alone does
        // not establish that every fixture descendant has finished.
        drop(directory);
        return None;
    };
    Some(std::path::PathBuf::from(root).canonicalize().unwrap())
}
