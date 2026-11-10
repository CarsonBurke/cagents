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
print(json.dumps({'Err':'fixture source panes refused'} if os.environ['FIXTURE_SOURCE_REFUSED']=='1' else {'Ok':{'stopped':[], 'stopped_windows':[], 'labels':{}}}))
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
        fs::write(
            &script,
            r#"#!/bin/sh
set -eu
"$TEST_CLI" verify-install-guard --stage-only
if "$TEST_CLI" verify-install-guard; then exit 91; fi
printf ready > "$TEST_READY"
case "$TEST_STATUS" in
  0) : ;;
  7) exit 7 ;;
  cancel) sleep 30 ;;
esac
"#,
        )
        .unwrap();
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
            Path::new("versions/old")
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
