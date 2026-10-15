//! Actual private holder/receipt failure through the provider shutdown caller.
//! This exercises local lifecycle ownership, not a real provider conversation.
use super::*;
use crate::{
    model::{LaunchRecipe, TerminalContinuity, TerminalId, TerminalSession},
    terminal::{TerminalConfig, TerminalHandle},
};
use std::{
    fs::File,
    os::fd::{AsRawFd, FromRawFd},
    path::Path,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

const ROOT: &str = "CAGENTS_PROVIDER_SHUTDOWN_ROOT";

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
            "isolated provider shutdown failed or exceeded 30s; retained {failure_root:?}:\n{stdout}\n{stderr}"
        );
        assert!(stdout.contains("1 passed"));
        return None;
    };
    Some(std::path::PathBuf::from(root).canonicalize().unwrap())
}

fn quiet_chat(root: &Path) -> crate::chat::runtime::Handle {
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
    crate::chat::runtime::Handle::start(spec, Box::new(Quiet), initial, store, None, |_| {}).unwrap()
}

fn chat_record(source: crate::chat::Session, handle: crate::chat::runtime::Handle) -> super::chat::Record {
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
    const TEST: &str = "host::provider_shutdown_tests::provider_shutdown_failure_is_reported_while_healthy_owners_are_reaped";
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
    struct Cleanup { handle: crate::chat::runtime::Handle, receipt: std::path::PathBuf }
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


// A real helper is retained by a LocalHandle whose ordinary actor was already
// reaped. This reaches the contended teardown mutex, not a fabricated reply.
fn retained_chat(
    root: &Path,
    proof: &HostSessionProof,
    number: u128,
) -> (crate::chat::Session, crate::chat::runtime::Handle, crate::daemon::holder_registry::Record) {
    use crate::chat::{Launch, NativeContinuity, OwnedCodexListener, Session,
        protocol::{ProcessSpec, TransportSpec}};
    use crate::daemon::holder_registry::{self as registry, Kind};
    let id = TerminalId::from_uuid(Uuid::from_u128(number));
    let socket = root.join(format!("cagents-codex-{id}/control.sock"));
    let source = Session {
        id,
        launch: Launch { provider: crate::agent::Provider::Codex, recipe: LaunchRecipe {
            program: "codex".into(), arguments: vec!["--remote".into(), format!("unix://{}", socket.display())],
            working_directory: root.to_owned(), environment: Default::default(), unset_environment: Default::default(),
        }, session_id: None },
        proof: Some(proof.clone()), terminal: None,
        native_continuity: NativeContinuity::ReconnectOnly,
        owned_codex_listener: Some(OwnedCodexListener {socket: socket.clone(), allocator: proof.clone()}),
    };
    let holder = crate::chat::native_holder::spawn(ProcessSpec {
        execution: None, messages: None, unset_environment: Default::default(),
        transport: TransportSpec::OwnedUnixWebSocket {socket}, program: "/bin/sleep".into(),
        arguments: vec!["60".into()], working_directory: root.to_owned(), environment: Default::default(),
    }, source.clone()).unwrap();
    let directory = registry::directory(Kind::Native).unwrap();
    let record = registry::evidence(&registry::socket(&directory, id), Kind::Native).unwrap().unwrap();
    assert!(record.root.is_current());
    let handle = quiet_chat(root);
    handle.close().unwrap();
    handle.fixture_retained_native_holder(holder, "Native teardown remains unconfirmed".into());
    (source, handle, record)
}

struct RetainedOwners(Vec<crate::chat::runtime::Handle>);
impl Drop for RetainedOwners {
    fn drop(&mut self) {
        for handle in &self.0 { let _ = handle.close(); }
    }
}

#[test]
fn two_contended_provider_owners_do_not_starve_a_healthy_chat_or_terminal() {
    const TEST: &str = "host::provider_shutdown_tests::two_contended_provider_owners_do_not_starve_a_healthy_chat_or_terminal";
    let Some(root) = private_runtime(TEST) else { return; };
    let proof = HostSessionProof {
        host_session_id: HostSessionId::new("fair-provider-shutdown"),
        boot_id: crate::daemon::read_boot_id().unwrap(),
        host_incarnation: HostIncarnation::new("fair-provider-shutdown"),
    };
    let paths = crate::daemon::RuntimePaths::discover().unwrap();
    let _lease = crate::daemon::HostLease::acquire(&paths, &proof).unwrap();
    let mut cleanup = RetainedOwners(Vec::new());
    let (first_source, first, first_record) = retained_chat(&root, &proof, 1);
    cleanup.0.push(first.clone());
    let (second_source, second, second_record) = retained_chat(&root, &proof, 2);
    cleanup.0.push(second.clone());
    let healthy = quiet_chat(&root);
    cleanup.0.push(healthy.clone());
    let healthy_process = healthy.provider_process().expect("actual healthy fixture child");
    assert_eq!(fs::read_link(format!("/proc/{}/cwd", healthy_process.pid)).unwrap(), root);
    let healthy_descriptor = unsafe {libc::syscall(libc::SYS_pidfd_open, healthy_process.pid, 0)};
    assert!(healthy_descriptor >= 0);
    let _healthy_pin = RootPin(unsafe {File::from_raw_fd(healthy_descriptor as i32)});
    assert!(healthy_process.is_current());
    let mut healthy_source = first_source.clone();
    healthy_source.id = TerminalId::from_uuid(Uuid::from_u128(3));
    healthy_source.owned_codex_listener = None;
    healthy_source.launch.recipe.program = "/bin/cat".into();
    healthy_source.launch.recipe.arguments.clear();
    let terminal = TerminalHandle::spawn(session(4, &root), TerminalConfig::default(), |_| {}).unwrap();
    let ready_by = Instant::now() + Duration::from_secs(5);
    let terminal_pid: u32 = loop {
        if let Ok(value) = fs::read_to_string(root.join("root-pid")) {
            if let Ok(pid) = value.parse() { break pid; }
        }
        assert!(Instant::now() < ready_by);
        thread::sleep(Duration::from_millis(5));
    };
    let terminal_start = start_time(terminal_pid).unwrap();
    assert_eq!(fs::read_link(format!("/proc/{terminal_pid}/cwd")).unwrap(), root);
    let descriptor = unsafe {libc::syscall(libc::SYS_pidfd_open, terminal_pid, 0)};
    assert!(descriptor >= 0);
    let _terminal_pin = RootPin(unsafe {File::from_raw_fd(descriptor as i32)});
    assert_eq!(start_time(terminal_pid).unwrap(), terminal_start);
    let mut state = HostState::new(proof);
    state.register_client(1);
    state.finish_spawn(1, session(4, &root), terminal.clone(), true).unwrap();
    state.chats.insert(first_source.id, chat_record(first_source, first.clone()));
    state.chats.insert(second_source.id, chat_record(second_source, second.clone()));
    state.chats.insert(healthy_source.id, chat_record(healthy_source, healthy.clone()));
    let (terminals, _) = state.graceful_shutdown();
    let host: SharedHost = Arc::new(super::state::HostLock::new(state));
    thread::scope(|scope| {
        // These sender locals drop before scope joins on panic, releasing the
        // two exact slot guards before any outer cleanup owner calls close.
        let (release_first, wait_first) = crossbeam_channel::bounded::<()>(1);
        let (release_second, wait_second) = crossbeam_channel::bounded::<()>(1);
        let (ready, held) = crossbeam_channel::bounded(2);
        let ready_first = ready.clone();
        let first_copy = first.clone();
        let expected_first = first_record.clone();
        scope.spawn(move || first_copy.with_retained_native_locked(&expected_first, || {
            ready_first.send(()).unwrap(); let _ = wait_first.recv();
        }));
        let second_copy = second.clone();
        let expected_second = second_record.clone();
        scope.spawn(move || second_copy.with_retained_native_locked(&expected_second, || {
            ready.send(()).unwrap(); let _ = wait_second.recv();
        }));
        held.recv_timeout(Duration::from_secs(2)).unwrap();
        held.recv_timeout(Duration::from_secs(2)).unwrap();
        let started = Instant::now();
        let error = finish_chat_shutdown_with_budget(&host, Duration::from_secs(2)).unwrap_err();
        assert!(started.elapsed() < Duration::from_secs(3), "{error}");
        for record in [&first_record, &second_record] {
            assert!(record.root.is_current(), "uncertainty must preserve this exact retained child");
            assert!(error.contains(&record.terminal.to_string()), "{error}");
        }
        assert!(!first.is_completed());
        assert!(!second.is_completed());
        assert!(healthy.is_completed(), "two contended owners prevented the third provider's close");
        assert!(!healthy_process.is_current(), "the exact healthy provider child was not reaped");
        assert_eq!(finish_terminal_shutdown(&terminals), vec![Ok(())]);
        assert!(terminal.is_completed());
        match start_time(terminal_pid) {
            Ok(current) => assert_ne!(current, terminal_start, "the healthy terminal root was not reaped"),
            Err(error) if error.kind() == io::ErrorKind::NotFound || error.raw_os_error() == Some(libc::ESRCH) => {}
            Err(error) => panic!("inspect the healthy terminal root: {error}"),
        }
        assert!(first_record.root.is_current() && second_record.root.is_current());
        drop(release_first);
        drop(release_second);
    });
    // Retry only the original owned capabilities after the actual contention
    // ends. No recipe, source proof, child identity or process is replaced.
    first.close().unwrap();
    second.close().unwrap();
    assert!(first.is_completed() && second.is_completed());
    assert!(!first_record.root.is_current() && !second_record.root.is_current());
}

#[test]
fn semantic_batch_keeps_more_sources_than_its_ticket_capacity() {
    const TEST: &str = "host::provider_shutdown_tests::semantic_batch_keeps_more_sources_than_its_ticket_capacity";
    let Some(root) = private_runtime(TEST) else { return; };
    // Construct 100 distinct closed presentation records sharing an already
    // reaped test actor. This is backlog coverage, not 100-provider native/perf evidence.
    let handle = quiet_chat(&root);
    handle.close().unwrap();
    let proof = HostSessionProof {
        host_session_id: HostSessionId::new("semantic-backlog"),
        boot_id: crate::daemon::read_boot_id().unwrap(),
        host_incarnation: HostIncarnation::new("semantic-backlog"),
    };
    let mut state = HostState::new(proof.clone());
    for index in 1..=100 {
        let id = TerminalId::from_uuid(Uuid::from_u128(index));
        let source = crate::chat::Session { id,
            launch: crate::chat::Launch { provider: crate::agent::Provider::Codex,
                recipe: session(index, &root).launch, session_id: None },
            proof: Some(proof.clone()), terminal: None,
            native_continuity: crate::chat::NativeContinuity::ReconnectOnly,
            owned_codex_listener: None,
        };
        state.chats.insert(id, chat_record(source, handle.clone()));
    }
    state.graceful_shutdown();
    let host: SharedHost = Arc::new(super::state::HostLock::new(state));
    finish_chat_shutdown_with_budget(&host, Duration::from_secs(5)).unwrap();
    assert_eq!(host.lock().unwrap().chats.len(), 100);
    assert!(host.lock().unwrap().chats.values().all(|record| record.handle.is_completed()));
}
