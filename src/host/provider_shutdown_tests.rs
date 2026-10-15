//! Actual private holder/receipt failure through the provider shutdown caller.
//! This exercises local lifecycle ownership, not a real provider conversation.
use super::*;
use crate::{
    model::{LaunchRecipe, TerminalContinuity, TerminalId, TerminalSession},
    terminal::{TerminalConfig, TerminalHandle},
};
use std::os::unix::fs::OpenOptionsExt;
use std::{
    fs::File,
    os::fd::{AsRawFd, FromRawFd},
    path::Path,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

const ROOT: &str = "CAGENTS_PROVIDER_SHUTDOWN_ROOT";

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
        reservation.quarantine();
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
            directory.keep();
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
        use io::{Read, Write};
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
    Ok(stat
        .rsplit_once(')')
        .unwrap()
        .1
        .split_whitespace()
        .nth(19)
        .unwrap()
        .into())
}

// A failed procfs inspection cannot count as original-process reaping.
fn original_is_current(expected: &crate::daemon::holder_registry::Process) -> bool {
    match crate::daemon::holder_registry::Process::read(expected.pid) {
        Ok(actual) => actual == *expected,
        Err(error)
            if error.kind() == io::ErrorKind::NotFound
                || error.raw_os_error() == Some(libc::ESRCH) =>
        {
            false
        }
        Err(error) => panic!("inspect the pinned original fixture process: {error}"),
    }
}

fn private_runtime(test: &str) -> Option<std::path::PathBuf> {
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
            "private provider shutdown failed or exceeded30s; retained {}:\n{}\n{}",
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

fn quiet_chat(root: &Path) -> crate::chat::runtime::Handle {
    use crate::chat::protocol::{Adapter, ProcessSpec, Step, TransportSpec};
    struct Quiet;
    impl Adapter for Quiet {
        fn initialize(&mut self) -> Result<Step, String> {
            Ok(Step::default())
        }
        fn command(&mut self, _: crate::chat::Command) -> Result<Step, String> {
            Err("fixture submits no provider work".into())
        }
        fn receive(&mut self, _: serde_json::Value) -> Result<Step, String> {
            Ok(Step::default())
        }
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
    crate::chat::runtime::Handle::start(spec, Box::new(Quiet), initial, store, None, |_| {})
        .unwrap()
}

fn chat_record(
    source: crate::chat::Session,
    handle: crate::chat::runtime::Handle,
) -> super::chat::Record {
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
    use crate::chat::{
        Launch, NativeContinuity, OwnedCodexListener, Session,
        protocol::{ProcessSpec, TransportSpec},
    };
    use crate::daemon::holder_registry::{self as registry, Kind};
    use std::{io::Write, os::unix::fs::OpenOptionsExt};
    const TEST: &str = "host::provider_shutdown_tests::provider_shutdown_failure_is_reported_while_healthy_owners_are_reaped";
    let Some(root) = private_runtime(TEST) else {
        return;
    };
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
        owned_codex_listener: Some(OwnedCodexListener {
            socket: socket.clone(),
            allocator: proof.clone(),
        }),
    };
    // Exercise the real helper/receipt cleanup protocol with an owned sleep
    // process. This is a lifecycle fixture, not native Codex qualification.
    let holder = crate::chat::native_holder::spawn(
        ProcessSpec {
            execution: None,
            messages: None,
            unset_environment: Default::default(),
            transport: TransportSpec::OwnedUnixWebSocket { socket },
            program: "/bin/sleep".into(),
            arguments: vec!["60".into()],
            working_directory: root.clone(),
            environment: Default::default(),
        },
        source.clone(),
    )
    .unwrap();
    let directory = registry::directory(Kind::Native).unwrap();
    let record = registry::evidence(&registry::socket(&directory, id), Kind::Native)
        .unwrap()
        .unwrap();
    assert!(original_is_current(&record.root));
    let receipt = directory.join(format!(
        "{id}-{}-{}.closed",
        record.holder.pid, record.holder.start
    ));
    let failed = quiet_chat(&root);
    failed.close().unwrap();
    struct Cleanup {
        handle: crate::chat::runtime::Handle,
        receipt: std::path::PathBuf,
    }
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.receipt);
            let _ = self.handle.close();
        }
    }
    let cleanup = Cleanup {
        handle: failed.clone(),
        receipt: receipt.clone(),
    };
    let mut invalid = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&receipt)
        .unwrap();
    invalid
        .write_all(b"transient invalid cleanup receipt")
        .unwrap();
    drop(invalid);
    failed.fixture_retained_native_holder(holder, "Native teardown remains unconfirmed".into());
    let healthy_chat = quiet_chat(&root);
    assert!(!healthy_chat.is_completed());
    let mut healthy_source = source.clone();
    healthy_source.id = TerminalId::from_uuid(Uuid::from_u128(2));
    healthy_source.owned_codex_listener = None;
    healthy_source.launch.recipe = session(2, &root).launch;
    let healthy_terminal =
        TerminalHandle::spawn(session(3, &root), TerminalConfig::default(), |_| {}).unwrap();
    let ready_by = Instant::now() + Duration::from_secs(5);
    let pid: u32 = loop {
        if let Ok(pid) = fs::read_to_string(root.join("root-pid")) {
            if let Ok(pid) = pid.parse() {
                break pid;
            }
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
    state
        .finish_spawn(1, session(3, &root), healthy_terminal.clone(), true)
        .unwrap();
    state.chats.insert(id, chat_record(source, failed.clone()));
    state.chats.insert(
        healthy_source.id,
        chat_record(healthy_source, healthy_chat.clone()),
    );
    let (handles, _) = state.graceful_shutdown();
    let host: SharedHost = Arc::new(super::state::HostLock::new(state));
    let started = Instant::now();
    let error = finish_chat_shutdown(&host).unwrap_err();
    assert!(
        error.contains(&format!("Provider {id} shutdown was not confirmed")),
        "{error}"
    );
    assert!(!failed.is_completed());
    assert!(
        original_is_current(&record.root),
        "failed proof must retain the exact actual provider owner"
    );
    assert!(
        healthy_chat.is_completed(),
        "later healthy provider was skipped after the first failure"
    );
    assert_eq!(finish_terminal_shutdown(&handles), vec![Ok(())]);
    assert!(healthy_terminal.is_completed());
    match start_time(pid) {
        Ok(current) => assert_ne!(
            current, original_start,
            "the exact healthy PTY root remains unreaped"
        ),
        Err(error)
            if error.kind() == io::ErrorKind::NotFound
                || error.raw_os_error() == Some(libc::ESRCH) => {}
        Err(error) => panic!("inspect the healthy PTY root after shutdown: {error}"),
    }
    assert!(started.elapsed() < Duration::from_secs(12));
    assert!(
        original_is_current(&record.root),
        "unrelated healthy cleanup must not terminate the retained provider"
    );
    // The same owned capability can make progress after its exact transient
    // receipt fault is removed; no replacement process or recipe is launched.
    fs::remove_file(&receipt).unwrap();
    failed.close().unwrap();
    assert!(failed.is_completed());
    assert!(!original_is_current(&record.root));
    drop(cleanup);
}

// A real helper is retained by a LocalHandle whose ordinary actor was already
// reaped. This reaches the contended teardown mutex, not a fabricated reply.
fn retained_chat(
    root: &Path,
    proof: &HostSessionProof,
    number: u128,
) -> (
    crate::chat::Session,
    crate::chat::runtime::Handle,
    crate::daemon::holder_registry::Record,
) {
    use crate::chat::{
        Launch, NativeContinuity, OwnedCodexListener, Session,
        protocol::{ProcessSpec, TransportSpec},
    };
    use crate::daemon::holder_registry::{self as registry, Kind};
    let id = TerminalId::from_uuid(Uuid::from_u128(number));
    let socket = root.join(format!("cagents-codex-{id}/control.sock"));
    let source = Session {
        id,
        launch: Launch {
            provider: crate::agent::Provider::Codex,
            recipe: LaunchRecipe {
                program: "codex".into(),
                arguments: vec!["--remote".into(), format!("unix://{}", socket.display())],
                working_directory: root.to_owned(),
                environment: Default::default(),
                unset_environment: Default::default(),
            },
            session_id: None,
        },
        proof: Some(proof.clone()),
        terminal: None,
        native_continuity: NativeContinuity::ReconnectOnly,
        owned_codex_listener: Some(OwnedCodexListener {
            socket: socket.clone(),
            allocator: proof.clone(),
        }),
    };
    let holder = crate::chat::native_holder::spawn(
        ProcessSpec {
            execution: None,
            messages: None,
            unset_environment: Default::default(),
            transport: TransportSpec::OwnedUnixWebSocket { socket },
            program: "/bin/sleep".into(),
            arguments: vec!["60".into()],
            working_directory: root.to_owned(),
            environment: Default::default(),
        },
        source.clone(),
    )
    .unwrap();
    let directory = registry::directory(Kind::Native).unwrap();
    let record = registry::evidence(&registry::socket(&directory, id), Kind::Native)
        .unwrap()
        .unwrap();
    assert!(original_is_current(&record.root));
    let handle = quiet_chat(root);
    handle.close().unwrap();
    handle.fixture_retained_native_holder(holder, "Native teardown remains unconfirmed".into());
    (source, handle, record)
}

struct RetainedOwners(Vec<crate::chat::runtime::Handle>);
impl Drop for RetainedOwners {
    fn drop(&mut self) {
        for handle in &self.0 {
            let _ = handle.close();
        }
    }
}

#[test]
fn two_contended_provider_owners_do_not_starve_a_healthy_chat_or_terminal() {
    const TEST: &str = "host::provider_shutdown_tests::two_contended_provider_owners_do_not_starve_a_healthy_chat_or_terminal";
    let Some(root) = private_runtime(TEST) else {
        return;
    };
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
    let healthy_process = healthy
        .provider_process()
        .expect("actual healthy fixture child");
    assert_eq!(
        fs::read_link(format!("/proc/{}/cwd", healthy_process.pid)).unwrap(),
        root
    );
    let healthy_descriptor = unsafe { libc::syscall(libc::SYS_pidfd_open, healthy_process.pid, 0) };
    assert!(healthy_descriptor >= 0);
    let _healthy_pin = RootPin(unsafe { File::from_raw_fd(healthy_descriptor as i32) });
    assert!(original_is_current(&healthy_process));
    let mut healthy_source = first_source.clone();
    healthy_source.id = TerminalId::from_uuid(Uuid::from_u128(3));
    healthy_source.owned_codex_listener = None;
    healthy_source.launch.recipe.program = "/bin/cat".into();
    healthy_source.launch.recipe.arguments.clear();
    let terminal =
        TerminalHandle::spawn(session(4, &root), TerminalConfig::default(), |_| {}).unwrap();
    let ready_by = Instant::now() + Duration::from_secs(5);
    let terminal_pid: u32 = loop {
        if let Ok(value) = fs::read_to_string(root.join("root-pid")) {
            if let Ok(pid) = value.parse() {
                break pid;
            }
        }
        assert!(Instant::now() < ready_by);
        thread::sleep(Duration::from_millis(5));
    };
    let terminal_start = start_time(terminal_pid).unwrap();
    assert_eq!(
        fs::read_link(format!("/proc/{terminal_pid}/cwd")).unwrap(),
        root
    );
    let descriptor = unsafe { libc::syscall(libc::SYS_pidfd_open, terminal_pid, 0) };
    assert!(descriptor >= 0);
    let _terminal_pin = RootPin(unsafe { File::from_raw_fd(descriptor as i32) });
    assert_eq!(start_time(terminal_pid).unwrap(), terminal_start);
    let mut state = HostState::new(proof);
    state.register_client(1);
    state
        .finish_spawn(1, session(4, &root), terminal.clone(), true)
        .unwrap();
    state
        .chats
        .insert(first_source.id, chat_record(first_source, first.clone()));
    state
        .chats
        .insert(second_source.id, chat_record(second_source, second.clone()));
    state.chats.insert(
        healthy_source.id,
        chat_record(healthy_source, healthy.clone()),
    );
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
        scope.spawn(move || {
            first_copy.with_retained_native_locked(&expected_first, || {
                ready_first.send(()).unwrap();
                let _ = wait_first.recv();
            })
        });
        let second_copy = second.clone();
        let expected_second = second_record.clone();
        scope.spawn(move || {
            second_copy.with_retained_native_locked(&expected_second, || {
                ready.send(()).unwrap();
                let _ = wait_second.recv();
            })
        });
        held.recv_timeout(Duration::from_secs(2)).unwrap();
        held.recv_timeout(Duration::from_secs(2)).unwrap();
        let started = Instant::now();
        let error = finish_chat_shutdown_with_budget(&host, Duration::from_secs(2)).unwrap_err();
        assert!(started.elapsed() < Duration::from_secs(3), "{error}");
        for record in [&first_record, &second_record] {
            assert!(
                original_is_current(&record.root),
                "uncertainty must preserve this exact retained child"
            );
            assert!(error.contains(&record.terminal.to_string()), "{error}");
        }
        assert!(!first.is_completed());
        assert!(!second.is_completed());
        assert!(
            healthy.is_completed(),
            "two contended owners prevented the third provider's close"
        );
        assert!(
            !original_is_current(&healthy_process),
            "the exact healthy provider child was not reaped"
        );
        assert_eq!(finish_terminal_shutdown(&terminals), vec![Ok(())]);
        assert!(terminal.is_completed());
        match start_time(terminal_pid) {
            Ok(current) => assert_ne!(
                current, terminal_start,
                "the healthy terminal root was not reaped"
            ),
            Err(error)
                if error.kind() == io::ErrorKind::NotFound
                    || error.raw_os_error() == Some(libc::ESRCH) => {}
            Err(error) => panic!("inspect the healthy terminal root: {error}"),
        }
        assert!(
            original_is_current(&first_record.root) && original_is_current(&second_record.root)
        );
        drop(release_first);
        drop(release_second);
    });
    // Retry only the original owned capabilities after the actual contention
    // ends. No recipe, source proof, child identity or process is replaced.
    first.close().unwrap();
    second.close().unwrap();
    assert!(first.is_completed() && second.is_completed());
    assert!(!original_is_current(&first_record.root) && !original_is_current(&second_record.root));
}

#[test]
fn semantic_batch_keeps_more_sources_than_its_ticket_capacity() {
    const TEST: &str =
        "host::provider_shutdown_tests::semantic_batch_keeps_more_sources_than_its_ticket_capacity";
    let Some(root) = private_runtime(TEST) else {
        return;
    };
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
        let source = crate::chat::Session {
            id,
            launch: crate::chat::Launch {
                provider: crate::agent::Provider::Codex,
                recipe: session(index, &root).launch,
                session_id: None,
            },
            proof: Some(proof.clone()),
            terminal: None,
            native_continuity: crate::chat::NativeContinuity::ReconnectOnly,
            owned_codex_listener: None,
        };
        state.chats.insert(id, chat_record(source, handle.clone()));
    }
    state.graceful_shutdown();
    let host: SharedHost = Arc::new(super::state::HostLock::new(state));
    finish_chat_shutdown_with_budget(&host, Duration::from_secs(5)).unwrap();
    assert_eq!(host.lock().unwrap().chats.len(), 100);
    assert!(
        host.lock()
            .unwrap()
            .chats
            .values()
            .all(|record| record.handle.is_completed())
    );
}

#[test]
fn fixture_child_drop_reaps_only_the_original_and_preserves_its_neighbor() {
    const TEST: &str = "host::provider_shutdown_tests::fixture_child_drop_reaps_only_the_original_and_preserves_its_neighbor";
    let Some(root) = private_runtime(TEST) else {
        return;
    };
    assert!(!crate::daemon::child_reaper::has_outstanding_ownership());
    let spawn = || {
        TestChild::spawn(
            Command::new("/bin/sleep")
                .arg("60")
                .env_clear()
                .current_dir(&root)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null()),
        )
        .unwrap()
    };
    let original = spawn();
    let neighbor = spawn();
    let first =
        crate::daemon::holder_registry::Process::read(original.child.as_ref().unwrap().id() as i32)
            .unwrap();
    let other =
        crate::daemon::holder_registry::Process::read(neighbor.child.as_ref().unwrap().id() as i32)
            .unwrap();
    assert!(original_is_current(&first) && original_is_current(&other));
    drop(original);
    let deadline = Instant::now() + Duration::from_secs(5);
    while original_is_current(&first) {
        assert!(
            Instant::now() < deadline,
            "the exact original Child was not reaped"
        );
        assert!(
            original_is_current(&other),
            "fixture cleanup touched its neighboring owner"
        );
        thread::sleep(Duration::from_millis(5));
    }
    assert!(original_is_current(&other));
    drop(neighbor);
    let deadline = Instant::now() + Duration::from_secs(5);
    while original_is_current(&other) || crate::daemon::child_reaper::has_outstanding_ownership() {
        assert!(
            Instant::now() < deadline,
            "fixture cleanup still owns an original child or reservation"
        );
        thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn fixture_output_overflow_is_bounded_and_cannot_count_as_a_passing_oracle() {
    use io::Write;
    use std::os::unix::net::UnixStream;
    const TEST: &str = "host::provider_shutdown_tests::fixture_output_overflow_is_bounded_and_cannot_count_as_a_passing_oracle";
    let Some(root) = private_runtime(TEST) else {
        return;
    };
    let (reader, mut writer) = UnixStream::pair().unwrap();
    writer.set_nonblocking(true).unwrap();
    let path = root.join("overflow.log");
    let mut capture = Captured::new(reader, &path).unwrap();
    let bytes = [b'x'; 8192];
    let mut sent = 0;
    let deadline = Instant::now() + Duration::from_secs(5);
    while sent < LOG_LIMIT + bytes.len() {
        assert!(
            Instant::now() < deadline,
            "bounded fixture capture made no progress"
        );
        match writer.write(&bytes) {
            Ok(count) => sent += count,
            Err(error)
                if error.kind() == io::ErrorKind::WouldBlock
                    || error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => panic!("write fixture overflow: {error}"),
        }
        capture.drain().unwrap();
    }
    drop(writer);
    capture.drain().unwrap();
    assert!(
        capture.truncated,
        "excess output must refuse fixture success"
    );
    assert_eq!(capture.bytes.len(), LOG_LIMIT);
    assert_eq!(fs::metadata(path).unwrap().len(), LOG_LIMIT as u64);
}
