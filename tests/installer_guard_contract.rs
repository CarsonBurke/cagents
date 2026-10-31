#![cfg(target_os = "linux")]

#[path = "support/user_fixture.rs"]
mod user_fixture;

use std::{
    fs::{self, File, OpenOptions},
    io::{BufRead, BufReader, Write},
    os::{
        fd::AsRawFd,
        unix::{
            fs::{OpenOptionsExt, PermissionsExt},
            net::UnixListener,
            process::CommandExt,
        },
    },
    path::Path,
    process::{Command, Output, Stdio},
    thread,
    time::{Duration, Instant},
};

// These fixtures copy executables and fork with pre_exec hooks. A sibling fork
// can briefly inherit a copy's writable FD before CLOEXEC, causing ETXTBSY in
// another fixture. Serialize the complete OS fixture, not product execution.
static OS_FIXTURE: std::sync::Mutex<()> = std::sync::Mutex::new(());

// This module is confined to the two coordinator fixtures. Its reexec owns no
// parent-side fake windows or socket servers, and never changes caller authority.
mod coordinator_fixture {
    use super::*;
    use std::{
        io::{self, Read},
        os::{
            fd::FromRawFd,
            unix::{
                ffi::{OsStrExt, OsStringExt},
                net::UnixStream,
                process::ExitStatusExt,
            },
        },
        path::PathBuf,
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
    };

    const SPEC: &str = "CAGENTS_TEST_INSTALLER_CUSTODY";
    const GUARDIAN: &str = "coordinator_fixture::guardian";
    #[derive(serde::Serialize, serde::Deserialize)]
    struct Spec {
        program: Vec<u8>,
        arguments: Vec<Vec<u8>>,
        owner: i32,
        ignored_sigchld: bool,
    }
    #[derive(serde::Serialize, serde::Deserialize)]
    struct Receipt {
        guardian: u32,
        coordinator: u32,
        status: i32,
        cancelled: bool,
        family_echild: bool,
        forced: bool,
    }

    pub(super) struct Root {
        directory: Option<tempfile::TempDir>,
        settled: Arc<AtomicBool>,
    }
    impl Root {
        pub(super) fn new(mut directory: tempfile::TempDir) -> Self {
            directory.disable_cleanup(true);
            Self {
                directory: Some(directory),
                settled: Arc::new(AtomicBool::new(false)),
            }
        }
        pub(super) fn path(&self) -> &Path {
            self.directory.as_ref().unwrap().path()
        }
        // Call only after the original parent server/window assertions and
        // retirement. The guardian owns neither of those parent-side fixtures.
        pub(super) fn close(mut self) {
            assert!(
                self.settled.load(Ordering::Acquire),
                "unproven installer family"
            );
            let directory = self.directory.take().unwrap();
            let path = directory.path().to_owned();
            directory.close().unwrap_or_else(|error| {
                panic!(
                    "removing settled fixture {}; remaining evidence retained: {error}",
                    path.display()
                )
            });
        }
        pub(super) fn close_after_parent(
            self,
            retire: impl FnOnce() -> io::Result<()>,
        ) -> io::Result<()> {
            retire()?;
            self.close();
            Ok(())
        }
    }
    impl Drop for Root {
        fn drop(&mut self) {
            if self.directory.is_some() {
                eprintln!("Retained installer fixture: {}", self.path().display());
            }
        }
    }

    // EOF revokes ownership. A distinct acknowledged TERM request exercises
    // product cancellation without substituting fixture deadline cleanup.
    fn owner_event(owner: &mut UnixStream) -> io::Result<u8> {
        let mut byte = [0];
        loop {
            match owner.read(&mut byte) {
                Ok(0) => return Ok(0),
                Ok(1) if byte[0] == b'T' => return Ok(b'T'),
                Ok(_) => return Err(io::Error::other("unexpected owner control data")),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(b'.'),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => return Ok(b'.'),
                Err(e) => return Err(e),
            }
        }
    }

    fn read(root: &Path, name: &str) -> io::Result<Vec<u8>> {
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(root.join(name))?;
        if !file.metadata()?.is_file() {
            return Err(io::Error::other("nonregular custody evidence"));
        }
        let mut bytes = Vec::new();
        file.take(64 * 1024 + 1).read_to_end(&mut bytes)?;
        if bytes.len() > 64 * 1024 {
            return Err(io::Error::other("oversized custody evidence"));
        }
        Ok(bytes)
    }

    fn capture(path: &Path) -> File {
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
            .unwrap()
    }

    // A sole-reaper guard used ONLY inside the dedicated guardian process.
    struct Family {
        child: std::process::Child,
        reaped: bool,
        settled: bool,
        forced: bool,
    }
    impl Family {
        fn retire(&mut self, force: bool) -> io::Result<()> {
            if self.settled {
                return Ok(());
            }
            if !self.reaped {
                self.forced = true;
                self.child.kill()?;
                self.child.wait()?;
                self.reaped = true;
            }
            let deadline = Instant::now() + Duration::from_secs(5);
            let natural = Instant::now() + Duration::from_secs(2);
            loop {
                if Instant::now() >= deadline {
                    return Err(io::Error::other("installer family settlement timed out"));
                }
                let waited = unsafe { libc::waitpid(-1, std::ptr::null_mut(), libc::WNOHANG) };
                if waited < 0 {
                    match io::Error::last_os_error().raw_os_error() {
                        Some(libc::ECHILD) => {
                            self.settled = true;
                            return Ok(());
                        }
                        Some(libc::EINTR) => continue,
                        _ => return Err(io::Error::last_os_error()),
                    }
                }
                if waited > 0 {
                    continue;
                }
                if !force && Instant::now() < natural {
                    thread::sleep(Duration::from_millis(5));
                    continue;
                }
                // Forced retirement establishes custody, never a product pass.
                self.forced = true;
                // No other thread/handler reaps here. Every enumerated PID is
                // our direct child and remains unreaped through its final signal.
                for task in fs::read_dir("/proc/self/task")? {
                    let children = match fs::read_to_string(task?.path().join("children")) {
                        Ok(v) => v,
                        Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                        Err(e) => return Err(e),
                    };
                    for pid in children.split_whitespace() {
                        let pid: i32 = pid.parse().map_err(io::Error::other)?;
                        if unsafe { libc::kill(pid, libc::SIGKILL) } != 0
                            && io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
                        {
                            return Err(io::Error::last_os_error());
                        }
                    }
                }
                thread::sleep(Duration::from_millis(5));
            }
        }
    }
    impl Drop for Family {
        fn drop(&mut self) {
            if let Err(error) = self.retire(true) {
                eprintln!("Unsettled installer fixture family: {error}");
            }
        }
    }

    pub(super) struct Coordinator {
        child: std::process::Child,
        owner: UnixStream,
        root: PathBuf,
        reaped: bool,
        settled: Arc<AtomicBool>,
    }
    impl Coordinator {
        pub(super) fn start(command: Command, namespace: &Root, ignored_sigchld: bool) -> Self {
            assert!(
                !namespace.settled.load(Ordering::Acquire),
                "fixture root already used"
            );
            let root = namespace.path();
            let (owner, remote) = UnixStream::pair().unwrap();
            let spec = Spec {
                program: command.get_program().as_bytes().to_vec(),
                arguments: command.get_args().map(|v| v.as_bytes().to_vec()).collect(),
                owner: remote.as_raw_fd(),
                ignored_sigchld,
            };
            let mut guardian = Command::new(std::env::current_exe().unwrap());
            for (name, value) in command.get_envs() {
                if let Some(value) = value {
                    guardian.env(name, value);
                } else {
                    guardian.env_remove(name);
                }
            }
            if let Some(cwd) = command.get_current_dir() {
                guardian.current_dir(cwd);
            }
            guardian
                .args([
                    "--exact",
                    GUARDIAN,
                    "--ignored",
                    "--nocapture",
                    "--test-threads=1",
                ])
                .env(SPEC, serde_json::to_string(&spec).unwrap())
                .env("CAGENTS_TEST_CUSTODY_ROOT", root)
                .stdin(Stdio::inherit())
                .stdout(capture(&root.join("guardian.stdout")))
                .stderr(capture(&root.join("guardian.stderr")));
            let fd = remote.as_raw_fd();
            // Only the owned control FD crosses this exec; parent authority is
            // inherited unchanged, never serialized into the specification.
            unsafe {
                guardian.pre_exec(move || {
                    if libc::fcntl(fd, libc::F_SETFD, 0) == -1 {
                        return Err(io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
            let child = guardian.spawn().unwrap();
            drop(remote);
            Self {
                child,
                owner,
                root: root.to_owned(),
                reaped: false,
                settled: Arc::clone(&namespace.settled),
            }
        }
        fn settle(&mut self) -> io::Result<(Receipt, Output)> {
            let status = self.child.wait()?;
            self.reaped = true;
            if !status.success() {
                return Err(io::Error::other(
                    "custody guardian failed; namespace retained",
                ));
            }
            let receipt: Receipt = serde_json::from_slice(&read(&self.root, "custody.json")?)
                .map_err(io::Error::other)?;
            if receipt.guardian != self.child.id()
                || receipt.coordinator <= 1
                || receipt.coordinator == receipt.guardian
                || !receipt.family_echild
            {
                return Err(io::Error::other("mismatched or unsettled custody receipt"));
            }
            let status = std::process::ExitStatus::from_raw(receipt.status);
            let terminal = receipt.status >= 0
                && receipt.status <= 0xffff
                && match (status.code(), status.signal()) {
                    (Some(_), None) => receipt.status & 0xff == 0,
                    (None, Some(signal)) => {
                        receipt.status & !0xff == 0 && signal > 0 && signal <= libc::SIGRTMAX()
                    }
                    _ => false,
                };
            if !terminal {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "nonterminal or invalid coordinator wait status",
                ));
            }
            let output = Output {
                status,
                stdout: read(&self.root, "coordinator.stdout")?,
                stderr: read(&self.root, "coordinator.stderr")?,
            };
            self.settled.store(true, Ordering::Release);
            Ok((receipt, output))
        }
        // Operation deadline plus cooperative cleanup; exact-child waits in
        // uninterruptible kernel I/O may extend it. No hard wall-time claim.
        pub(super) fn wait_until(&mut self, deadline: Instant) -> io::Result<Output> {
            let mut expired = false;
            while self.child.try_wait()?.is_none() {
                if Instant::now() >= deadline {
                    expired = true;
                    let _ = self.owner.shutdown(std::net::Shutdown::Write);
                    break;
                }
                thread::sleep(Duration::from_millis(10));
            }
            let (receipt, output) = self.settle()?;
            if expired || receipt.cancelled {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "coordinator exceeded fixture deadline",
                ));
            }
            if receipt.forced {
                return Err(io::Error::other(
                    "coordinator required forced family cleanup",
                ));
            }
            Ok(output)
        }
        pub(super) fn signal_term(&mut self) -> io::Result<()> {
            self.owner.set_read_timeout(Some(Duration::from_secs(5)))?;
            self.owner.write_all(b"T")?;
            let mut ack = [0];
            self.owner.read_exact(&mut ack)?;
            if ack != *b"T" {
                return Err(io::Error::other("coordinator TERM was not acknowledged"));
            }
            Ok(())
        }
    }
    impl Drop for Coordinator {
        fn drop(&mut self) {
            if !self.reaped {
                let _ = self.owner.shutdown(std::net::Shutdown::Write);
                if let Err(error) = self.settle() {
                    eprintln!(
                        "Retained installer fixture {}: {error}",
                        self.root.display()
                    );
                    if !thread::panicking() {
                        panic!("unproven installer family settlement");
                    }
                }
            }
        }
    }

    #[test]
    #[ignore = "private sole-reaper entrypoint, selected only by coordinator fixtures"]
    fn guardian() {
        let spec: Spec = serde_json::from_str(&std::env::var(SPEC).unwrap()).unwrap();
        let root = PathBuf::from(std::env::var_os("CAGENTS_TEST_CUSTODY_ROOT").unwrap());
        assert!(spec.owner > 2);
        assert_ne!(
            unsafe { libc::signal(libc::SIGCHLD, libc::SIG_DFL) },
            libc::SIG_ERR
        );
        assert_eq!(
            unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) },
            0
        );
        let mut owner = unsafe { UnixStream::from_raw_fd(spec.owner) };
        assert_eq!(
            unsafe { libc::fcntl(owner.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) },
            0
        );
        owner.set_nonblocking(true).unwrap();
        let mut command = Command::new(std::ffi::OsString::from_vec(spec.program));
        command
            .args(spec.arguments.into_iter().map(std::ffi::OsString::from_vec))
            .env_remove(SPEC)
            .env_remove("CAGENTS_TEST_CUSTODY_ROOT")
            .stdin(Stdio::inherit())
            .stdout(capture(&root.join("coordinator.stdout")))
            .stderr(capture(&root.join("coordinator.stderr")))
            .process_group(0);
        if spec.ignored_sigchld {
            unsafe {
                command.pre_exec(|| {
                    if libc::signal(libc::SIGCHLD, libc::SIG_IGN) == libc::SIG_ERR {
                        return Err(io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
        }
        assert_eq!(
            owner_event(&mut owner).unwrap(),
            b'.',
            "owner revoked before coordinator launch"
        );
        let mut family = Family {
            child: command.spawn().unwrap(),
            reaped: false,
            settled: false,
            forced: false,
        };
        fs::write(root.join("coordinator.pid"), family.child.id().to_string()).unwrap();
        let mut cancelled = false;
        let mut grace = None;
        let status = loop {
            if let Some(status) = family.child.try_wait().unwrap() {
                family.reaped = true;
                break status;
            }
            let event = owner_event(&mut owner).unwrap();
            if event == b'T' || (event == 0 && !cancelled) {
                let sent = unsafe { libc::kill(family.child.id() as i32, libc::SIGTERM) };
                if sent != 0 {
                    let error = io::Error::last_os_error();
                    if error.raw_os_error() == Some(libc::ESRCH) {
                        if let Some(status) = family.child.try_wait().unwrap() {
                            family.reaped = true;
                            break status;
                        }
                    }
                    panic!("coordinator TERM failed: {error}");
                }
                if event == b'T' {
                    owner.write_all(b"T").unwrap();
                } else {
                    cancelled = true;
                    grace = Some(Instant::now() + Duration::from_secs(2));
                }
            }
            if grace.is_some_and(|deadline| Instant::now() >= deadline) {
                family.forced = true;
                family.child.kill().unwrap();
                let status = family.child.wait().unwrap();
                family.reaped = true;
                break status;
            }
            thread::sleep(Duration::from_millis(10));
        };
        family.retire(cancelled).unwrap();
        let receipt = Receipt {
            guardian: std::process::id(),
            coordinator: family.child.id(),
            status: status.into_raw(),
            cancelled,
            family_echild: true,
            forced: family.forced,
        };
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(root.join("custody.json"))
            .unwrap();
        file.write_all(&serde_json::to_vec(&receipt).unwrap())
            .unwrap();
    }

    #[test]
    fn failed_readiness_deadline_and_leaked_installers_cannot_escape_custody_or_pass() {
        let _fixture = OS_FIXTURE.lock().unwrap();
        for mode in ["readiness", "deadline", "natural"] {
            let root = Root::new(tempfile::tempdir().unwrap());
            let mut command = Command::new("/bin/sh");
            command.args(["-c", r#"trap '' TERM
/usr/bin/setsid /bin/sh -c 'exec 9>"$TEST_ROOT/fence"; /usr/bin/flock -x 9; printf "%s" "$$" > "$TEST_ROOT/installer.pid.tmp"; /bin/mv "$TEST_ROOT/installer.pid.tmp" "$TEST_ROOT/installer.pid"; exec /bin/sleep 60' &
while [ ! -f "$TEST_ROOT/release" ]; do /bin/sleep 0.01; done
exit 0
"#]).env("TEST_ROOT", root.path());
            let mut child = Coordinator::start(command, &root, false);
            let ready_by = Instant::now() + Duration::from_secs(5);
            let (coordinator, installer): (i32, i32) = loop {
                if let (Ok(coordinator), Ok(installer)) = (
                    fs::read_to_string(root.path().join("coordinator.pid")),
                    fs::read_to_string(root.path().join("installer.pid")),
                ) {
                    if let (Ok(coordinator), Ok(installer)) =
                        (coordinator.parse(), installer.parse())
                    {
                        break (coordinator, installer);
                    }
                }
                assert!(
                    Instant::now() < ready_by,
                    "detached installer did not reach its barrier"
                );
                thread::sleep(Duration::from_millis(5));
            };
            assert_eq!(unsafe { libc::getpgid(coordinator) }, coordinator);
            assert_eq!(unsafe { libc::getpgid(installer) }, installer);
            assert_ne!(coordinator, installer);
            let fence = File::open(root.path().join("fence")).unwrap();
            assert_eq!(
                unsafe { libc::flock(fence.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
                -1
            );
            if mode == "readiness" {
                // A setup error drops the armed owner before normal completion.
                let rejected: io::Result<()> = (move || {
                    let _child = child;
                    Err(io::Error::other("injected readiness rejection"))
                })();
                assert!(rejected.is_err());
            } else {
                if mode == "natural" {
                    fs::write(
                        root.path().join("release"),
                        "exit without retiring installer",
                    )
                    .unwrap();
                }
                let deadline = if mode == "deadline" {
                    Instant::now()
                } else {
                    Instant::now() + Duration::from_secs(5)
                };
                let error = child.wait_until(deadline).unwrap_err();
                assert_eq!(
                    error.kind(),
                    if mode == "deadline" {
                        io::ErrorKind::TimedOut
                    } else {
                        io::ErrorKind::Other
                    }
                );
            }
            let receipt: Receipt =
                serde_json::from_slice(&read(root.path(), "custody.json").unwrap()).unwrap();
            assert!(receipt.family_echild && receipt.forced);
            assert_eq!(receipt.coordinator, coordinator as u32);
            assert_eq!(receipt.cancelled, mode != "natural");
            if mode == "natural" {
                assert!(std::process::ExitStatus::from_raw(receipt.status).success());
            }
            assert_eq!(
                unsafe { libc::flock(fence.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
                0
            );
            drop(fence);
            root.close();
        }
    }

    #[test]
    fn rejected_receipt_or_parent_retirement_retains_namespace_without_unwinding() {
        let _fixture = OS_FIXTURE.lock().unwrap();
        for invalid_status in [
            None,
            Some((libc::SIGSTOP << 8) | 0x7f),
            Some(0xffff),
            Some(0x10000),
        ] {
            let root = Root::new(tempfile::tempdir().unwrap());
            let path = root.path().to_owned();
            let mut child = Coordinator::start(Command::new("/bin/true"), &root, false);
            let deadline = Instant::now() + Duration::from_secs(5);
            while child.child.try_wait().unwrap().is_none() {
                assert!(Instant::now() < deadline, "receipt control did not finish");
                thread::sleep(Duration::from_millis(10));
            }
            let original = read(&path, "custody.json").unwrap();
            let mut receipt: Receipt = serde_json::from_slice(&original).unwrap();
            if let Some(status) = invalid_status {
                receipt.status = status;
            } else {
                receipt.guardian += 1;
            }
            fs::write(
                path.join("custody.json"),
                serde_json::to_vec(&receipt).unwrap(),
            )
            .unwrap();
            let error = child.settle().err().expect("invalid receipt was accepted");
            if invalid_status.is_some() {
                assert_eq!(error.kind(), io::ErrorKind::InvalidData);
            }
            assert!(!child.settled.load(Ordering::Acquire));
            drop(root);
            assert!(
                path.is_dir(),
                "caught custody errors must not auto-delete evidence"
            );
            // Validate the original genuine receipt before removing this
            // passing meta-test's own retained control namespace.
            fs::write(path.join("custody.json"), original).unwrap();
            child.settle().unwrap();
            assert!(child.settled.load(Ordering::Acquire));
            fs::remove_dir_all(path).unwrap();
        }
        let root = Root::new(tempfile::tempdir().unwrap());
        let path = root.path().to_owned();
        let mut child = Coordinator::start(Command::new("/bin/true"), &root, false);
        child
            .wait_until(Instant::now() + Duration::from_secs(5))
            .unwrap();
        let mut parent_window = OwnedProcess(Command::new("/bin/sleep").arg("60").spawn().unwrap());
        let listener = UnixListener::bind(path.join("parent.sock")).unwrap();
        let result =
            root.close_after_parent(|| Err(io::Error::other("injected parent retirement failure")));
        assert!(result.is_err() && path.is_dir());
        assert!(parent_window.0.try_wait().unwrap().is_none());
        assert!(
            listener.local_addr().is_ok(),
            "guardian ECHILD excludes the live parent listener"
        );
        parent_window.stop_checked().unwrap();
        drop(parent_window);
        drop(listener);
        assert!(child.settled.load(Ordering::Acquire));
        fs::remove_dir_all(path).unwrap();
    }
}

struct OwnedProcess(std::process::Child);
impl OwnedProcess {
    fn stop_checked(&mut self) -> std::io::Result<()> {
        if self.0.try_wait()?.is_none() {
            if let Err(error) = self.0.kill() {
                // An exit racing the signal is still a checked retirement;
                // a live original child with a rejected signal is an error.
                if self.0.try_wait()?.is_none() {
                    return Err(error);
                }
            }
        }
        self.0.wait()?;
        Ok(())
    }
    fn window(executable: &Path, runtime: &Path, ready: &Path) -> Self {
        fs::copy(std::env::current_exe().unwrap(), executable).unwrap();
        let child = Command::new(executable)
            .args(["--exact", "staged_window_lease_fixture", "--nocapture"])
            .env("CAGENTS_TEST_UI_LEASE", ready)
            .env("XDG_RUNTIME_DIR", runtime)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let fixture = Self(child);
        let deadline = Instant::now() + Duration::from_secs(5);
        while !ready.exists() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(
            ready.exists(),
            "installed fixture UI must acquire its actual lease"
        );
        fixture
    }
}
impl Drop for OwnedProcess {
    fn drop(&mut self) {
        // Failure fallback only. Namespace deletion requires stop_checked;
        // ignored errors here never establish a parent-retirement proof.
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn staged_window_lease_fixture() {
    use std::io::Read;
    let Some(ready) = std::env::var_os("CAGENTS_TEST_UI_LEASE") else {
        return;
    };
    let _lease = cagents::instance::WindowLease::acquire(false, None).unwrap();
    fs::write(ready, "ready").unwrap();
    let _ = std::io::stdin().read(&mut [0]);
}

fn verify(runtime: &Path, lease: Option<&File>) -> Output {
    verify_mode(runtime, lease, false)
}

fn verify_mode(runtime: &Path, lease: Option<&File>, stage_only: bool) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_cagents"));
    command
        .arg("verify-install-guard")
        .env("XDG_RUNTIME_DIR", runtime)
        .env("CAGENTS_UI_UPDATE_GUARD_FD", "197");
    if stage_only {
        command.arg("--stage-only");
    }
    if let Some(lease) = lease {
        let fd = lease.as_raw_fd();
        // SAFETY: the parent retains this descriptor until output returns.
        unsafe {
            command.pre_exec(move || {
                if libc::dup2(fd, 197) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::fcntl(197, libc::F_SETFD, 0) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    command.output().unwrap()
}

#[test]
fn external_installer_saves_closes_and_reopens_windows_even_on_failure() {
    let _fixture = OS_FIXTURE.lock().unwrap();
    for status in [
        "0",
        "7",
        "cancel",
        "deferred",
        "complete",
        "inspector-refused",
        "inspector-missing",
    ] {
        let root = coordinator_fixture::Root::new(tempfile::tempdir().unwrap());
        let refused = matches!(status, "inspector-refused" | "inspector-missing");
        let fixture_bin = root.path().join("bin");
        fs::create_dir(&fixture_bin).unwrap();
        let cli = fixture_bin.join("cagents");
        fs::copy(env!("CARGO_BIN_EXE_cagents"), &cli).unwrap();
        if status != "inspector-missing" {
            install_source_inspector_fixture(&fixture_bin.join("cagentsd"));
        }
        let runtime = root.path().join("runtime");
        fs::create_dir(&runtime).unwrap();
        fs::set_permissions(&runtime, fs::Permissions::from_mode(0o700)).unwrap();
        let development = cfg!(any(debug_assertions, feature = "development"));
        let name = if development {
            "cagents-development-ui"
        } else {
            "cagents-ui"
        };
        let window = file(&runtime.join(format!("{name}.lock.primary")));
        assert_eq!(
            unsafe { libc::flock(window.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0
        );
        let listener = UnixListener::bind(runtime.join(format!("{name}.activate"))).unwrap();
        listener.set_nonblocking(true).unwrap();
        let events = root.path().join("events");
        let server_events = events.clone();
        let server = thread::spawn(move || {
            for expected in [
                "update-prepare",
                if refused {
                    "update-cancel"
                } else {
                    "update-commit"
                },
            ] {
                let deadline = Instant::now() + Duration::from_secs(5);
                let mut stream = loop {
                    match listener.accept() {
                        Ok((stream, _)) => break stream,
                        Err(error)
                            if error.kind() == std::io::ErrorKind::WouldBlock
                                && Instant::now() < deadline =>
                        {
                            thread::sleep(Duration::from_millis(10))
                        }
                        Err(error) => panic!("window request unavailable: {error}"),
                    }
                };
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut request = String::new();
                BufReader::new(&stream).read_line(&mut request).unwrap();
                assert_eq!(request.trim(), expected);
                writeln!(
                    OpenOptions::new()
                        .append(true)
                        .create(true)
                        .open(&server_events)
                        .unwrap(),
                    "{expected}"
                )
                .unwrap();
                if expected == "update-commit" {
                    assert_eq!(unsafe { libc::flock(window.as_raw_fd(), libc::LOCK_UN) }, 0);
                }
                writeln!(stream, "ok").unwrap();
            }
            (window, listener)
        });
        let prefix = root.path().join("installation");
        let bin = prefix.join("versions/old/bin");
        fs::create_dir_all(&bin).unwrap();
        std::os::unix::fs::symlink("versions/old", prefix.join("current")).unwrap();
        let ui = bin.join("cagents-ui");
        fs::write(
            &ui,
            "#!/bin/sh\ntest -z \"${CAGENTS_UI_UPDATE_GUARD_FD+x}${CAGENTS_INSTALL_RESULT_FD+x}${CAGENTS_ARCHIVE+x}${CAGENTS_CHECKSUMS+x}${CAGENTS_REPOSITORY+x}\" || exit 92\nprintf 'old %s\\n' \"$*\" > \"$HOME/reopened\"\nprintf 'ready\\n'\n",
        )
        .unwrap();
        fs::set_permissions(&ui, fs::Permissions::from_mode(0o700)).unwrap();
        let new_bin = prefix.join("versions/new/bin");
        fs::create_dir_all(&new_bin).unwrap();
        let new_ui = new_bin.join("cagents-ui");
        fs::write(
            &new_ui,
            fs::read_to_string(&ui)
                .unwrap()
                .replace("'old %s", "'new %s"),
        )
        .unwrap();
        fs::set_permissions(&new_ui, fs::Permissions::from_mode(0o700)).unwrap();
        let installer = root.path().join("installer.sh");
        fs::write(&installer, "#!/bin/sh\n\"$FIXTURE_REAL_CLI\" verify-install-guard >/dev/null || exit 91\ntrap 'printf \"rolled-back\\n\" >> \"$HOME/events\"; exit 7' TERM\nprintf 'installed\\n' >> \"$HOME/events\"\nif [ \"$1\" = cancel ]; then while :; do sleep 0.05; done; fi\nif [ \"$1\" = deferred ]; then \"$FIXTURE_REAL_CLI\" record-deferred-install >/dev/null || exit 93; fi\ncase \"$1\" in deferred|complete) ln -sfn versions/new \"$FIXTURE_PREFIX/current\"; exit 0 ;; esac\nexit \"$1\"\n").unwrap();
        let mut command = Command::new(&cli);
        command
            .arg("coordinate-install")
            .arg("--prefix")
            .arg(&prefix)
            .arg("--installer")
            .arg(&installer)
            .arg("--")
            .arg(status)
            .env("HOME", root.path())
            .env("XDG_RUNTIME_DIR", &runtime)
            .env("XDG_DATA_HOME", root.path().join("data"))
            .env("XDG_CONFIG_HOME", root.path().join("config"))
            .env_remove("CAGENTS_UI_UPDATE_GUARD_FD")
            .env("CAGENTS_ARCHIVE", "/stale/installer/archive")
            .env("CAGENTS_CHECKSUMS", "/stale/installer/checksums")
            .env("CAGENTS_REPOSITORY", "fixture/source")
            .env("FIXTURE_REAL_CLI", &cli)
            .env("FIXTURE_PREFIX", &prefix)
            .env(
                "FIXTURE_SOURCE_PROTOCOL",
                cagents::terminal_ipc::PROTOCOL_VERSION.to_string(),
            )
            .env(
                "FIXTURE_SOURCE_REFUSED",
                if status == "inspector-refused" {
                    "1"
                } else {
                    "0"
                },
            )
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // Ignored SIGCHLD survives exec; the coordinator must reset it before
        // relying on its unreaped child as process-group cleanup authority.
        let mut child = coordinator_fixture::Coordinator::start(command, &root, true);
        if status == "cancel" {
            let deadline = Instant::now() + Duration::from_secs(5);
            while !fs::read_to_string(&events).is_ok_and(|text| text.contains("installed")) {
                assert!(Instant::now() < deadline, "installer did not start");
                thread::sleep(Duration::from_millis(10));
            }
            child.signal_term().unwrap();
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        let output = child.wait_until(deadline).unwrap();
        let (window, listener) = server.join().unwrap_or_else(|error| {
            panic!("window fixture failed: {error:?}; coordinator: {output:?}")
        });
        assert_eq!(
            output.status.success(),
            matches!(status, "0" | "deferred" | "complete"),
            "{output:?}"
        );
        if refused {
            assert_eq!(
                fs::read_to_string(&events).unwrap(),
                if status == "inspector-refused" {
                    "update-prepare\nsource-inspect\nupdate-cancel\n"
                } else {
                    "update-prepare\nupdate-cancel\n"
                },
                "{output:?}"
            );
            if status == "inspector-refused" {
                assert!(
                    String::from_utf8_lossy(&output.stderr)
                        .contains("fixture source panes refused"),
                    "{output:?}"
                );
            } else {
                assert!(
                    String::from_utf8_lossy(&output.stderr).contains("(os error 2)"),
                    "missing matched inspector must expose the actual not-found failure: {output:?}"
                );
            }
            println!(
                "source inspection {status}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            let probe = File::open(runtime.join(format!("{name}.lock.primary"))).unwrap();
            assert_eq!(
                unsafe { libc::flock(probe.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
                -1,
                "source inspection refusal closed the original window"
            );
            assert_eq!(
                std::io::Error::last_os_error().kind(),
                std::io::ErrorKind::WouldBlock
            );
            assert!(listener.local_addr().is_ok());
            assert!(
                !root.path().join("reopened").exists(),
                "an unclosed source window was relaunched"
            );
        } else {
            assert_eq!(
                fs::read_to_string(events).unwrap(),
                if status == "cancel" {
                    "update-prepare\nsource-inspect\nupdate-commit\ninstalled\nrolled-back\n"
                } else {
                    "update-prepare\nsource-inspect\nupdate-commit\ninstalled\n"
                }
            );
            assert!(
                fs::read_to_string(root.path().join("reopened"))
                    .unwrap()
                    .contains("--window primary")
            );
            assert!(
                fs::read_to_string(root.path().join("reopened"))
                    .unwrap()
                    .starts_with(if status == "complete" { "new " } else { "old " }),
                "deferred installations must reopen the exact prior UI version"
            );
        }
        assert_eq!(unsafe { libc::flock(window.as_raw_fd(), libc::LOCK_UN) }, 0);
        drop(window);
        drop(listener);
        root.close();
    }
}

/// The isolated CLI lane has no host executable. Model its process boundary
/// explicitly beside a private copy of the real CLI; never use a stale helper
/// from another feature lane. This fixture verifies the actual inherited fence
/// and request, but does not qualify the host's SQLite/provider inspection.
fn install_source_inspector_fixture(path: &Path) {
    fs::write(path, r#"#!/usr/bin/python3
import json, os, pathlib, subprocess, sys
assert len(sys.argv) == 3 and sys.argv[1] == '--inspect-upgrade-source'
assert json.loads(sys.argv[2]) == {'protocol':int(os.environ['FIXTURE_SOURCE_PROTOCOL']),
    'proof':None, 'require_resumable':False, 'reconcile_retained':False}
assert not any(key in os.environ for key in ['CAGENTS_INSTALL_RESULT_FD',
    'CAGENTS_INSTALL_OWNER_CLI', 'CAGENTS_ARCHIVE', 'CAGENTS_CHECKSUMS', 'CAGENTS_REPOSITORY'])
fd=int(os.environ['CAGENTS_UI_UPDATE_GUARD_FD'])
verified=subprocess.run([os.environ['FIXTURE_REAL_CLI'], 'verify-install-guard', '--stage-only'],
    pass_fds=(fd,), stdout=subprocess.PIPE, stderr=subprocess.PIPE)
if verified.returncode:
    sys.stderr.buffer.write(verified.stderr)
    sys.exit(91)
with (pathlib.Path(os.environ['HOME'])/'events').open('a') as events:
    events.write('source-inspect\n')
print(json.dumps({'Err':'fixture source panes refused'} if os.environ['FIXTURE_SOURCE_REFUSED']=='1' else {'Ok':None}))
"#).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}

fn file(path: &Path) -> File {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .unwrap()
}

#[test]
fn inherited_fence_requires_exact_locked_description_and_closed_windows() {
    let _fixture = OS_FIXTURE.lock().unwrap();
    let runtime = tempfile::tempdir().unwrap();
    fs::set_permissions(runtime.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let name = if cfg!(any(debug_assertions, feature = "development")) {
        "cagents-development-ui.lock"
    } else {
        "cagents-ui.lock"
    };
    let lease = file(&runtime.path().join(format!("{name}.update")));
    assert!(
        !verify(runtime.path(), None).status.success(),
        "an environment flag is not a lease"
    );
    assert!(
        !verify(runtime.path(), Some(&lease)).status.success(),
        "an unlocked file is not authority"
    );
    assert!(
        !verify_mode(runtime.path(), Some(&lease), true)
            .status
            .success(),
        "an unlocked staging fence is not authority"
    );
    assert_eq!(
        unsafe { libc::flock(lease.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0
    );
    let different = file(&runtime.path().join("different"));
    assert!(!verify(runtime.path(), Some(&different)).status.success());
    for stage_only in [false, true] {
        assert!(
            !verify_mode(runtime.path(), None, stage_only)
                .status
                .success()
        );
        assert!(
            !verify_mode(runtime.path(), Some(&different), stage_only)
                .status
                .success()
        );
    }
    let valid = verify(runtime.path(), Some(&lease));
    assert!(valid.status.success(), "{valid:?}");
    let independent = File::open(runtime.path().join(format!("{name}.update"))).unwrap();
    assert_eq!(
        unsafe { libc::flock(independent.as_raw_fd(), libc::LOCK_SH | libc::LOCK_NB) },
        -1,
        "validation must not unlock its parent"
    );
    let window = file(&runtime.path().join(format!("{name}.primary")));
    assert_eq!(
        unsafe { libc::flock(window.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0
    );
    assert!(
        !verify(runtime.path(), Some(&lease)).status.success(),
        "an unclosed window cannot be bypassed"
    );
    let staged = verify_mode(runtime.path(), Some(&lease), true);
    assert!(staged.status.success(), "{staged:?}");
    assert_eq!(
        unsafe { libc::flock(independent.as_raw_fd(), libc::LOCK_SH | libc::LOCK_NB) },
        -1
    );
}

#[test]
fn staging_never_contacts_or_reopens_live_windows_even_on_failure_or_cancel() {
    let _fixture = OS_FIXTURE.lock().unwrap();
    for status in ["0", "7", "cancel"] {
        let root = coordinator_fixture::Root::new(tempfile::tempdir_in("/var/tmp").unwrap());
        let runtime = root.path().join("runtime");
        fs::create_dir(&runtime).unwrap();
        fs::set_permissions(&runtime, fs::Permissions::from_mode(0o700)).unwrap();
        let name = if cfg!(any(debug_assertions, feature = "development")) {
            "cagents-development-ui"
        } else {
            "cagents-ui"
        };
        let listener = UnixListener::bind(runtime.join(format!("{name}.activate"))).unwrap();
        listener.set_nonblocking(true).unwrap();
        let prefix = root.path().join("install");
        for version in ["old", "new"] {
            let bin = prefix.join("versions").join(version).join("bin");
            fs::create_dir_all(&bin).unwrap();
            for name in ["cagents", "cagentsd", "cagents-ui"] {
                fs::write(
                    bin.join(name),
                    "#!/bin/sh\nprintf 'unexpected launch\\n' >> \"$TEST_ACTIONS\"\nexit 90\n",
                )
                .unwrap();
                fs::set_permissions(bin.join(name), fs::Permissions::from_mode(0o755)).unwrap();
            }
        }
        fs::write(
            prefix.join(".cagents-install-root"),
            "cagents install root\n",
        )
        .unwrap();
        let mut window = OwnedProcess::window(
            &prefix.join("versions/old/bin/cagents-ui"),
            &runtime,
            &root.path().join("window-ready"),
        );
        std::os::unix::fs::symlink("versions/old", prefix.join("current")).unwrap();
        let script = root.path().join("installer");
        fs::write(&script, r#"#!/bin/sh
set -eu
"$TEST_CLI" verify-install-guard --stage-only
if "$TEST_CLI" verify-install-guard; then exit 91; fi
"$TEST_CLI" record-deferred-install --stage-only --prefix "$TEST_PREFIX" --version-root "$TEST_PREFIX/versions/new"
printf ready > "$TEST_READY"
case "$TEST_STATUS" in
  0) ln -s versions/new "$TEST_PREFIX/next"; mv -Tf "$TEST_PREFIX/next" "$TEST_PREFIX/current" ;;
  7) exit 7 ;;
  cancel) sleep 30 ;;
esac
"#).unwrap();
        let ready = root.path().join("ready");
        let actions = root.path().join("actions");
        let mut command = Command::new(env!("CARGO_BIN_EXE_cagents"));
        command
            .args(["coordinate-install", "--stage-only", "--prefix"])
            .arg(&prefix)
            .arg("--installer")
            .arg(&script)
            .env("XDG_RUNTIME_DIR", &runtime)
            .env("XDG_CONFIG_HOME", root.path().join("config"))
            .env("XDG_DATA_HOME", root.path().join("data"))
            .env("XDG_CACHE_HOME", root.path().join("cache"))
            .env("TEST_CLI", env!("CARGO_BIN_EXE_cagents"))
            .env("TEST_PREFIX", &prefix)
            .env("TEST_READY", &ready)
            .env("TEST_ACTIONS", &actions)
            .env("TEST_STATUS", status)
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut child = coordinator_fixture::Coordinator::start(command, &root, false);
        let deadline = Instant::now() + Duration::from_secs(5);
        while !ready.exists() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(
            ready.exists(),
            "staging did not reach its held-fence transaction"
        );
        if status == "cancel" {
            child.signal_term().unwrap();
        }
        let output = child
            .wait_until(Instant::now() + Duration::from_secs(5))
            .unwrap();
        assert_eq!(output.status.success(), status == "0", "{output:?}");
        assert!(!actions.exists(), "no UI or daemon may be launched");
        assert!(
            matches!(listener.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock),
            "no prepare, commit or cancel request may be sent"
        );
        let independent = File::open(runtime.join(format!("{name}.lock.primary"))).unwrap();
        assert_eq!(
            unsafe { libc::flock(independent.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            -1,
            "window lease must survive"
        );
        let fence = File::open(runtime.join(format!("{name}.lock.update"))).unwrap();
        assert_eq!(
            unsafe { libc::flock(fence.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0,
            "staging must release its launch fence"
        );
        assert_eq!(
            fs::read_link(prefix.join("current")).unwrap(),
            Path::new(if status == "0" {
                "versions/new"
            } else {
                "versions/old"
            })
        );
        root.close_after_parent(|| {
            window.stop_checked()?;
            drop(window);
            drop(listener);
            Ok(())
        })
        .unwrap();
    }
}

#[test]
fn staged_candidate_startup_authenticates_the_inherited_coordinator_fence() {
    let _fixture = OS_FIXTURE.lock().unwrap();
    let root = tempfile::tempdir_in("/var/tmp").unwrap();
    let prefix = root.path().join("install");
    for version in ["old", "new"] {
        let bin = prefix.join("versions").join(version).join("bin");
        fs::create_dir_all(&bin).unwrap();
        for name in ["cagents", "cagentsd", "cagents-ui"] {
            fs::write(bin.join(name), "complete matched fixture").unwrap();
        }
    }
    let candidate = prefix.join("versions/new/bin/cagents");
    fs::copy(env!("CARGO_BIN_EXE_cagents"), &candidate).unwrap();
    fs::write(
        prefix.join(".cagents-install-root"),
        "cagents install root\n",
    )
    .unwrap();
    std::os::unix::fs::symlink("versions/new", prefix.join("current")).unwrap();
    let marker = prefix.join("versions/new/.deferred-owner.json");
    fs::write(&marker, r#"{"version":1,"owner_version":"old"}"#).unwrap();
    fs::set_permissions(&marker, fs::Permissions::from_mode(0o600)).unwrap();
    let runtime = root.path().join("runtime");
    fs::create_dir(&runtime).unwrap();
    fs::set_permissions(&runtime, fs::Permissions::from_mode(0o700)).unwrap();
    let name = if cfg!(any(debug_assertions, feature = "development")) {
        "cagents-development-ui.lock"
    } else {
        "cagents-ui.lock"
    };
    // After cooperative UI shutdown the coordinator keeps both fences alive.
    // There is deliberately no daemon or window to authenticate instead.
    let legacy = file(&runtime.join(name));
    assert_eq!(
        unsafe { libc::flock(legacy.as_raw_fd(), libc::LOCK_SH | libc::LOCK_NB) },
        0
    );
    let update_path = runtime.join(format!("{name}.update"));
    let update = file(&update_path);
    let run = |inherited: Option<&File>, marker: Option<&str>| {
        let mut command = Command::new(&candidate);
        command
            .args(["--json", "daemon", "status"])
            .env("XDG_RUNTIME_DIR", &runtime)
            .env("XDG_CONFIG_HOME", root.path().join("config"))
            .env("XDG_DATA_HOME", root.path().join("data"))
            .env("XDG_CACHE_HOME", root.path().join("cache"))
            .env_remove("CAGENTS_DAEMON_LAUNCH_FENCE_FD")
            .env_remove("CAGENTS_UI_UPDATE_GUARD_FD");
        if let Some(marker) = marker {
            command.env("CAGENTS_UI_UPDATE_GUARD_FD", marker);
        }
        let descriptor = inherited.map(AsRawFd::as_raw_fd);
        // SAFETY: only descriptor operations run after fork, and the parent
        // retains each input description until the child exits.
        unsafe {
            command.pre_exec(move || {
                if let Some(descriptor) = descriptor {
                    if libc::dup2(descriptor, 197) == -1 || libc::fcntl(197, libc::F_SETFD, 0) == -1
                    {
                        return Err(std::io::Error::last_os_error());
                    }
                } else {
                    libc::close(197);
                }
                Ok(())
            });
        }
        command.output().unwrap()
    };
    let ordinary = run(None, None);
    assert!(
        !ordinary.status.success(),
        "legacy contention must remain a refusal: {ordinary:?}"
    );
    for (descriptor, marker) in [
        (None, "197"),
        (Some(&update), "197"),
        (Some(&update), "999"),
    ] {
        let output = run(descriptor, Some(marker));
        assert!(
            !output.status.success(),
            "missing, unheld or malformed authority: {output:?}"
        );
    }
    assert_eq!(
        unsafe { libc::flock(update.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0
    );
    let different = file(&runtime.join("unrelated-update"));
    let independent = File::open(&update_path).unwrap();
    for descriptor in [&different, &independent] {
        let output = run(Some(descriptor), Some("197"));
        assert!(
            !output.status.success(),
            "a different description must not borrow authority: {output:?}"
        );
    }
    let valid = run(Some(&update), Some("197"));
    assert!(valid.status.success(), "{valid:?}");
    let result: serde_json::Value = serde_json::from_slice(&valid.stdout).unwrap();
    assert_eq!(result["data"]["running"], false);
    assert_eq!(
        unsafe { libc::flock(independent.as_raw_fd(), libc::LOCK_SH | libc::LOCK_NB) },
        -1,
        "validation must not release the coordinator's exclusive fence"
    );
    assert_eq!(unsafe { libc::flock(update.as_raw_fd(), libc::LOCK_UN) }, 0);
    // An older coordinator releases its launch fence before native reopen, but
    // keeps the shared legacy fence until the new window handshakes. Pin an
    // actual candidate executable with the daemon's kernel lease, not a stale
    // path/marker claim. No provider or production daemon is contacted.
    let daemon_dir = runtime.join(cagents::instance::RUNTIME_SUBDIRECTORY);
    fs::create_dir_all(&daemon_dir).unwrap();
    fs::set_permissions(&daemon_dir, fs::Permissions::from_mode(0o700)).unwrap();
    let candidate_daemon = prefix.join("versions/new/bin/cagentsd");
    fs::copy("/bin/sleep", &candidate_daemon).unwrap();
    let daemon_lock_path = daemon_dir.join("cagentsd.lock");
    let daemon_lock = file(&daemon_lock_path);
    assert_eq!(
        unsafe { libc::flock(daemon_lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0
    );
    let mut command = Command::new(&candidate_daemon);
    command.arg("30");
    let descriptor = daemon_lock.as_raw_fd();
    unsafe {
        command.pre_exec(move || {
            if libc::fcntl(descriptor, libc::F_SETFD, 0) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let daemon = OwnedProcess(command.spawn().unwrap());
    fs::write(&daemon_lock_path, format!(r#"{{"pid":{}}}"#, daemon.0.id())).unwrap();
    let native = prefix.join("versions/new/bin/cagents-ui");
    fs::copy(std::env::current_exe().unwrap(), &native).unwrap();
    let reopened = Command::new(&native)
        .args(["--exact", "staged_native_ui_owner_fixture", "--nocapture"])
        .env("CAGENTS_TEST_STAGED_NATIVE_OWNER", "candidate")
        .env("XDG_RUNTIME_DIR", &runtime)
        .env_remove("CAGENTS_DAEMON_LAUNCH_FENCE_FD")
        .env_remove("CAGENTS_UI_UPDATE_GUARD_FD")
        .output()
        .unwrap();
    assert!(reopened.status.success(), "{reopened:?}");
    let candidate_status = run(None, None);
    assert!(candidate_status.status.success(), "{candidate_status:?}");
    let result: serde_json::Value = serde_json::from_slice(&candidate_status.stdout).unwrap();
    assert_eq!(result["data"]["running"], true);
    assert_eq!(
        unsafe { libc::flock(legacy.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0
    );
    let legacy_owner = run(None, None);
    assert!(
        !legacy_owner.status.success(),
        "an exclusive legacy UI cannot be bypassed by a candidate daemon: {legacy_owner:?}"
    );
    assert_eq!(
        unsafe { libc::flock(legacy.as_raw_fd(), libc::LOCK_SH | libc::LOCK_NB) },
        0
    );
    drop(daemon);
    drop(daemon_lock);
    let stopped_candidate = run(None, None);
    assert!(
        !stopped_candidate.status.success(),
        "a stopped candidate cannot relax unknown shared legacy ownership: {stopped_candidate:?}"
    );
}

#[test]
fn staged_cli_routes_new_windows_and_control_to_kernel_verified_matched_owner() {
    if !user_fixture::enter() {
        return;
    }
    let _fixture = OS_FIXTURE.lock().unwrap();
    let root = tempfile::tempdir_in("/var/tmp").unwrap();
    let prefix = root.path().join("install");
    let old = prefix.join("versions/old/bin");
    let new = prefix.join("versions/new/bin");
    for bin in [&old, &new] {
        fs::create_dir_all(bin).unwrap();
    }
    fs::write(
        prefix.join(".cagents-install-root"),
        "cagents install root\n",
    )
    .unwrap();
    fs::copy(env!("CARGO_BIN_EXE_cagents"), new.join("cagents")).unwrap();
    for bin in [&old, &new] {
        fs::copy("/bin/sleep", bin.join("cagentsd")).unwrap();
        fs::write(bin.join("cagents-ui"), "complete matched fixture").unwrap();
    }
    fs::write(old.join("cagents"), "#!/bin/sh\nprintf '%s\\n' \"$*\"\n").unwrap();
    fs::set_permissions(old.join("cagents"), fs::Permissions::from_mode(0o755)).unwrap();
    let marker = prefix.join("versions/new/.deferred-owner.json");
    fs::write(&marker, r#"{"version":1,"owner_version":"old"}"#).unwrap();
    fs::set_permissions(&marker, fs::Permissions::from_mode(0o600)).unwrap();
    let runtime = root.path().join("runtime");
    let daemon_dir = runtime.join(if cfg!(any(debug_assertions, feature = "development")) {
        "cagents-development"
    } else {
        "cagents"
    });
    fs::create_dir_all(&daemon_dir).unwrap();
    fs::set_permissions(&runtime, fs::Permissions::from_mode(0o700)).unwrap();
    fs::set_permissions(&daemon_dir, fs::Permissions::from_mode(0o700)).unwrap();
    let lock = file(&daemon_dir.join("cagentsd.lock"));
    assert_eq!(
        unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0
    );
    let fd = lock.as_raw_fd();
    let mut command = Command::new(old.join("cagentsd"));
    command.arg("30");
    unsafe {
        command.pre_exec(move || {
            if libc::fcntl(fd, libc::F_SETFD, 0) == -1 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    }
    let mut owner = command.spawn().unwrap();
    fs::write(
        daemon_dir.join("cagentsd.lock"),
        format!("{{\"pid\":{}}}", owner.id()),
    )
    .unwrap();
    for args in [
        vec!["ui", "--new-window"],
        vec!["--json", "daemon", "status"],
        vec!["terminal", "list"],
    ] {
        let output = Command::new(new.join("cagents"))
            .args(&args)
            .env("XDG_CONFIG_HOME", root.path().join("config"))
            .env("XDG_DATA_HOME", root.path().join("data"))
            .env("XDG_CACHE_HOME", root.path().join("cache"))
            .env("XDG_RUNTIME_DIR", &runtime)
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        assert_eq!(
            String::from_utf8(output.stdout).unwrap().trim(),
            args.join(" ")
        );
    }
    // Exercise the real native launch resolver from a matched candidate path.
    // An older updater reopens that exact UI path without retiring our marker.
    fs::copy(std::env::current_exe().unwrap(), new.join("cagents-ui")).unwrap();
    let native_route = |expected: &str| {
        let output = Command::new(new.join("cagents-ui"))
            .args(["--exact", "staged_native_ui_owner_fixture", "--nocapture"])
            .env("CAGENTS_TEST_STAGED_NATIVE_OWNER", expected)
            .env("CAGENTS_TEST_STAGED_OWNER_PATH", old.join("cagents-ui"))
            .env("XDG_RUNTIME_DIR", &runtime)
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
    };
    native_route("old");
    fs::write(
        &marker,
        serde_json::to_vec(&serde_json::json!({
            "version": 1, "owner_version": "old", "daemon_compatible": true,
            "candidate_protocol": cagents::terminal_ipc::PROTOCOL_VERSION,
        }))
        .unwrap(),
    )
    .unwrap();
    native_route("candidate");
    let output = Command::new(new.join("cagents"))
        .args(["--json", "daemon", "status"])
        .env("XDG_RUNTIME_DIR", &runtime)
        .output()
        .unwrap();
    assert_ne!(
        String::from_utf8(output.stdout).unwrap().trim(),
        "--json daemon status",
        "compatible control must use the candidate"
    );
    // Untrusted marker shape and filesystem authority fail before any routing.
    for body in [
        "not JSON",
        r#"{"version":2,"owner_version":"old"}"#,
        r#"{"version":1,"owner_version":"../old"}"#,
    ] {
        fs::write(&marker, body).unwrap();
        let output = Command::new(new.join("cagents"))
            .args(["terminal", "list"])
            .env("XDG_RUNTIME_DIR", &runtime)
            .output()
            .unwrap();
        assert!(
            !output.status.success(),
            "invalid marker cannot select old control"
        );
    }
    fs::write(&marker, r#"{"version":1,"owner_version":"old"}"#).unwrap();
    let alias = prefix.join("alias");
    fs::hard_link(&marker, &alias).unwrap();
    let output = Command::new(new.join("cagents"))
        .args(["terminal", "list"])
        .env("XDG_RUNTIME_DIR", &runtime)
        .output()
        .unwrap();
    assert!(!output.status.success(), "shared marker inode cannot route");
    fs::remove_file(&alias).unwrap();
    fs::rename(&marker, &alias).unwrap();
    std::os::unix::fs::symlink(&alias, &marker).unwrap();
    let output = Command::new(new.join("cagents"))
        .args(["terminal", "list"])
        .env("XDG_RUNTIME_DIR", &runtime)
        .output()
        .unwrap();
    assert!(!output.status.success(), "marker symlink cannot route");
    fs::remove_file(&marker).unwrap();
    fs::rename(&alias, &marker).unwrap();
    // Local capability discovery must report the candidate, without forwarding.
    let output = Command::new(new.join("cagents"))
        .args(["daemon", "protocol-version"])
        .env("XDG_RUNTIME_DIR", &runtime)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert_ne!(
        String::from_utf8(output.stdout).unwrap().trim(),
        "daemon protocol-version"
    );
    owner.kill().unwrap();
    owner.wait().unwrap();
    drop(lock);
    // A real installed old UI survives its daemon. Read-only public commands
    // select its CLI without invoking daemon start; autostart attempts exactly
    // the old CLI, never silently falls through to the incompatible candidate.
    let actions = root.path().join("recovery-actions");
    fs::write(
        old.join("cagents"),
        r#"#!/bin/sh
printf '%s\n' "$*" >> "$CAGENTS_TEST_RECOVERY_ACTIONS"
printf '%s\n' "$*"
"#,
    )
    .unwrap();
    let window = OwnedProcess::window(
        &old.join("cagents-ui"),
        &runtime,
        &root.path().join("surviving-ui-ready"),
    );
    for (args, expected) in [
        (vec!["--json", "daemon", "status"], "--json daemon status"),
        (vec!["daemon", "preflight"], "daemon preflight"),
        // Stop carries its implicit no-start policy to the matched owner.
        (vec!["daemon", "stop"], "--no-start daemon stop"),
    ] {
        let output = Command::new(new.join("cagents"))
            .args(&args)
            .env("XDG_RUNTIME_DIR", &runtime)
            .env("CAGENTS_TEST_RECOVERY_ACTIONS", &actions)
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        assert_eq!(String::from_utf8(output.stdout).unwrap().trim(), expected);
    }
    assert_eq!(fs::read_to_string(&actions).unwrap().lines().count(), 3);
    let handoff = OpenOptions::new()
        .read(true)
        .write(true)
        .open(
            runtime.join(if cfg!(any(debug_assertions, feature = "development")) {
                "cagents-development-ui.lock.update"
            } else {
                "cagents-ui.lock.update"
            }),
        )
        .unwrap();
    assert_eq!(
        unsafe { libc::flock(handoff.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0
    );
    let output = Command::new(new.join("cagents"))
        .args(["daemon", "start"])
        .env("XDG_RUNTIME_DIR", &runtime)
        .env("CAGENTS_TEST_RECOVERY_ACTIONS", &actions)
        .output()
        .unwrap();
    assert!(
        !output.status.success(),
        "an update handoff vetoes recovery"
    );
    assert_eq!(fs::read_to_string(&actions).unwrap().lines().count(), 3);
    drop(handoff);
    let output = Command::new(new.join("cagents"))
        .args(["daemon", "start"])
        .env("XDG_RUNTIME_DIR", &runtime)
        .env("CAGENTS_TEST_RECOVERY_ACTIONS", &actions)
        .output()
        .unwrap();
    assert!(
        !output.status.success(),
        "a success exit alone cannot authenticate the restored daemon"
    );
    assert_eq!(
        fs::read_to_string(&actions).unwrap(),
        "--json daemon status\ndaemon preflight\n--no-start daemon stop\ndaemon start\n"
    );
    drop(window);
    // A stopped owner does not authorize stale routing: the candidate reports
    // normal daemon status instead of invoking the old fixture CLI.
    let output = Command::new(new.join("cagents"))
        .args(["--json", "daemon", "status"])
        .env("XDG_RUNTIME_DIR", &runtime)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert!(
        String::from_utf8(output.stdout)
            .unwrap()
            .contains("\"running\":false")
    );
    // A candidate that started with no previous owner must remain usable. Its
    // own verified kernel daemon identity makes the old unknown marker stale.
    fs::write(&marker, r#"{"version":1,"owner_version":null}"#).unwrap();
    let candidate_lock = OpenOptions::new()
        .read(true)
        .write(true)
        .open(daemon_dir.join("cagentsd.lock"))
        .unwrap();
    assert_eq!(
        unsafe { libc::flock(candidate_lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0
    );
    let fd = candidate_lock.as_raw_fd();
    let mut command = Command::new(new.join("cagentsd"));
    command.arg("30");
    unsafe {
        command.pre_exec(move || {
            if libc::fcntl(fd, libc::F_SETFD, 0) == -1 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    }
    let candidate = OwnedProcess(command.spawn().unwrap());
    fs::write(
        daemon_dir.join("cagentsd.lock"),
        format!("{{\"pid\":{}}}", candidate.0.id()),
    )
    .unwrap();
    let output = Command::new(new.join("cagents"))
        .args(["--json", "daemon", "status"])
        .env("XDG_RUNTIME_DIR", &runtime)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert!(
        String::from_utf8(output.stdout)
            .unwrap()
            .contains("\"running\":true")
    );
    drop(candidate);
    drop(candidate_lock);
}

#[test]
fn staged_native_ui_owner_fixture() {
    let _fixture = OS_FIXTURE.lock().unwrap();
    let Some(expected) = std::env::var_os("CAGENTS_TEST_STAGED_NATIVE_OWNER") else {
        return;
    };
    let selected = cagents::control::staged_ui_executable().unwrap();
    if expected == "old" {
        assert_eq!(
            selected,
            Some(std::path::PathBuf::from(
                std::env::var_os("CAGENTS_TEST_STAGED_OWNER_PATH").unwrap()
            ))
        );
    } else {
        assert_eq!(
            selected, None,
            "reviewed compatible reopening must not bounce to the old UI"
        );
    }
}

#[test]
fn sealed_anonymous_update_probe_does_not_require_installed_routing() {
    let _fixture = OS_FIXTURE.lock().unwrap();
    use std::os::fd::FromRawFd;
    let root = tempfile::tempdir().unwrap();
    let runtime = root.path().join("runtime");
    fs::create_dir(&runtime).unwrap();
    fs::set_permissions(&runtime, fs::Permissions::from_mode(0o700)).unwrap();
    for sealed in [false, true] {
        let anonymous = if sealed {
            let fd = unsafe {
                libc::memfd_create(
                    c"cagents-update-probe".as_ptr(),
                    libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING,
                )
            };
            assert_ne!(fd, -1);
            unsafe { File::from_raw_fd(fd) }
        } else {
            let path = root.path().join("unlinked-update-probe");
            let file = file(&path);
            fs::remove_file(path).unwrap();
            file
        };
        let mut source = File::open(env!("CARGO_BIN_EXE_cagents")).unwrap();
        std::io::copy(&mut source, &mut &anonymous).unwrap();
        fs::set_permissions(
            format!("/proc/self/fd/{}", anonymous.as_raw_fd()),
            fs::Permissions::from_mode(0o700),
        )
        .unwrap();
        if sealed {
            assert_eq!(
                unsafe {
                    libc::fcntl(
                        anonymous.as_raw_fd(),
                        libc::F_ADD_SEALS,
                        libc::F_SEAL_WRITE
                            | libc::F_SEAL_SHRINK
                            | libc::F_SEAL_GROW
                            | libc::F_SEAL_SEAL,
                    )
                },
                0
            );
        }
        // A regular executable cannot remain open for writing during exec.
        // Preserve its unlinked inode through a read-only descriptor instead.
        let anonymous = if sealed {
            anonymous
        } else {
            let readable = File::open(format!("/proc/self/fd/{}", anonymous.as_raw_fd())).unwrap();
            drop(anonymous);
            readable
        };
        let fd = anonymous.as_raw_fd();
        let mut command = Command::new(format!("/proc/self/fd/{fd}"));
        command
            .args(["--json", "daemon", "status"])
            .env("XDG_CONFIG_HOME", root.path().join("config"))
            .env("XDG_DATA_HOME", root.path().join("data"))
            .env("XDG_CACHE_HOME", root.path().join("cache"))
            .env("XDG_RUNTIME_DIR", &runtime);
        unsafe {
            command.pre_exec(move || {
                if libc::fcntl(fd, libc::F_SETFD, 0) == -1 {
                    Err(std::io::Error::last_os_error())
                } else {
                    Ok(())
                }
            });
        }
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "anonymous probe sealed={sealed}: {output:?}"
        );
        let response: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(response["ok"], true);
        assert_eq!(response["command"], "daemon.status");
        assert_eq!(response["data"]["running"], false);
    }
}

fn executable(path: &Path, body: &str) {
    fs::write(path, body).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

/// A verified release whose CLI delegates to this build's real coordinator,
/// like a newer `dist/` release run against an installation that predates it.
fn coordinator_release(dist: &Path, version: &str) -> (std::path::PathBuf, std::path::PathBuf) {
    let top = dist.join("tree").join(format!("cagents-{version}"));
    fs::create_dir_all(top.join("bin")).unwrap();
    executable(
        &top.join("bin/cagents"),
        r#"#!/bin/sh
# Fixture only: an independent writer replaces current just as selection runs.
if [ "$1" = select-install ] && [ "$2" != --help ] && [ -n "$TEST_RACE_CURRENT" ]; then
  /usr/bin/ln -snf "$TEST_RACE_CURRENT" "$TEST_PREFIX/current" || exit 97
fi
exec "$TEST_REAL_CLI" "$@"
"#,
    );
    for binary in ["cagents-ui", "cagentsd", "cagents-uninstall"] {
        executable(
            &top.join("bin").join(binary),
            "#!/bin/sh\nprintf 'staged %s\\n' \"$0\" >> \"$TEST_LAUNCHES\"\nexit 90\n",
        );
    }
    fs::write(top.join("bin/.cagents-ui-lease-v1"), "").unwrap();
    for relative in [
        "share/applications/dev.cagents.Cagents.desktop",
        "share/autostart/dev.cagents.Cagents.Daemon.desktop",
        "share/icons/hicolor/scalable/apps/dev.cagents.Cagents.svg",
        "share/icons/hicolor/128x128/apps/dev.cagents.Cagents.png",
        "share/metainfo/dev.cagents.Cagents.metainfo.xml",
        "share/doc/cagents/LICENSE",
    ] {
        let file = top.join(relative);
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(file, "[Desktop Entry]\nExec=cagents ui\nIcon=cagents\n").unwrap();
    }
    let archive = dist.join(format!("cagents-{version}-x86_64-unknown-linux-gnu.tar.gz"));
    assert!(
        Command::new("tar")
            .arg("-czf")
            .arg(&archive)
            .arg("-C")
            .arg(top.parent().unwrap())
            .arg(top.file_name().unwrap())
            .status()
            .unwrap()
            .success()
    );
    let hash = Command::new("sha256sum").arg(&archive).output().unwrap();
    assert!(hash.status.success());
    let checksums = dist.join("SHA256SUMS");
    fs::write(
        &checksums,
        format!(
            "{}  {}\n",
            String::from_utf8(hash.stdout)
                .unwrap()
                .split_whitespace()
                .next()
                .unwrap(),
            archive.file_name().unwrap().to_str().unwrap()
        ),
    )
    .unwrap();
    (archive, checksums)
}

#[test]
fn selection_withdraws_and_restores_a_staged_build_without_touching_live_owners() {
    if !user_fixture::enter() {
        return;
    }
    let _fixture = OS_FIXTURE.lock().unwrap();
    let root = tempfile::tempdir_in("/var/tmp").unwrap();
    let root = root.path();
    let [home, data, config, cache, tmp, runtime, tools] =
        ["home", "data", "config", "cache", "tmp", "runtime", "tools"].map(|leaf| root.join(leaf));
    for directory in [&home, &data, &config, &cache, &tmp, &runtime, &tools] {
        fs::create_dir(directory).unwrap();
    }
    fs::set_permissions(&runtime, fs::Permissions::from_mode(0o700)).unwrap();
    for tool in ["gtk-update-icon-cache", "update-desktop-database"] {
        executable(&tools.join(tool), "#!/bin/sh\nexit 0\n");
    }
    let prefix = data.join("cagents-install");
    // The running tree carries this build's version so the window fixture can
    // drive the application's own offer and apply checks from inside it.
    let running = cagents::VERSION;
    let staged = "9.9.9";
    assert_ne!(running, staged);
    let old = prefix.join("versions").join(running).join("bin");
    fs::create_dir_all(&old).unwrap();
    fs::write(
        prefix.join(".cagents-install-root"),
        "cagents install root\n",
    )
    .unwrap();
    // Every invocation of an installed CLI is recorded: selection must not run
    // one, and routed launches must reach exactly the running owner's CLI.
    let old_cli = root.join("old-cli");
    let launches = root.join("staged-launches");
    executable(
        &old.join("cagents"),
        "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$TEST_OLD_CLI\"\nprintf '%s\\n' \"$*\"\n",
    );
    fs::copy("/bin/sleep", old.join("cagentsd")).unwrap();
    // This version was once staged itself. Its marker must stay obsolete.
    let obsolete = prefix
        .join("versions")
        .join(running)
        .join(".deferred-owner.json");
    fs::write(&obsolete, r#"{"version":1,"owner_version":"0.0.1"}"#).unwrap();
    fs::set_permissions(&obsolete, fs::Permissions::from_mode(0o600)).unwrap();
    std::os::unix::fs::symlink(Path::new("versions").join(running), prefix.join("current"))
        .unwrap();

    let daemon_dir = runtime.join(cagents::instance::RUNTIME_SUBDIRECTORY);
    fs::create_dir_all(&daemon_dir).unwrap();
    fs::set_permissions(&daemon_dir, fs::Permissions::from_mode(0o700)).unwrap();
    let lock = file(&daemon_dir.join("cagentsd.lock"));
    assert_eq!(
        unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0
    );
    let fd = lock.as_raw_fd();
    let mut command = Command::new(old.join("cagentsd"));
    command.arg("600");
    unsafe {
        command.pre_exec(move || {
            if libc::fcntl(fd, libc::F_SETFD, 0) == -1 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    }
    let mut daemon = OwnedProcess(command.spawn().unwrap());
    let daemon_pid = daemon.0.id();
    fs::write(
        daemon_dir.join("cagentsd.lock"),
        format!("{{\"pid\":{daemon_pid}}}"),
    )
    .unwrap();
    drop(lock);
    let mut window = OwnedProcess::window(
        &old.join("cagents-ui"),
        &runtime,
        &root.join("window-ready"),
    );
    let window_pid = window.0.id();

    let real_cli = root.join("coordinator/cagents");
    fs::create_dir(real_cli.parent().unwrap()).unwrap();
    fs::copy(env!("CARGO_BIN_EXE_cagents"), &real_cli).unwrap();
    let (archive, checksums) = coordinator_release(&root.join("dist"), staged);
    let install = |args: &[&str], race: Option<&str>| {
        let mut command = Command::new("sh");
        command
            .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("install.sh"))
            .args(args)
            .env("HOME", &home)
            .env("XDG_DATA_HOME", &data)
            .env("XDG_CONFIG_HOME", &config)
            .env("XDG_CACHE_HOME", &cache)
            .env("XDG_RUNTIME_DIR", &runtime)
            .env("TMPDIR", &tmp)
            .env("PATH", format!("{}:/usr/bin:/bin", tools.display()))
            .env("CAGENTS_ARCHIVE", &archive)
            .env("CAGENTS_CHECKSUMS", &checksums)
            .env("TEST_REAL_CLI", &real_cli)
            .env("TEST_PREFIX", &prefix)
            .env("TEST_OLD_CLI", &old_cli)
            .env("TEST_LAUNCHES", &launches)
            .env_remove("CAGENTS_UI_UPDATE_GUARD_FD");
        if let Some(race) = race {
            command.env("TEST_RACE_CURRENT", race);
        }
        command.output().unwrap()
    };
    let select = |version: &str| install(&["--select", version, "--stage-only"], None);
    let current = || fs::read_link(prefix.join("current")).unwrap();
    let canonical = prefix.canonicalize().unwrap();
    let staged_ui = canonical
        .join("versions")
        .join(staged)
        .join("bin/cagents-ui");
    // Runs inside the live window's own executable: the offer it shows, and
    // what its Update and reopen control does with a previously shown offer.
    let offer = |stale: Option<&Path>| {
        let report = root.join("offer-report");
        let _ = fs::remove_file(&report);
        let mut command = Command::new(old.join("cagents-ui"));
        command
            .args(["--exact", "selection_offer_fixture", "--nocapture"])
            .env("CAGENTS_TEST_SELECTION_OFFER", &report)
            .env("XDG_RUNTIME_DIR", &runtime)
            .env("XDG_DATA_HOME", &data)
            .env("XDG_CONFIG_HOME", &config)
            .env("XDG_CACHE_HOME", &cache);
        if let Some(stale) = stale {
            command.env("CAGENTS_TEST_STALE_OFFER", stale);
        }
        let output = command.output().unwrap();
        assert!(output.status.success(), "{output:?}");
        serde_json::from_slice::<serde_json::Value>(&fs::read(report).unwrap()).unwrap()
    };
    // A launch through current, by the native resolver the desktop entry uses.
    let launch_route = |expected: &str| {
        let output = Command::new(prefix.join("current/bin/cagents-ui"))
            .args(["--exact", "staged_native_ui_owner_fixture", "--nocapture"])
            .env("CAGENTS_TEST_STAGED_NATIVE_OWNER", expected)
            .env(
                "CAGENTS_TEST_STAGED_OWNER_PATH",
                canonical
                    .join("versions")
                    .join(running)
                    .join("bin/cagents-ui"),
            )
            .env("XDG_RUNTIME_DIR", &runtime)
            .output()
            .unwrap();
        assert!(output.status.success(), "{expected}: {output:?}");
    };
    let owners_untouched = |daemon: &mut OwnedProcess, window: &mut OwnedProcess| {
        assert!(
            daemon.0.try_wait().unwrap().is_none(),
            "daemon must keep running"
        );
        assert!(
            window.0.try_wait().unwrap().is_none(),
            "window must keep running"
        );
        assert_eq!(daemon.0.id(), daemon_pid);
        assert_eq!(window.0.id(), window_pid);
        let lease = File::open(daemon_dir.join("cagentsd.lock")).unwrap();
        assert_eq!(
            unsafe { libc::flock(lease.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            -1,
            "the original daemon must still hold its lease"
        );
        assert_eq!(fs::read_to_string(&old_cli).unwrap_or_default(), "");
        assert!(!launches.exists(), "no staged executable may run");
    };

    let output = install(&["--stage-only"], None);
    assert!(output.status.success(), "{output:?}");
    assert_eq!(current(), Path::new("versions").join(staged));
    let marker = prefix
        .join("versions")
        .join(staged)
        .join(".deferred-owner.json");
    let recorded = fs::read(&marker).unwrap();
    let recorded_json: serde_json::Value = serde_json::from_slice(&recorded).unwrap();
    assert_eq!(recorded_json["owner_version"], running);
    // Give the staged tree a real native resolver, as an actual release has.
    fs::copy(std::env::current_exe().unwrap(), &staged_ui).unwrap();
    assert_eq!(offer(None)["offered"], staged_ui.to_str().unwrap());
    launch_route("old");
    owners_untouched(&mut daemon, &mut window);

    // Withdraw: serve the version the window and daemon already run.
    let output = select(running);
    assert!(output.status.success(), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stdout).contains("Serving installed cagents"));
    assert_eq!(current(), Path::new("versions").join(running));
    assert_eq!(
        fs::read(&marker).unwrap(),
        recorded,
        "staged routing is kept"
    );
    assert_eq!(
        fs::read_to_string(&obsolete).unwrap(),
        r#"{"version":1,"owner_version":"0.0.1"}"#
    );
    let withdrawn = offer(Some(&staged_ui));
    assert_eq!(withdrawn["offered"], serde_json::Value::Null, "{withdrawn}");
    assert!(
        withdrawn["refused"]
            .as_str()
            .unwrap()
            .contains("the installed candidate changed"),
        "a previously shown offer must not apply: {withdrawn}"
    );
    // The running version launches itself; its own old marker stays obsolete.
    launch_route("candidate");
    owners_untouched(&mut daemon, &mut window);

    // Refusals leave current, every tree and both live owners exactly as found.
    let refuse = |output: Output, reason: &str| {
        assert!(!output.status.success(), "{reason}: {output:?}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains(reason), "{reason}: {stderr}");
        assert_eq!(current(), Path::new("versions").join(running), "{reason}");
    };
    let incomplete = prefix.join("versions/9.9.8/bin");
    fs::create_dir_all(&incomplete).unwrap();
    for name in ["cagents", "cagents-ui"] {
        executable(&incomplete.join(name), "#!/bin/sh\nexit 90\n");
    }
    refuse(
        select("9.9.8"),
        "is not a complete installation of this prefix",
    );
    let elsewhere = root.join("elsewhere");
    let foreign = elsewhere.join("versions/9.9.6/bin");
    fs::create_dir_all(&foreign).unwrap();
    fs::write(
        elsewhere.join(".cagents-install-root"),
        "cagents install root\n",
    )
    .unwrap();
    for name in ["cagents", "cagents-ui", "cagentsd"] {
        executable(&foreign.join(name), "#!/bin/sh\nexit 90\n");
    }
    std::os::unix::fs::symlink(
        elsewhere.join("versions/9.9.6"),
        prefix.join("versions/9.9.6"),
    )
    .unwrap();
    refuse(select("9.9.6"), "is not an installed version directory");
    let alias = prefix.join("versions/9.9.4");
    fs::create_dir(&alias).unwrap();
    std::os::unix::fs::symlink(&foreign, alias.join("bin")).unwrap();
    refuse(
        select("9.9.4"),
        "version 9.9.4 belongs to another installation",
    );
    let unstaged = prefix.join("versions/9.9.7/bin");
    fs::create_dir_all(&unstaged).unwrap();
    for name in ["cagents", "cagents-ui", "cagentsd"] {
        executable(&unstaged.join(name), "#!/bin/sh\nexit 90\n");
    }
    refuse(
        select("9.9.7"),
        &format!("9.9.7 was not staged over the running version {running}"),
    );
    refuse(select("9.9.5"), "version 9.9.5 is not installed");
    let output = install(&["--select", staged], None);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("--select requires --stage-only"));
    // An update job holds its lease from review through completion.
    // Selection takes the same private lease, which the withdrawal created.
    let job = OpenOptions::new()
        .read(true)
        .write(true)
        .open(prefix.join(".update/lock"))
        .unwrap();
    assert_eq!(
        unsafe { libc::flock(job.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0
    );
    refuse(select(staged), "an application update is in progress");
    drop(job);
    // A link written after the installer inspected current is kept rather
    // than overwritten. (The exchange-time comparison is a unit test.)
    let independent = Path::new("versions/9.9.7");
    let output = install(&["--select", staged, "--stage-only"], independent.to_str());
    assert!(!output.status.success(), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("current installation changed while preparing the selection"),
        "{output:?}"
    );
    assert_eq!(current(), independent);
    fs::remove_file(prefix.join("current")).unwrap();
    std::os::unix::fs::symlink(Path::new("versions").join(running), prefix.join("current"))
        .unwrap();
    owners_untouched(&mut daemon, &mut window);

    // Reselect: the staged build and its offer return exactly as staged.
    let output = select(staged);
    assert!(output.status.success(), "{output:?}");
    assert_eq!(current(), Path::new("versions").join(staged));
    assert_eq!(fs::read(&marker).unwrap(), recorded);
    assert_eq!(offer(None)["offered"], staged_ui.to_str().unwrap());
    launch_route("old");
    // Selecting what is already served changes nothing.
    let output = select(staged);
    assert!(output.status.success(), "{output:?}");
    assert_eq!(current(), Path::new("versions").join(staged));
    owners_untouched(&mut daemon, &mut window);
    for version in [running, staged, "9.9.7", "9.9.8"] {
        assert!(
            prefix.join("versions").join(version).join("bin").is_dir(),
            "{version}"
        );
    }
    assert!(
        fs::read_dir(&prefix).unwrap().all(|entry| !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".select.")),
        "selection must not leave temporary links"
    );

    // With only the window left, it alone is the owner: withdrawing still
    // works, and a marker that relied on sharing the stopped daemon cannot be
    // restored, since its candidate would start a second daemon beside it.
    daemon.stop_checked().unwrap();
    drop(daemon);
    let output = select(running);
    assert!(output.status.success(), "{output:?}");
    assert_eq!(current(), Path::new("versions").join(running));
    fs::write(
        &marker,
        serde_json::to_vec(&serde_json::json!({
            "version": 1, "owner_version": running, "daemon_compatible": true,
            "candidate_protocol": cagents::terminal_ipc::PROTOCOL_VERSION,
        }))
        .unwrap(),
    )
    .unwrap();
    refuse(select(staged), "daemon, which is no longer running");
    fs::write(&marker, &recorded).unwrap();
    let output = select(staged);
    assert!(output.status.success(), "{output:?}");
    assert_eq!(current(), Path::new("versions").join(staged));
    assert!(window.0.try_wait().unwrap().is_none());
    assert_eq!(window.0.id(), window_pid);
    assert_eq!(fs::read_to_string(&old_cli).unwrap_or_default(), "");
    // With nothing running, nothing can be routed: any complete tree serves.
    window.stop_checked().unwrap();
    let output = select("9.9.7");
    assert!(output.status.success(), "{output:?}");
    assert_eq!(current(), Path::new("versions/9.9.7"));
    let output = install(&["--select", "", "--stage-only"], None);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("--select requires a version"));
    assert_eq!(current(), Path::new("versions/9.9.7"));
    // A stripped version prefix must remain a selection error, rather than
    // falling through to ordinary installation of the coordinator release.
    let output = install(&["--select", "v", "--stage-only"], None);
    assert!(!output.status.success(), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("unsafe selected version: "),
        "{output:?}"
    );
    assert_eq!(current(), Path::new("versions/9.9.7"));
    assert_eq!(fs::read(&marker).unwrap(), recorded);
    assert!(!launches.exists(), "no staged executable may run");
}

#[test]
fn selection_offer_fixture() {
    let Some(report) = std::env::var_os("CAGENTS_TEST_SELECTION_OFFER") else {
        return;
    };
    let mut result = serde_json::json!({ "offered": cagents::update::superseding_ui() });
    if let Some(stale) = std::env::var_os("CAGENTS_TEST_STALE_OFFER") {
        result["refused"] = cagents::update::apply_installed(Path::new(&stale))
            .unwrap_err()
            .to_string()
            .into();
    }
    fs::write(report, result.to_string()).unwrap();
}
