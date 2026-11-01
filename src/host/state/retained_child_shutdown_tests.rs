//! Already-published external original custody must not look like an empty host.
//! These are private synthetic process_request fixtures, not a delegated CLI
//! user-success claim, an exit-wide fence or a new cleanup/signal policy.
use super::*;
use crate::{
    daemon::child_reaper,
    terminal_ipc::{ClientRequest, SHUTDOWN_VERSION, ServerResponse},
};
use std::{
    fs::File,
    io,
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::fs::PermissionsExt,
    },
    path::Path,
    process::{Child, ChildStdin, Command, Stdio},
};

const ROOT: &str = "CAGENTS_RETAINED_CHILD_ADMISSION_ROOT";
const SCOPE: &str = "CAGENTS_RETAINED_CHILD_ADMISSION_AUTHORITY_PRESENCE";

struct TestChild(Child);

impl Drop for TestChild {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

// Keep private runtime paths and captured child output until the external
// command owner has checked the whole family, on success as well as failure.
struct EvidenceDirectory(Option<tempfile::TempDir>);

impl Drop for EvidenceDirectory {
    fn drop(&mut self) {
        if let Some(directory) = self.0.take() {
            let _retained = directory.keep();
        }
    }
}

fn private_runtime(test: &str) -> Option<PathBuf> {
    if let Some(root) = std::env::var_os(ROOT) {
        let expected = match std::env::var(SCOPE).as_deref() {
            Ok("present") => true,
            Ok("absent") => false,
            _ => panic!("missing or invalid retained-child fixture authority presence marker"),
        };
        assert_eq!(
            crate::agent_execution::Authority::from_environment()
                != crate::agent_execution::Authority::default(),
            expected,
            "isolated retained-child fixture lost its inherited authority scope"
        );
        return Some(PathBuf::from(root).canonicalize().unwrap());
    }

    // Honour the command owner's legitimate non-repository TMPDIR. A public
    // /tmp or /var/tmp checkout marker is neither removed nor bypassed.
    let directory = tempfile::Builder::new().prefix("rc-").tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let _evidence = EvidenceDirectory(Some(directory));
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
    for name in ["home", "run", "data", "config", "cache", "tmp"] {
        fs::create_dir(root.join(name)).unwrap();
        fs::set_permissions(root.join(name), fs::Permissions::from_mode(0o700)).unwrap();
    }
    let stdout = root.join("test.stdout");
    let stderr = root.join("test.stderr");
    let mut command = Command::new(std::env::current_exe().unwrap());
    // Leave delegation inherited and opaque. Only public Authority equality
    // crosses the boundary as a boolean assertion; no capability is copied.
    for (name, _) in std::env::vars_os() {
        if name != std::ffi::OsStr::new(crate::agent_execution::AUTHORITY_ENV) {
            command.env_remove(name);
        }
    }
    command
        .args(["--exact", test, "--nocapture", "--test-threads=1"])
        .current_dir(&root)
        .env(ROOT, &root)
        .env(
            SCOPE,
            if crate::agent_execution::Authority::from_environment()
                != crate::agent_execution::Authority::default()
            {
                "present"
            } else {
                "absent"
            },
        )
        .env("HOME", root.join("home"))
        .env("XDG_RUNTIME_DIR", root.join("run"))
        .env("XDG_DATA_HOME", root.join("data"))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_CACHE_HOME", root.join("cache"))
        .env("TMPDIR", root.join("tmp"))
        .env("PATH", "/usr/bin:/bin")
        .stdin(Stdio::null())
        .stdout(Stdio::from(File::create(&stdout).unwrap()))
        .stderr(Stdio::from(File::create(&stderr).unwrap()));
    let mut child = TestChild(command.spawn().unwrap());
    let deadline = Instant::now() + Duration::from_secs(30);
    let status = loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            break Some(status);
        }
        if Instant::now() >= deadline {
            child.0.kill().unwrap();
            child.0.wait().unwrap();
            break None;
        }
        thread::sleep(Duration::from_millis(10));
    };
    let stdout = fs::read_to_string(stdout).unwrap();
    let stderr = fs::read_to_string(stderr).unwrap();
    assert!(
        status.is_some_and(|status| status.success()),
        "isolated retained-child fixture failed or exceeded 30s; retained {}:\n{stdout}\n{stderr}",
        root.display()
    );
    assert!(
        stdout.contains(&format!("test {test} ... ok")),
        "missing exact child test; retained {}:\n{stdout}",
        root.display()
    );
    assert!(
        stdout.contains("test result: ok. 1 passed; 0 failed; 0 ignored;"),
        "zero or incorrect child selection; retained {}:\n{stdout}",
        root.display()
    );
    None
}

fn host() -> SharedHost {
    let mut state = HostState::new(HostSessionProof {
        host_session_id: crate::model::HostSessionId::new("retained-child-admission"),
        boot_id: crate::model::BootId::new("retained-child-admission"),
        host_incarnation: crate::model::HostIncarnation::new("retained-child-admission"),
    });
    // This private direct request fixture is an explicit user-principal client;
    // it never substitutes for inherited/delegated CLI authentication.
    state.register_client(1);
    Arc::new(HostLock::new(state))
}

fn request(
    host: &SharedHost,
    request: ClientRequest,
) -> Result<
    (
        ServerResponse,
        Vec<crate::terminal_ipc::ServerMessage>,
        bool,
    ),
    ProtocolError,
> {
    super::super::client::process_request(host, 1, request)
}

fn preflight(host: &SharedHost) -> ShutdownCapability {
    let (response, effects, stopping) = request(
        host,
        ClientRequest::ShutdownV1 {
            version: SHUTDOWN_VERSION,
            operation: ShutdownOperation::Preflight,
        },
    )
    .unwrap();
    assert!(effects.is_empty());
    assert!(!stopping);
    let ServerResponse::ShutdownPreflight(preflight) = response else {
        panic!("actual preflight response: {response:?}");
    };
    preflight.capability
}

fn assert_empty(host: &SharedHost) {
    let (response, effects, stopping) = request(host, ClientRequest::List).unwrap();
    assert!(effects.is_empty());
    assert!(!stopping);
    assert!(matches!(response, ServerResponse::Listed { terminals } if terminals.is_empty()));
}

fn state_stamp(host: &SharedHost) -> (bool, bool, usize, u64, bool) {
    let state = host.lock().unwrap();
    (
        state.shutting_down,
        state.shutdown_requested,
        state.shutdown_capabilities.len(),
        state.inventory_generation,
        state.upgrade.is_some(),
    )
}

fn refuse_all(host: &SharedHost, capability: &ShutdownCapability) {
    assert_empty(host);
    let before = state_stamp(host);
    let operations = [
        ClientRequest::Shutdown,
        ClientRequest::ShutdownV1 {
            version: SHUTDOWN_VERSION,
            operation: ShutdownOperation::Preflight,
        },
        ClientRequest::ShutdownV1 {
            version: SHUTDOWN_VERSION,
            operation: ShutdownOperation::Stop {
                require_empty: true,
                expected_generation: None,
            },
        },
        ClientRequest::ShutdownV1 {
            version: SHUTDOWN_VERSION,
            operation: ShutdownOperation::Stop {
                require_empty: false,
                expected_generation: None,
            },
        },
        ClientRequest::ShutdownV1 {
            version: SHUTDOWN_VERSION,
            operation: ShutdownOperation::Confirm {
                capability: capability.clone(),
            },
        },
    ];
    for operation in operations {
        let error = request(host, operation).unwrap_err();
        assert_eq!(error.code, ProtocolErrorCode::TerminalUnavailable);
        assert!(error.retryable);
        assert_eq!(error.message, RETAINED_CHILD_CLEANUP_MESSAGE);
        assert_eq!(
            state_stamp(host),
            before,
            "refusal cannot issue a capability or commit shutdown"
        );
    }
    let error = request(
        host,
        ClientRequest::PrepareUpgrade {
            capability: capability.clone(),
            relaunch_agents: false,
        },
    )
    .unwrap_err();
    // Existing upgrade validation keeps its existing error projection; no new
    // wire variant or fake ShutdownOutcome is used.
    assert_eq!(error.code, ProtocolErrorCode::InvalidRequest);
    assert!(!error.retryable);
    assert_eq!(error.message, RETAINED_CHILD_CLEANUP_MESSAGE);
    assert_eq!(state_stamp(host), before);
    let error = request(
        host,
        ClientRequest::ShutdownV1 {
            version: SHUTDOWN_VERSION + 1,
            operation: ShutdownOperation::Preflight,
        },
    )
    .unwrap_err();
    assert_eq!(error.code, ProtocolErrorCode::UnsupportedVersion);
    assert_eq!(state_stamp(host), before);
    assert!(child_reaper::has_retained_ownership());
}

fn accepted_empty_stop(host: &SharedHost) {
    let (response, effects, stopping) = request(
        host,
        ClientRequest::ShutdownV1 {
            version: SHUTDOWN_VERSION,
            operation: ShutdownOperation::Stop {
                require_empty: true,
                expected_generation: None,
            },
        },
    )
    .unwrap();
    assert!(effects.is_empty());
    assert!(stopping);
    assert!(matches!(
        response,
        ServerResponse::ShutdownOutcome(ShutdownOutcome::Accepted {
            live_terminals_before: 0,
            termination_requested: true,
        })
    ));
}

fn epoch(pid: u32) -> String {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
    stat.rsplit_once(')')
        .unwrap()
        .1
        .split_whitespace()
        .nth(19)
        .unwrap()
        .into()
}

struct Original {
    child: Option<Child>,
    input: Option<ChildStdin>,
    pidfd: File,
    pid: u32,
    start: String,
    settled: bool,
}
impl Original {
    fn spawn(root: &Path) -> Self {
        let mut child = Command::new("/bin/cat")
            .current_dir(root)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let pid = child.id();
        let descriptor = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
        if descriptor < 0 {
            let error = io::Error::last_os_error();
            let _ = child.kill();
            let _ = child.wait();
            panic!("original-child pidfd: {error}");
        }
        let input = child.stdin.take();
        let mut original = Self {
            child: Some(child),
            input,
            pidfd: unsafe { File::from_raw_fd(descriptor as i32) },
            pid,
            start: String::new(),
            settled: false,
        };
        original.start = epoch(pid);
        assert_eq!(fs::read_link(format!("/proc/{pid}/cwd")).unwrap(), root);
        original
    }
    fn assert_live(&mut self) {
        assert_eq!(epoch(self.pid), self.start);
        let mut descriptor = libc::pollfd {
            fd: self.pidfd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        assert_eq!(unsafe { libc::poll(&mut descriptor, 1, 0) }, 0);
        if let Some(child) = &mut self.child {
            assert!(child.try_wait().unwrap().is_none());
        }
    }
    fn retain(&mut self, reservation: child_reaper::Reservation) {
        let pidfd = self.pidfd.try_clone().unwrap();
        let child = self.child.take().unwrap();
        if let Err((error, child, _, reservation)) = reservation.retain(child, pidfd) {
            self.child = Some(child);
            drop(reservation);
            panic!("original ownership transfer: {error}");
        }
    }
    fn exit_ready(&self) -> bool {
        let mut descriptor = libc::pollfd {
            fd: self.pidfd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        assert!(unsafe { libc::poll(&mut descriptor, 1, 0) } >= 0);
        descriptor.revents & libc::POLLIN != 0
    }
    fn wait_exit(&self) {
        let by = Instant::now() + Duration::from_secs(5);
        while !self.exit_ready() {
            assert!(Instant::now() < by, "original child did not exit");
            thread::sleep(Duration::from_millis(2));
        }
    }
    fn finish_caller_owned(&mut self, reservation: child_reaper::Reservation) {
        drop(self.input.take());
        self.wait_exit();
        assert!(self.child.take().unwrap().wait().unwrap().success());
        // Direct original wait must precede releasing even a quarantined
        // caller-held credit; no live child temporarily disappears from custody.
        drop(reservation);
        self.finish();
    }
    fn finish(&mut self) {
        drop(self.input.take());
        self.wait_exit();
        if let Some(mut child) = self.child.take() {
            assert!(child.wait().unwrap().success());
        }
        let by = Instant::now() + Duration::from_secs(5);
        while child_reaper::has_retained_ownership() {
            assert!(
                Instant::now() < by,
                "original ownership not actually reaped"
            );
            thread::sleep(Duration::from_millis(2));
        }
        self.wait_exit();
        match fs::read_to_string(format!("/proc/{}/stat", self.pid)) {
            Ok(stat) => assert_ne!(
                stat.rsplit_once(')')
                    .unwrap()
                    .1
                    .split_whitespace()
                    .nth(19)
                    .unwrap(),
                self.start,
                "the original incarnation must be reaped, not only exited",
            ),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => panic!("could not inspect original reap: {error}"),
        }
        self.settled = true;
    }
}
impl Drop for Original {
    fn drop(&mut self) {
        if self.settled {
            return;
        }
        // Assertion-failure cleanup uses the original kernel capability, never
        // a fresh numeric PID lookup. Normal success closes stdin and reaps.
        drop(self.input.take());
        let _ = unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                self.pidfd.as_raw_fd(),
                libc::SIGKILL,
                std::ptr::null::<libc::siginfo_t>(),
                0,
            )
        };
        if let Some(mut child) = self.child.take() {
            let _ = child.wait();
        }
        let _ = child_reaper::retry_preserved();
        let by = Instant::now() + Duration::from_secs(5);
        while child_reaper::has_retained_ownership() && Instant::now() < by {
            thread::sleep(Duration::from_millis(2));
        }
    }
}

#[test]
fn retained_original_child_refuses_empty_control_until_exact_reap() {
    const NAME: &str = "host::state::retained_child_shutdown_tests::retained_original_child_refuses_empty_control_until_exact_reap";
    let Some(root) = private_runtime(NAME) else {
        return;
    };
    let host = host();
    let capability = preflight(&host);
    assert_eq!(capability.live_terminals, 0);
    let reservation = child_reaper::reserve().unwrap();
    let mut original = Original::spawn(&root);
    original.assert_live();
    original.retain(reservation);
    original.assert_live();
    refuse_all(&host, &capability);
    original.assert_live();
    original.finish();
    assert!(!child_reaper::has_outstanding_ownership());
    assert_empty(&host);
    accepted_empty_stop(&host);
}

#[test]
fn retained_original_refuses_parent_upgrade_but_not_an_isolated_upgrade_consumer() {
    const NAME: &str = "host::state::retained_child_shutdown_tests::retained_original_refuses_parent_upgrade_but_not_an_isolated_upgrade_consumer";
    const CONSUMER: &str =
        "host::state::tests::committed_upgrade_fences_control_and_retains_checkpoint_until_close";
    let Some(root) = private_runtime(NAME) else {
        return;
    };
    let host = host();
    let capability = preflight(&host);
    let reservation = child_reaper::reserve().unwrap();
    let mut original = Original::spawn(&root);
    original.retain(reservation);
    original.assert_live();
    refuse_all(&host, &capability);

    // Keep the original stdin and pidfd throughout the synchronous child run.
    // Its exact existing consumer must complete prepare/commit using a fresh
    // reaper, while this producer's global custody keeps parent admission blocked.
    super::tests::run_upgrade_fixture_in_fresh_process(CONSUMER);

    original.assert_live();
    refuse_all(&host, &capability);
    original.finish();
    assert!(!child_reaper::has_outstanding_ownership());
    accepted_empty_stop(&host);
}

#[test]
fn quarantined_and_suspended_originals_keep_the_empty_inventory_refused() {
    const NAME: &str = "host::state::retained_child_shutdown_tests::quarantined_and_suspended_originals_keep_the_empty_inventory_refused";
    let Some(root) = private_runtime(NAME) else {
        return;
    };
    let host = host();
    let capability = preflight(&host);
    let reservation = child_reaper::reserve().unwrap();
    let mut original = Original::spawn(&root);
    reservation.quarantine();
    original.assert_live();
    refuse_all(&host, &capability);
    original.finish_caller_owned(reservation);
    assert!(!child_reaper::has_outstanding_ownership());

    let reservation = child_reaper::reserve().unwrap();
    let mut original = Original::spawn(&root);
    original.assert_live();
    let pidfd = original.pidfd.try_clone().unwrap();
    reservation.preserve_suspended(
        original.child.take().unwrap(),
        Some(pidfd),
        "private admission regression",
    );
    refuse_all(&host, &capability);
    drop(original.input.take());
    original.wait_exit();
    // An exited suspended Child is still owned until the original reaper
    // explicitly retries with its retained valid pidfd and actually waits.
    refuse_all(&host, &capability);
    child_reaper::retry_preserved().unwrap();
    original.finish();
    assert!(!child_reaper::has_outstanding_ownership());
    accepted_empty_stop(&host);
}

#[test]
fn normal_caller_spawn_credit_keeps_the_counted_confirmation_contract() {
    const NAME: &str = "host::state::retained_child_shutdown_tests::normal_caller_spawn_credit_keeps_the_counted_confirmation_contract";
    let Some(root) = private_runtime(NAME) else {
        return;
    };
    let host = host();
    let reservation = child_reaper::reserve().unwrap();
    let mut original = Original::spawn(&root);
    original.assert_live();
    assert!(child_reaper::has_outstanding_ownership());
    assert!(!child_reaper::has_retained_ownership());
    host.lock()
        .unwrap()
        .reserve_spawn(TerminalId::new())
        .unwrap();
    let capability = preflight(&host);
    assert_eq!(capability.live_terminals, 1);
    let (response, _, stopping) = request(
        &host,
        ClientRequest::ShutdownV1 {
            version: SHUTDOWN_VERSION,
            operation: ShutdownOperation::Stop {
                require_empty: true,
                expected_generation: None,
            },
        },
    )
    .unwrap();
    assert!(!stopping);
    assert!(matches!(
        response,
        ServerResponse::ShutdownOutcome(ShutdownOutcome::EmptyRequired { current_count: 1 })
    ));
    let (response, effects, stopping) = request(
        &host,
        ClientRequest::ShutdownV1 {
            version: SHUTDOWN_VERSION,
            operation: ShutdownOperation::Confirm { capability },
        },
    )
    .unwrap();
    assert!(effects.is_empty());
    assert!(stopping);
    assert!(matches!(
        response,
        ServerResponse::ShutdownOutcome(ShutdownOutcome::Accepted {
            live_terminals_before: 1,
            termination_requested: true
        })
    ));
    original.assert_live();
    original.finish_caller_owned(reservation);
    assert!(!child_reaper::has_outstanding_ownership());
}

#[test]
fn upgrade_rechecks_original_custody_published_after_its_first_validation() {
    const NAME: &str = "host::state::retained_child_shutdown_tests::upgrade_rechecks_original_custody_published_after_its_first_validation";
    let Some(root) = private_runtime(NAME) else {
        return;
    };
    let host = host();
    let capability = preflight(&host);
    let before = state_stamp(&host);
    let store = crate::upgrade::handoff::Store::open(&root.join("checkpoint")).unwrap();
    let checkpoint_path = store.path.clone();
    assert!(store.load().unwrap().is_none());
    let reservation = child_reaper::reserve().unwrap();
    let mut original = Original::spawn(&root);
    let error = prepare_upgrade_with(&host, capability, || {
        original.retain(reservation);
        original.assert_live();
        Ok(store)
    })
    .unwrap_err();
    assert_eq!(error.code, ProtocolErrorCode::InvalidRequest);
    assert_eq!(error.message, RETAINED_CHILD_CLEANUP_MESSAGE);
    assert_eq!(state_stamp(&host), before);
    assert!(
        !checkpoint_path.exists(),
        "late refusal must precede durable checkpoint publication"
    );
    original.assert_live();
    original.finish();
    assert!(!child_reaper::has_outstanding_ownership());
    accepted_empty_stop(&host);
}
