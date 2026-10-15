//! Private, bounded original-child harness for the concrete-backend contract.
use std::{fs::{File, OpenOptions}, io::{self, Read}, os::{fd::{AsRawFd, FromRawFd}, unix::{fs::{OpenOptionsExt, PermissionsExt}, process::CommandExt}}, process::{Child, Command, ExitStatus, Stdio}, time::{Duration, Instant}};
const CHILD: &str = "CAGENTS_BACKEND_BINDING_RESTART_FIXTURE";
const LOG_LIMIT: u64 = 4 * 1024 * 1024;
struct Evidence(Option<tempfile::TempDir>);
impl Drop for Evidence {
    fn drop(&mut self) { if let Some(directory) = self.0.take() { eprintln!("backend binding fixture evidence retained at {}", directory.keep().display()); } }
}
struct Original {
    child: Option<Child>, pidfd: Option<File>,
    reservation: Option<crate::daemon::child_reaper::Reservation>,
}
impl Original {
    fn status(&mut self) -> io::Result<Option<ExitStatus>> {
        let status = self.child.as_mut().expect("original child").try_wait()?;
        if status.is_some() { self.child.take(); self.pidfd.take(); self.reservation.take(); }
        Ok(status)
    }
}
impl Drop for Original {
    fn drop(&mut self) {
        let Some(mut child) = self.child.take() else { return; };
        let reservation = self.reservation.take().expect("pre-spawn credit");
        reservation.quarantine();
        if let Some(fd) = &self.pidfd {
            unsafe { libc::syscall(libc::SYS_pidfd_send_signal, fd.as_raw_fd(), libc::SIGKILL, std::ptr::null::<libc::siginfo_t>(), 0); }
        } else { let _ = child.kill(); } // still exclusively owns its unreaped PID
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if matches!(child.try_wait(), Ok(Some(_))) { return; }
            if Instant::now() >= deadline { break; }
            std::thread::sleep(Duration::from_millis(5));
        }
        match self.pidfd.take() {
            Some(pidfd) => if let Err((error, child, pidfd, reservation)) = reservation.retain(child, pidfd) {
                reservation.preserve_suspended(child, Some(pidfd), &format!("backend fixture: {error}"));
            },
            None => reservation.preserve_suspended(child, None, "backend fixture original pidfd capture failed"),
        }
    }
}
/// Returns true in the parent after exactly one named contract actually passed.
/// Failure/timeout preserves private evidence and original child ownership.
pub(super) fn isolated(exact: &str) -> bool {
    if std::env::var(CHILD).as_deref() == Ok(exact) { return false; }
    let mut evidence = Evidence(Some(tempfile::Builder::new().prefix("cagents-backend-binding-").tempdir_in("/var/tmp").unwrap()));
    let root = evidence.0.as_ref().unwrap().path();
    for name in ["home", "run", "data", "config", "cache"] {
        std::fs::create_dir(root.join(name)).unwrap();
        std::fs::set_permissions(root.join(name), std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let log = |name| OpenOptions::new().write(true).create_new(true).mode(0o600).custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC).open(root.join(name)).unwrap();
    let mut command = Command::new(std::env::current_exe().unwrap());
    command.args(["--exact", exact, "--nocapture", "--test-threads=1"]).env_clear().env(CHILD, exact)
        .env("HOME", root.join("home")).env("XDG_RUNTIME_DIR", root.join("run"))
        .env("XDG_DATA_HOME", root.join("data")).env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_CACHE_HOME", root.join("cache")).env("PATH", "/usr/bin:/bin")
        .stdin(Stdio::null()).stdout(Stdio::from(log("stdout.log"))).stderr(Stdio::from(log("stderr.log")));
    // Only async-signal-safe scalar syscalls execute after fork.
    unsafe { command.pre_exec(|| {
        let cap = libc::rlimit { rlim_cur: LOG_LIMIT, rlim_max: LOG_LIMIT };
        let core = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
        if libc::setrlimit(libc::RLIMIT_FSIZE, &cap) != 0 || libc::setrlimit(libc::RLIMIT_CORE, &core) != 0 { return Err(io::Error::last_os_error()); }
        Ok(())
    }); }
    let reservation = crate::daemon::child_reaper::reserve().unwrap();
    let child = command.spawn().unwrap();
    let mut original = Original { child: Some(child), pidfd: None, reservation: Some(reservation) };
    let pid = original.child.as_ref().unwrap().id();
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
    assert!(fd >= 0, "cannot pin original backend fixture child");
    original.pidfd = Some(unsafe { File::from_raw_fd(fd as i32) });
    std::fs::write(root.join("startup.json"), serde_json::to_vec_pretty(&serde_json::json!({"test":exact,"pid":pid,"authority":"pre_reserved_original_child_and_pidfd","deadline_seconds":40,"log_limit_bytes":LOG_LIMIT})).unwrap()).unwrap();
    let deadline = Instant::now() + Duration::from_secs(40);
    let status = loop {
        if let Some(status) = original.status().unwrap() { break status; }
        assert!(Instant::now() < deadline, "backend fixture deadline exceeded");
        std::thread::sleep(Duration::from_millis(10));
    };
    let read = |name| {
        let mut bytes = Vec::new(); File::open(root.join(name)).unwrap().take(LOG_LIMIT + 1).read_to_end(&mut bytes).unwrap();
        assert!(bytes.len() as u64 <= LOG_LIMIT, "fixture log exceeded cap");
        String::from_utf8_lossy(&bytes).into_owned()
    };
    let stdout = read("stdout.log"); let stderr = read("stderr.log");
    let executed = stdout.lines().any(|line| line == "running 1 test")
        && stdout.lines().any(|line| line == format!("test {exact} ... ok"))
        && stdout.lines().any(|line| line.starts_with("test result: ok. 1 passed; 0 failed; 0 ignored;"));
    assert!(status.success() && executed, "backend fixture did not prove the named contract ({status}); stdout={} stderr={}", stdout.chars().take(4096).collect::<String>(), stderr.chars().take(4096).collect::<String>());
    drop(evidence.0.take());
    true
}
pub(super) struct Completed(pub std::sync::mpsc::Sender<()>);
impl Drop for Completed { fn drop(&mut self) { let _ = self.0.send(()); } }
