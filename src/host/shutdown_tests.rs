use super::*;
use crate::{
    model::{LaunchRecipe, TerminalContinuity, TerminalId, TerminalSession},
    terminal::{TerminalConfig, TerminalHandle, TerminalSendError},
};
use std::{
    fs::File,
    os::fd::{AsRawFd, FromRawFd},
    path::Path,
    process::{Child, Command, Stdio},
    sync::mpsc,
    time::{Duration, Instant},
};

const TEST: &str = "host::shutdown_tests::shutdown_batch_continues_after_an_unfinished_worker";
const ROOT: &str = "CAGENTS_SHUTDOWN_BATCH_ROOT";

struct TestChild(Child);

impl Drop for TestChild {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

fn session(id: u128, root: &Path) -> TerminalSession {
    let recipe = LaunchRecipe {
        program: "/bin/sh".into(),
        arguments: vec![
            "-c".into(),
            "printf '%s' \"$$\" > root-pid; exec /bin/sleep 600".into(),
        ],
        working_directory: root.to_owned(),
        environment: Default::default(),
        unset_environment: Default::default(),
    };
    TerminalSession {
        id: TerminalId::from_uuid(Uuid::from_u128(id)),
        launch: recipe.clone(),
        transcript_path: None,
        resume_hint: None,
        owned_codex_listener: None,
        recovery_origin: None,
        predecessor: None,
        continuity: TerminalContinuity::RelaunchAvailable {
            recipe,
            last_exit: None,
        },
    }
}

// The fixture root writes its PID in its private cwd. Pin its incarnation
// before starting shutdown; failure cleanup never signals a numeric PID.
struct RootPin(File);

impl Drop for RootPin {
    fn drop(&mut self) {
        unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                self.0.as_raw_fd(),
                libc::SIGKILL,
                std::ptr::null::<libc::siginfo_t>(),
                0,
            );
        }
    }
}

fn start_time(pid: u32) -> io::Result<String> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat"))?;
    Ok(stat.rsplit_once(')').unwrap().1.split_whitespace().nth(19).unwrap().into())
}

fn private_runtime(test: &str) -> Option<std::path::PathBuf> {
    let Some(root) = std::env::var_os(ROOT) else {
        // Do not mutate process-wide XDG variables in a parallel test runner.
        let directory = tempfile::tempdir_in("/var/tmp").unwrap();
        let root = directory.path().canonicalize().unwrap();
        for name in ["home", "run", "data", "config", "cache"] {
            fs::create_dir(root.join(name)).unwrap();
            fs::set_permissions(root.join(name), fs::Permissions::from_mode(0o700)).unwrap();
        }
        let stdout = root.join("test.stdout");
        let stderr = root.join("test.stderr");
        let mut child = TestChild(Command::new(std::env::current_exe().unwrap())
            .args(["--exact", test, "--nocapture"])
            .env_clear()
            .env(ROOT, &root)
            .env("HOME", root.join("home"))
            .env("XDG_RUNTIME_DIR", root.join("run"))
            .env("XDG_DATA_HOME", root.join("data"))
            .env("XDG_CONFIG_HOME", root.join("config"))
            .env("XDG_CACHE_HOME", root.join("cache"))
            .env("PATH", "/usr/bin:/bin")
            .stdout(Stdio::from(File::create(&stdout).unwrap()))
            .stderr(Stdio::from(File::create(&stderr).unwrap()))
            .spawn()
            .unwrap());
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
        let failure_root = if status.is_some_and(|status| status.success()) {
            None
        } else {
            // A timed-out child may still have discoverable private holders;
            // keep their evidence rather than unlinking their private runtime.
            Some(directory.keep())
        };
        assert!(
            status.is_some_and(|status| status.success()),
            "isolated shutdown batch failed or exceeded 30s; retained {failure_root:?}:\n{stdout}\n{stderr}"
        );
        assert!(stdout.contains("1 passed"));
        return None;
    };
    Some(std::path::PathBuf::from(root).canonicalize().unwrap())
}

#[test]
fn shutdown_batch_continues_after_an_unfinished_worker() {
    let Some(root) = private_runtime(TEST) else { return; };
    let mut state = HostState::new(HostSessionProof {
        host_session_id: HostSessionId::new("shutdown-batch"),
        boot_id: crate::model::BootId::new("shutdown-batch"),
        host_incarnation: HostIncarnation::new("shutdown-batch"),
    });
    state.register_client(1);
    let (wedged, release) = TerminalHandle::spawn_with_wedged_worker_for_test();
    state.finish_spawn(1, session(1, &root), wedged.clone(), true).unwrap();
    let healthy = TerminalHandle::spawn(session(2, &root), TerminalConfig::default(), |_| {}).unwrap();
    state.finish_spawn(1, session(2, &root), healthy.clone(), true).unwrap();
    let ready_by = Instant::now() + Duration::from_secs(5);
    let pid: u32 = loop {
        if let Ok(pid) = fs::read_to_string(root.join("root-pid")) {
            if let Ok(pid) = pid.trim().parse() {
                break pid;
            }
        }
        assert!(Instant::now() < ready_by, "private PTY root did not start");
        thread::sleep(Duration::from_millis(5));
    };
    let original_start = start_time(pid).unwrap();
    assert_eq!(fs::read_link(format!("/proc/{pid}/cwd")).unwrap(), root);
    let descriptor = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
    assert!(descriptor >= 0, "{}", io::Error::last_os_error());
    let root_pin = RootPin(unsafe { File::from_raw_fd(descriptor as i32) });
    assert_eq!(start_time(pid).unwrap(), original_start);
    let (handles, _) = state.graceful_shutdown();
    assert_eq!(handles.len(), 2);

    thread::scope(move |scope| {
        // Locals drop before scope joins on panic, releasing the wedged worker
        // and terminating only the pinned private PTY root if a check fails.
        let release = release;
        let _root_pin = root_pin;
        let (finished, result) = mpsc::sync_channel(1);
        let started = Instant::now();
        let batch = scope.spawn(move || {
            let _ = finished.send(finish_terminal_shutdown(&handles));
        });
        // The wedged worker is first in HostState's terminal-ID order. Its
        // five-second join budget must not postpone the healthy PTY's request.
        let healthy_by = started + Duration::from_secs(3);
        loop {
            match start_time(pid) {
                Ok(start) if start == original_start => {}
                Ok(_) => break, // The original was reaped before PID reuse.
                Err(error) if error.kind() == io::ErrorKind::NotFound
                    || error.raw_os_error() == Some(libc::ESRCH) => break,
                Err(error) => panic!("could not inspect the private PTY root: {error}"),
            }
            assert!(Instant::now() < healthy_by, "healthy PTY root was not reaped while the first worker was blocked");
            thread::sleep(Duration::from_millis(5));
        }
        assert!(matches!(result.try_recv(), Err(mpsc::TryRecvError::Empty)));
        let outcomes = result.recv_timeout(Duration::from_secs(9)).expect("shutdown batch exceeded its bounded worker budgets");
        assert!(started.elapsed() < Duration::from_secs(12));
        assert_eq!(outcomes.len(), 2);
        assert_eq!(outcomes[0], Err(TerminalSendError::Closed));
        assert_eq!(outcomes[1], Ok(()), "healthy cleanup was skipped after the first failure");
        assert!(healthy.is_completed());
        assert!(!wedged.is_completed(), "an unfinished worker must not acquire fictional PTY completion");
        drop(release);
        batch.join().unwrap();
    });
}

fn quiet_chat(root: &Path) -> crate::chat::Handle {
    use crate::chat::protocol::{Adapter, ProcessSpec, Step, TransportSpec};
    struct Quiet;
    impl Adapter for Quiet {
        fn initialize(&mut self) -> Result<Step, String> { Ok(Step::default()) }
        fn command(&mut self, _: crate::chat::Command) -> Result<Step, String> { Err("fixture submits no provider work".into()) }
        fn receive(&mut self, _: serde_json::Value) -> Result<Step, String> { Ok(Step::default()) }
    }
    let spec = ProcessSpec {
        execution: None,
        messages: None,
        unset_environment: Default::default(),
        transport: TransportSpec::JsonLines,
        program: "/bin/cat".into(),
        arguments: Vec::new(),
        working_directory: root.to_owned(),
        environment: Default::default(),
    };
    let mut initial = Step::default();
    let store = crate::chat::runtime::preflight(&spec, &mut initial).unwrap();
    crate::chat::Handle::start(spec, Box::new(Quiet), initial, store, None, |_| {}).unwrap()
}

fn chat_record(source: crate::chat::Session, handle: crate::chat::Handle) -> super::chat::Record {
    super::chat::Record {
        execution_instance: Uuid::new_v4(),
        console_recipe: None,
        paired_terminal: None,
        session: source,
        handle,
        source_process: None,
        subscribers: Default::default(),
        observations: Default::default(),
        completion_observed: false,
        returning_terminal: false,
    }
}

#[test]
fn provider_shutdown_failure_is_reported_while_healthy_owners_are_reaped() {
    use crate::chat::{Launch, NativeContinuity, OwnedCodexListener, Session, protocol::{ProcessSpec, TransportSpec}};
    use crate::daemon::holder_registry::{self as registry, Kind};
    use std::{io::Write, os::unix::fs::OpenOptionsExt};
    const TEST: &str = "host::shutdown_tests::provider_shutdown_failure_is_reported_while_healthy_owners_are_reaped";
    let Some(root) = private_runtime(TEST) else { return; };
    let proof = HostSessionProof {
        host_session_id: HostSessionId::new("provider-shutdown"),
        boot_id: crate::daemon::read_boot_id().unwrap(),
        host_incarnation: HostIncarnation::new("provider-shutdown"),
    };
    let paths = crate::daemon::RuntimePaths::discover().unwrap();
    let _lease = crate::daemon::HostLease::acquire(&paths, &proof).unwrap();
    let id = TerminalId::from_uuid(Uuid::from_u128(1));
    let socket = root.join(format!("cagents-codex-{id}/control.sock"));
    let source = Session {
        id,
        launch: Launch {
            provider: crate::agent::Provider::Codex,
            recipe: LaunchRecipe {
                program: "codex".into(),
                arguments: vec!["--remote".into(), format!("unix://{}", socket.display())],
                working_directory: root.clone(),
                environment: Default::default(),
                unset_environment: Default::default(),
            },
            session_id: None,
        },
        proof: Some(proof.clone()),
        terminal: None,
        native_continuity: NativeContinuity::ReconnectOnly,
        owned_codex_listener: Some(OwnedCodexListener { socket: socket.clone(), allocator: proof.clone() }),
    };
    // Exercise the real helper/receipt cleanup protocol with an owned sleep
    // process. This is a lifecycle fixture, not native Codex qualification.
    let holder = crate::chat::native_holder::spawn(ProcessSpec {
        execution: None,
        messages: None,
        unset_environment: Default::default(),
        transport: TransportSpec::OwnedUnixWebSocket { socket },
        program: "/bin/sleep".into(),
        arguments: vec!["60".into()],
        working_directory: root.clone(),
        environment: Default::default(),
    }, source.clone()).unwrap();
    let directory = registry::directory(Kind::Native).unwrap();
    let record = registry::evidence(&registry::socket(&directory, id), Kind::Native).unwrap().unwrap();
    assert!(record.root.is_current());
    let receipt = directory.join(format!("{id}-{}-{}.closed", record.holder.pid, record.holder.start));
    let failed = quiet_chat(&root);
    failed.close().unwrap();
    struct Cleanup { handle: crate::chat::Handle, receipt: std::path::PathBuf }
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.receipt);
            let _ = self.handle.close();
        }
    }
    let cleanup = Cleanup { handle: failed.clone(), receipt: receipt.clone() };
    let mut invalid = fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&receipt).unwrap();
    invalid.write_all(b"transient invalid cleanup receipt").unwrap();
    drop(invalid);
    failed.fixture_retained_native_holder(holder, "Native teardown remains unconfirmed".into());
    let healthy_chat = quiet_chat(&root);
    assert!(!healthy_chat.is_completed());
    let mut healthy_source = source.clone();
    healthy_source.id = TerminalId::from_uuid(Uuid::from_u128(2));
    healthy_source.owned_codex_listener = None;
    healthy_source.launch.recipe = session(2, &root).launch;
    let healthy_terminal = TerminalHandle::spawn(session(3, &root), TerminalConfig::default(), |_| {}).unwrap();
    let ready_by = Instant::now() + Duration::from_secs(5);
    let pid: u32 = loop {
        if let Ok(pid) = fs::read_to_string(root.join("root-pid")) {
            if let Ok(pid) = pid.parse() { break pid; }
        }
        assert!(Instant::now() < ready_by);
        thread::sleep(Duration::from_millis(5));
    };
    let original_start = start_time(pid).unwrap();
    assert_eq!(fs::read_link(format!("/proc/{pid}/cwd")).unwrap(), root);
    let descriptor = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
    assert!(descriptor >= 0);
    let _pin = RootPin(unsafe { File::from_raw_fd(descriptor as i32) });
    assert_eq!(start_time(pid).unwrap(), original_start);
    let mut state = HostState::new(proof);
    state.register_client(1);
    state.finish_spawn(1, session(3, &root), healthy_terminal.clone(), true).unwrap();
    state.chats.insert(id, chat_record(source, failed.clone()));
    state.chats.insert(healthy_source.id, chat_record(healthy_source, healthy_chat.clone()));
    let (handles, _) = state.graceful_shutdown();
    let host: SharedHost = Arc::new(super::state::HostLock::new(state));
    let started = Instant::now();
    let error = finish_chat_shutdown(&host).unwrap_err();
    assert!(error.contains(&format!("Provider {id} shutdown was not confirmed")), "{error}");
    assert!(!failed.is_completed());
    assert!(record.root.is_current(), "failed proof must retain the exact actual provider owner");
    assert!(healthy_chat.is_completed(), "later healthy provider was skipped after the first failure");
    assert_eq!(finish_terminal_shutdown(&handles), vec![Ok(())]);
    assert!(healthy_terminal.is_completed());
    match start_time(pid) {
        Ok(current) => assert_ne!(current, original_start, "the exact healthy PTY root remains unreaped"),
        Err(error) if error.kind() == io::ErrorKind::NotFound || error.raw_os_error() == Some(libc::ESRCH) => {}
        Err(error) => panic!("inspect the healthy PTY root after shutdown: {error}"),
    }
    assert!(started.elapsed() < Duration::from_secs(12));
    assert!(record.root.is_current(), "unrelated healthy cleanup must not terminate the retained provider");
    // The same owned capability can make progress after its exact transient
    // receipt fault is removed; no replacement process or recipe is launched.
    fs::remove_file(&receipt).unwrap();
    failed.close().unwrap();
    assert!(failed.is_completed());
    assert!(!record.root.is_current());
    drop(cleanup);
}
