//! Real cleanup refusal must reach the host result without skipping other owners.
use super::*;
use crate::{
    chat::{
        self,
        protocol::{Adapter, ProcessSpec, Step, TransportSpec},
        runtime::Handle,
    },
    daemon::holder_registry::{self as registry, Kind, Process},
    model::{LaunchRecipe, TerminalContinuity, TerminalId, TerminalSession},
    terminal::{TerminalConfig, TerminalHandle},
};
use std::{
    fs::File,
    io::Write,
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::fs::OpenOptionsExt,
    },
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

const TEST: &str =
    "host::provider_shutdown_tests::cleanup_refusal_is_reported_and_healthy_owners_still_stop";
const ROOT: &str = "CAGENTS_PROVIDER_SHUTDOWN_TEST_ROOT";
const INHERITED_SCOPE: &str = "CAGENTS_PROVIDER_SHUTDOWN_AUTHORITY_PRESENCE";
const RETAINED_HELPER: &str = "CAGENTS_TEST_RETAINED_ACTOR_HELPER";

/// Keep the ORIGINAL runtime on success and failure until the external strict
/// command guardian settles the full family. Child exit alone is not that proof.
struct EvidenceDirectory(Option<tempfile::TempDir>);
impl Drop for EvidenceDirectory {
    fn drop(&mut self) {
        if let Some(directory) = self.0.take() {
            let _retained = directory.keep();
        }
    }
}

fn private_runtime(test: &str, retained_actor: bool) -> Option<PathBuf> {
    if let Some(root) = std::env::var_os(ROOT) {
        let expected = match std::env::var(INHERITED_SCOPE).as_deref() {
            Ok("present") => true,
            Ok("absent") => false,
            _ => panic!("missing or invalid shutdown fixture authority presence marker"),
        };
        assert_eq!(
            crate::agent_execution::Authority::from_environment()
                != crate::agent_execution::Authority::default(),
            expected,
            "private shutdown fixture lost its inherited authority scope"
        );
        return Some(PathBuf::from(root).canonicalize().unwrap());
    }
    use crate::process::{CancellationToken, CommandSpec, ProcessExecutor, SystemProcessExecutor};
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let _evidence = EvidenceDirectory(Some(directory));
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
    for name in ["home", "run", "data", "config", "cache", "tmp"] {
        fs::create_dir(root.join(name)).unwrap();
        fs::set_permissions(root.join(name), fs::Permissions::from_mode(0o700)).unwrap();
    }
    let executable = std::env::current_exe().unwrap();
    let mut command = CommandSpec::new(&executable);
    // Keep delegation and any same-head helper path inherited unchanged.
    // Other child state is private; no process-wide environment mutation races
    // another libtest thread and no capability value is inspected or copied.
    for (name, _) in std::env::vars_os() {
        if name != std::ffi::OsStr::new(crate::agent_execution::AUTHORITY_ENV)
            && name != std::ffi::OsStr::new(RETAINED_HELPER)
        {
            command = command.env_remove(name);
        }
    }
    command = command
        .args(["--exact", test, "--nocapture", "--test-threads=1"])
        .cwd(&root)
        .env(ROOT, &root)
        .env(
            INHERITED_SCOPE,
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
        .timeout(Duration::from_secs(40))
        .output_limits(256 * 1024, 256 * 1024);
    if retained_actor {
        command = command
            .env("CAGENTS_RETAINED_ACTOR_FIXTURE", "1")
            .env("CLAUDE_CONFIG_DIR", root.join("home/.claude"))
            .env(
                "PATH",
                format!("{}:/usr/bin:/bin", root.join("bin").display()),
            )
            .timeout(Duration::from_secs(90));
    }
    let result = SystemProcessExecutor.execute(&command, &CancellationToken::new());
    let (stdout, stderr) = match &result {
        Ok(output) => (&output.stdout, &output.stderr),
        Err(error) => (&error.stdout, &error.stderr),
    };
    for (name, bytes) in [("stdout", stdout), ("stderr", stderr)] {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(root.join(name))
            .unwrap();
        file.write_all(bytes).unwrap();
    }
    let output = result.unwrap_or_else(|error| {
        panic!(
            "private shutdown fixture retained at {}: {error}",
            root.display()
        )
    });
    let output = output.require_success(&executable).unwrap_or_else(|error| {
        panic!(
            "private shutdown fixture retained at {}: {error}",
            root.display()
        )
    });
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        stdout.contains(&format!("test {test} ... ok")),
        "wrong test output: {stdout}"
    );
    assert!(
        stdout.contains("test result: ok. 1 passed; 0 failed; 0 ignored;"),
        "{stdout}"
    );
    None
}

/// Observational incarnation plus retained pidfd. No signal uses a numeric PID.
struct ProcessPin {
    process: Process,
    descriptor: File,
}
impl ProcessPin {
    fn capture(process: Process) -> Self {
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, process.pid, 0) };
        assert!(fd >= 0, "{}", io::Error::last_os_error());
        let descriptor = unsafe { File::from_raw_fd(fd as i32) };
        assert_eq!(Process::read(process.pid).unwrap(), process);
        Self {
            process,
            descriptor,
        }
    }
    fn live(&self) -> bool {
        let mut poll = libc::pollfd {
            fd: self.descriptor.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let ready = unsafe { libc::poll(&mut poll, 1, 0) };
        assert!(ready >= 0, "{}", io::Error::last_os_error());
        ready == 0 && self.process.is_current()
    }
    fn assert_reaped(&self) {
        assert!(!self.live(), "original fixture process remains live");
        match Process::read(self.process.pid) {
            Ok(current) => assert_ne!(current, self.process, "original remains unreaped"),
            Err(error)
                if error.kind() == io::ErrorKind::NotFound
                    || error.raw_os_error() == Some(libc::ESRCH) => {}
            Err(error) => panic!("inspect original fixture process: {error}"),
        }
    }
}
impl Drop for ProcessPin {
    fn drop(&mut self) {
        unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                self.descriptor.as_raw_fd(),
                libc::SIGKILL,
                std::ptr::null::<libc::siginfo_t>(),
                0,
            );
        }
    }
}

fn quiet_chat(root: &Path) -> Handle {
    struct Quiet;
    impl Adapter for Quiet {
        fn initialize(&mut self) -> Result<Step, String> {
            Ok(Step::default())
        }
        fn command(&mut self, _: chat::Command) -> Result<Step, String> {
            Err("no provider work in this fixture".into())
        }
        fn receive(&mut self, _: serde_json::Value) -> Result<Step, String> {
            Ok(Step::default())
        }
    }
    let spec = ProcessSpec {
        stdout_noise: None,
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
    let store = chat::runtime::preflight(&spec, &mut initial).unwrap();
    Handle::start(spec, Box::new(Quiet), initial, store, None, |_| {}).unwrap()
}

fn record(session: chat::Session, handle: Handle) -> super::chat::Record {
    super::chat::Record {
        execution_instance: Uuid::new_v4(),
        console_recipe: None,
        paired_terminal: None,
        session,
        handle,
        source_process: None,
        subscribers: Default::default(),
        observations: Default::default(),
        completion_observed: false,
        returning_terminal: false,
    }
}

fn terminal(root: &Path) -> TerminalSession {
    let recipe = LaunchRecipe {
        program: "/bin/sh".into(),
        arguments: vec![
            "-c".into(),
            "printf %s \"$$\" > root-pid; exec /bin/sleep 60".into(),
        ],
        working_directory: root.to_owned(),
        environment: Default::default(),
        unset_environment: Default::default(),
    };
    TerminalSession {
        id: TerminalId::from_uuid(Uuid::from_u128(3)),
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

#[test]
fn listener_failure_precedes_real_holder_adoption_and_preserves_the_checkpoint() {
    use crate::upgrade::handoff::{Carried, Checkpoint, Phase, Store};
    use std::{
        cell::Cell,
        os::unix::fs::{FileTypeExt, MetadataExt},
    };

    const EXACT: &str = "host::provider_shutdown_tests::listener_failure_precedes_real_holder_adoption_and_preserves_the_checkpoint";
    let Some(root) = private_runtime(EXACT, false) else {
        return;
    };
    let proof = |incarnation: &str| HostSessionProof {
        host_session_id: HostSessionId::new("listener-startup"),
        boot_id: daemon::read_boot_id().unwrap(),
        host_incarnation: HostIncarnation::new(incarnation),
    };
    let old_proof = proof("before-listener");
    let paths = RuntimePaths::discover().unwrap();
    let old_lease = HostLease::acquire(&paths, &old_proof).unwrap();
    let mut source = terminal(&root);
    source.id = TerminalId::new();
    source.launch.arguments = vec![
        "-c".into(),
        "printf 'launched\\n' >> launches; printf 'listener-ready\\n'; \
         IFS= read -r line; printf 'continued:%s\\n' \"$line\"; IFS= read -r line"
            .into(),
    ];
    source.continuity = TerminalContinuity::DetachedSameBoot {
        host_session_id: old_proof.host_session_id.clone(),
        boot_id: old_proof.boot_id.clone(),
        host_incarnation: old_proof.host_incarnation.clone(),
    };
    let first = TerminalHandle::spawn(
        source.clone(),
        TerminalConfig {
            holder_recovery: Some(crate::terminal::holder::Recovery {
                source: source.clone(),
                previous_sources: Vec::new(),
                relaunches: Vec::new(),
                options: Default::default(),
                execution_environment: None,
                execution_recipe: None,
            }),
            ..Default::default()
        },
        |_| {},
    )
    .unwrap();
    let visible_text = |handle: &TerminalHandle, expected: &str| {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let snapshot = handle.snapshot();
            handle.acknowledge_redraw(snapshot.generation);
            let text: String = snapshot
                .lines
                .iter()
                .flat_map(|line| line.cells.iter().map(|cell| cell.text.as_str()))
                .collect();
            if text.contains(expected) {
                break;
            }
            assert!(Instant::now() < deadline, "missing {expected:?}: {text:?}");
            thread::sleep(Duration::from_millis(10));
        }
    };
    visible_text(&first, "listener-ready");
    let original = first.holder_identity().unwrap().clone();
    let original_root = ProcessPin::capture(original.root.clone());
    let original_holder = ProcessPin::capture(original.holder.clone());
    let holders = crate::terminal::holder::directory().unwrap();
    let holder_socket = crate::terminal::holder::socket(&holders, source.id);
    first.carry(holder_socket.clone()).unwrap();
    assert!(first.is_completed());
    drop(first);

    let store = Store::discover().unwrap();
    let mut checkpoint = Checkpoint::new(
        old_proof,
        Vec::new(),
        vec![Carried {
            source: source.clone(),
            previous_sources: Vec::new(),
            relaunches: Vec::new(),
            options: Default::default(),
        }],
        Vec::new(),
    );
    checkpoint.phase = Phase::Ready;
    checkpoint.preserve_carried = true;
    store.save(&checkpoint).unwrap();
    let checkpoint_bytes = fs::read(&store.path).unwrap();
    let checkpoint_stat = fs::metadata(&store.path).unwrap();
    drop(old_lease);

    let lease = HostLease::acquire(&paths, &proof("after-listener")).unwrap();
    let host = Arc::new(state::HostLock::new(HostState::new(proof(
        "after-listener",
    ))));
    // Production opens the durable execution registry before recovering an
    // original holder; a bare HostState deliberately has no such connection.
    host.lock()
        .unwrap()
        .install_model(model::Registry::at(&root.join("registry.sqlite3")).unwrap());
    assert!(host.lock().unwrap().execution_registry.is_some());
    host.lock().unwrap().enable_holder_recovery();
    // Inject a real bind failure after acquiring the actual lease. This socket
    // is only this fixture's blocker; it never accepts or serves a client.
    let blocker = UnixListener::bind(&paths.socket).unwrap();
    let blocker_stat = fs::symlink_metadata(&paths.socket).unwrap();
    let restored = Cell::new(false);
    let restore = || {
        state::restore_upgrade(&host).map_err(io::Error::other)?;
        restored.set(true);
        Ok(())
    };
    let error = listener_before_restore(&paths, &restore).unwrap_err();
    assert!(matches!(error, HostError::Io(ref error) if error.kind() == io::ErrorKind::AddrInUse));
    assert!(!restored.get());
    assert!(HostLease::is_held(&paths).unwrap());
    assert!(host.lock().unwrap().terminals.is_empty());
    assert_eq!(fs::read(&store.path).unwrap(), checkpoint_bytes);
    let unchanged = fs::metadata(&store.path).unwrap();
    assert_eq!(
        (unchanged.dev(), unchanged.ino()),
        (checkpoint_stat.dev(), checkpoint_stat.ino())
    );
    assert_eq!(store.load().unwrap().unwrap().phase, Phase::Ready);
    assert_eq!(
        crate::terminal::holder::evidence(&holder_socket).unwrap(),
        Some(original.clone())
    );
    assert!(original_root.live() && original_holder.live());
    assert_eq!(fs::read(root.join("launches")).unwrap(), b"launched\n");

    // The positive control invokes the SAME real restore through the gate.
    // Only remove the exact private blocker inode this test just created.
    drop(blocker);
    let named = fs::symlink_metadata(&paths.socket).unwrap();
    assert_eq!(
        (named.dev(), named.ino()),
        (blocker_stat.dev(), blocker_stat.ino())
    );
    assert!(named.file_type().is_socket());
    fs::remove_file(&paths.socket).unwrap();
    let listener = listener_before_restore(&paths, &restore).unwrap();
    assert!(restored.get());
    assert!(store.load().unwrap().is_none());
    assert_eq!(fs::metadata(&paths.socket).unwrap().mode() & 0o777, 0o600);
    // SAFETY: fcntl observes this test's still-owned listener descriptor.
    let flags = unsafe { libc::fcntl(listener.as_raw_fd(), libc::F_GETFL) };
    assert!(flags >= 0, "{}", io::Error::last_os_error());
    assert_ne!(flags & libc::O_NONBLOCK, 0);
    let successor = host.lock().unwrap().terminals[&source.id].handle.clone();
    assert_eq!(successor.holder_identity(), Some(&original));
    assert!(original_root.live() && original_holder.live());
    // Adoption restores the cached screen with a hidden worker. Follow the
    // public Attach + Visible host path before requiring fresh PTY snapshots.
    {
        let mut state = host.lock().unwrap();
        state.register_client(1);
        state.attach(1, source.id).unwrap();
        state
            .set_visibility(
                1,
                source.id,
                crate::terminal_ipc::TerminalVisibility::Visible,
            )
            .unwrap();
    }
    visible_text(&successor, "listener-ready");
    successor.input(b"after-bind\n".to_vec()).unwrap();
    visible_text(&successor, "continued:after-bind");
    assert_eq!(fs::read(root.join("launches")).unwrap(), b"launched\n");
    assert!(
        finish_terminal_shutdown(&[successor])
            .into_iter()
            .all(|result| result.is_ok())
    );
    original_root.assert_reaped();
    // The first worker handed its actual Child to the existing reaper when
    // carried. Joining the adopted worker proves its holder exited, but the
    // original parent's wait may still be completing for that same process.
    let reaped_by = Instant::now() + Duration::from_secs(5);
    while Process::read(original_holder.process.pid)
        .is_ok_and(|current| current == original_holder.process)
    {
        assert!(
            Instant::now() < reaped_by,
            "original carried holder was not reaped"
        );
        thread::sleep(Duration::from_millis(10));
    }
    original_holder.assert_reaped();
    assert!(HostLease::is_held(&paths).unwrap());
    drop(listener);
    drop(lease);
    assert!(!paths.socket.exists());
    assert!(!HostLease::is_held(&paths).unwrap());
}

#[test]
fn cleanup_refusal_is_reported_and_healthy_owners_still_stop() {
    let Some(root) = private_runtime(TEST, false) else {
        return;
    };
    let proof = HostSessionProof {
        host_session_id: HostSessionId::new("shutdown-error"),
        boot_id: daemon::read_boot_id().unwrap(),
        host_incarnation: HostIncarnation::new("shutdown-error"),
    };
    let paths = daemon::RuntimePaths::discover().unwrap();
    let _lease = daemon::HostLease::acquire(&paths, &proof).unwrap();
    let id = TerminalId::from_uuid(Uuid::from_u128(1));
    let socket = root.join(format!("cagents-codex-{id}/control.sock"));
    let source = chat::Session {
        id,
        launch: chat::Launch {
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
        native_continuity: chat::NativeContinuity::ReconnectOnly,
        owned_codex_listener: Some(chat::OwnedCodexListener {
            socket: socket.clone(),
            allocator: proof.clone(),
        }),
    };
    // Genuine execed helper and owned root; this is not a Codex conversation fixture.
    let holder = chat::native_holder::spawn(
        ProcessSpec {
            stdout_noise: None,
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
    let evidence = registry::evidence(&registry::socket(&directory, id), Kind::Native)
        .unwrap()
        .unwrap();
    let failed_pin = ProcessPin::capture(evidence.root.clone());
    assert!(failed_pin.live());
    let receipt = directory.join(format!(
        "{id}-{}-{}.closed",
        evidence.holder.pid, evidence.holder.start
    ));
    let failed = quiet_chat(&root);
    failed.close().unwrap();
    struct Cleanup {
        handle: Handle,
        receipt: PathBuf,
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
    invalid.write_all(b"invalid cleanup receipt").unwrap();
    drop(invalid);
    failed.fixture_retained_native_holder(holder, "Native teardown remains unconfirmed".into());
    let healthy = quiet_chat(&root);
    let mut healthy_source = source.clone();
    healthy_source.id = TerminalId::from_uuid(Uuid::from_u128(2));
    healthy_source.launch.recipe.program = "/bin/cat".into();
    healthy_source.launch.recipe.arguments.clear();
    healthy_source.owned_codex_listener = None;
    let session = terminal(&root);
    let pty = TerminalHandle::spawn(session.clone(), TerminalConfig::default(), |_| {}).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    let pid: i32 = loop {
        if let Ok(text) = fs::read_to_string(root.join("root-pid")) {
            if let Ok(pid) = text.parse() {
                break pid;
            }
        }
        assert!(Instant::now() < deadline, "healthy PTY did not start");
        thread::sleep(Duration::from_millis(5));
    };
    let pty_pin = ProcessPin::capture(Process::read(pid).unwrap());
    assert_eq!(fs::read_link(format!("/proc/{pid}/cwd")).unwrap(), root);
    let mut state = HostState::new(proof);
    state.register_client(1);
    state.finish_spawn(1, session, pty.clone(), true).unwrap();
    state.chats.insert(id, record(source, failed.clone()));
    state
        .chats
        .insert(healthy_source.id, record(healthy_source, healthy.clone()));
    let (handles, _) = state.graceful_shutdown();
    let host: SharedHost = Arc::new(state::HostLock::new(state));
    let error = finish_chat_shutdown(&host).unwrap_err();
    assert!(
        error.contains(&format!("Provider {id} shutdown was not confirmed")),
        "{error}"
    );
    assert!(!failed.is_completed());
    assert!(
        failed_pin.live(),
        "refused cleanup must retain the original owner"
    );
    assert!(
        healthy.is_completed(),
        "later healthy provider must still close"
    );
    // Same terminal request-all/wait-all order as the actual run tail, after Err.
    for handle in &handles {
        handle.request_shutdown();
    }
    for handle in handles {
        handle.shutdown().unwrap();
    }
    assert!(pty.is_completed());
    pty_pin.assert_reaped();
    assert!(
        failed_pin.live(),
        "healthy teardown must not release failed custody"
    );
    fs::remove_file(&receipt).unwrap();
    failed.close().unwrap();
    assert!(failed.is_completed());
    failed_pin.assert_reaped();
    drop(cleanup);
}

#[test]
fn ordinary_shutdown_commits_final_outcomes_and_retires_completed_retained_actor() {
    ordinary_retained_shutdown(
        "host::provider_shutdown_tests::ordinary_shutdown_commits_final_outcomes_and_retires_completed_retained_actor",
        false,
    );
}

#[test]
fn ordinary_shutdown_retains_uncommitted_actor_and_still_closes_healthy_owners() {
    ordinary_retained_shutdown(
        "host::provider_shutdown_tests::ordinary_shutdown_retains_uncommitted_actor_and_still_closes_healthy_owners",
        true,
    );
}

fn ordinary_retained_shutdown(test: &str, fail_commit: bool) {
    use crate::{agent::Provider, model::ProjectKind, workspace_model::ModelEdit};

    let Some(root) = private_runtime(test, true) else {
        return;
    };
    // The actual same-head cagentsd helper execs this private SDK carrier. It
    // finishes one genuine protocol turn and exits before ordinary shutdown.
    // The gate puts final outcomes after graceful_shutdown stops new broker
    // work. An already admitted drain may finish, so success checks the actual
    // journal clock rather than assuming a pending page cannot be consumed.
    for name in ["bin", "project", "home/.claude"] {
        fs::create_dir_all(root.join(name)).unwrap();
    }
    let provider = root.join("bin/claude");
    fs::write(
        &provider,
        r#"#!/usr/bin/python3
import json, os, pathlib, sys, time
root = pathlib.Path(__file__).resolve().parent.parent
(root / 'provider-pid').write_text(str(os.getpid()))
session = next((v.split('=', 1)[1] for v in sys.argv[1:] if v.startswith('--session-id=')), 'aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa')
def send(value): print(json.dumps(value), flush=True)
for line in sys.stdin:
    frame = json.loads(line)
    if frame.get('type') == 'control_request':
        send({'type':'control_response', 'response':{'subtype':'success', 'request_id':frame['request_id'], 'response':{}}})
    elif frame.get('type') == 'user':
        (root / 'provider-received').write_text('received')
        deadline = time.monotonic() + 15
        while not (root / 'release-final').exists():
            if time.monotonic() >= deadline: raise RuntimeError('final-outcome gate timed out')
            time.sleep(0.01)
        received = frame['uuid']
        send({'type':'user', 'uuid':received, 'session_id':session, 'message':{'role':'user', 'content':[{'type':'text', 'text':'One final ordinary shutdown turn'}]}})
        send({'type':'assistant', 'uuid':'final-answer', 'session_id':session, 'message':{'role':'assistant', 'content':[{'type':'text', 'text':'Final ordinary shutdown answer'}]}})
        send({'type':'result', 'uuid':'final-result', 'subtype':'success', 'is_error':False, 'session_id':session})
        (root / 'provider-done').write_text('done')
        break
"#,
    )
    .unwrap();
    fs::set_permissions(&provider, fs::Permissions::from_mode(0o700)).unwrap();
    let proof = HostSessionProof {
        host_session_id: HostSessionId::new("ordinary-retained-shutdown"),
        boot_id: daemon::read_boot_id().unwrap(),
        host_incarnation: HostIncarnation::new("ordinary-retained-shutdown"),
    };
    let paths = daemon::RuntimePaths::discover().unwrap();
    let _lease = daemon::HostLease::acquire(&paths, &proof).unwrap();
    let registry_path = root.join("registry.sqlite3");
    let mut registry = model::Registry::at(&registry_path).unwrap();
    registry
        .apply(&ModelEdit::OpenProject {
            repository: root.join("project"),
            name: "Ordinary retained shutdown".into(),
            kind: ProjectKind::Folder,
        })
        .unwrap();
    let mut state = HostState::new(proof.clone());
    state.install_model(registry);
    let host = Arc::new(state::HostLock::new(state));
    let notifications =
        notifications::Broker::start(&host, crate::notifications::Application::Cagents).unwrap();
    host.lock().unwrap().notifications = Some(notifications.handle());
    let source = chat::fresh_session(
        Provider::Claude,
        TerminalId::from_uuid(Uuid::from_u128(1)),
        root.join("project"),
        &crate::account::Account::system(Provider::Claude),
    )
    .unwrap();
    let id = source.id;
    super::chat::start_execution(
        &host,
        0,
        source,
        Arc::new(chat::protocol::ExecutionContext::capture(&root.join("project")).unwrap()),
    )
    .unwrap();
    // Cleanup retries the same semantic path after removing only the injected
    // private Store fault. The external guardian still owns failure-family
    // settlement; this never treats a pidfd kill as a successful retirement.
    struct Cleanup {
        host: SharedHost,
        root: PathBuf,
    }
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = fs::write(self.root.join("release-final"), b"release");
            if let Ok(db) = rusqlite::Connection::open(self.root.join("registry.sqlite3")) {
                let _ = db.execute_batch("DROP TRIGGER IF EXISTS fail_shutdown_commit;");
            }
            let _ = finish_chat_shutdown(&self.host);
        }
    }
    let handle = host.lock().unwrap().chats[&id].handle.clone();
    let identity = handle.actor_identity().expect("real retained actor");
    let helper_pin = ProcessPin::capture(identity.record.holder.clone());
    let provider_pin = ProcessPin::capture(identity.record.root.clone());
    let cleanup = Cleanup {
        host: host.clone(),
        root: root.clone(),
    };
    assert!(helper_pin.live() && provider_pin.live());
    let wait = |condition: &mut dyn FnMut() -> bool, reason: &str| {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !condition() {
            assert!(Instant::now() < deadline, "{reason}");
            thread::sleep(Duration::from_millis(10));
        }
    };
    wait(&mut || handle.update_activity().1, "SDK did not initialize");
    assert_eq!(
        fs::read_to_string(root.join("provider-pid"))
            .unwrap()
            .parse::<i32>()
            .unwrap(),
        identity.record.root.pid
    );
    handle
        .command(chat::Command::Prompt {
            text: "One final ordinary shutdown turn".into(),
            attachments: Vec::new(),
            mode: chat::PromptMode::Send,
        })
        .unwrap();
    wait(
        &mut || root.join("provider-received").exists(),
        "SDK did not consume the prompt",
    );
    let healthy = quiet_chat(&root);
    let mut healthy_source = host.lock().unwrap().chats[&id].session.clone();
    healthy_source.id = TerminalId::from_uuid(Uuid::from_u128(2));
    assert!(
        id < healthy_source.id,
        "healthy owner must follow the refused owner"
    );
    healthy_source.launch.provider = Provider::Omp;
    healthy_source.launch.recipe.program = "/bin/cat".into();
    healthy_source.launch.recipe.arguments.clear();
    healthy_source.launch.session_id = None;
    healthy_source.native_continuity = Default::default();
    {
        let mut state = host.lock().unwrap();
        state
            .chats
            .insert(healthy_source.id, record(healthy_source, healthy.clone()));
        state.graceful_shutdown();
    }
    let registry = agent_execution::registry(&host).unwrap();
    let committed_before = registry
        .lock()
        .unwrap()
        .native_actor_committed_through(identity.instance, &identity.record)
        .unwrap();
    let fault = rusqlite::Connection::open(&registry_path).unwrap();
    if fail_commit {
        fault.execute_batch("CREATE TRIGGER fail_shutdown_commit BEFORE INSERT ON native_actor_cursors BEGIN SELECT RAISE(ABORT, 'fixture final Store failure'); END;").unwrap();
    }
    fs::write(root.join("release-final"), b"release").unwrap();
    wait(
        &mut || {
            root.join("provider-done").exists()
                && handle
                    .retained_outcome_clock_until(Instant::now() + Duration::from_secs(2))
                    .is_ok_and(|(_, latest)| latest > committed_before)
                && !provider_pin.live()
                && !identity.record.root.is_current()
        },
        "completed SDK did not journal its real final outcomes",
    );
    provider_pin.assert_reaped();
    let (_, final_sequence) = handle
        .retained_outcome_clock_until(Instant::now() + Duration::from_secs(2))
        .unwrap();
    assert!(final_sequence > committed_before);
    if fail_commit {
        let pending = handle.uncommitted_outcomes().unwrap();
        assert!(pending.iter().any(|entry| matches!(
            &entry.update,
            chat::runtime::Update::Attention(_) | chat::runtime::Update::AttentionContext { .. }
        )));
        let error = finish_chat_shutdown(&host).unwrap_err();
        assert!(error.contains("fixture final Store failure"), "{error}");
        assert!(helper_pin.live(), "failed commit must retain the helper");
        assert!(healthy.is_completed(), "healthy owner was skipped");
        assert_eq!(
            registry
                .lock()
                .unwrap()
                .native_actor_committed_through(identity.instance, &identity.record)
                .unwrap(),
            committed_before
        );
        assert!(
            registry
                .lock()
                .unwrap()
                .native_actor_retirement(identity.instance)
                .unwrap()
                .is_none()
        );
        assert!(!handle.uncommitted_outcomes().unwrap().is_empty());
        fault
            .execute_batch("DROP TRIGGER fail_shutdown_commit;")
            .unwrap();
    }
    finish_chat_shutdown(&host).unwrap();
    assert!(healthy.is_completed());
    let registry = registry.lock().unwrap();
    assert!(
        registry
            .native_actor_committed_through(identity.instance, &identity.record)
            .unwrap()
            >= final_sequence
    );
    let retired = registry
        .native_actor_retirement(identity.instance)
        .unwrap()
        .expect("durable exact semantic retirement");
    assert_eq!(retired.actor(), identity.instance);
    assert_eq!(retired.owner(), &identity.record);
    assert_eq!(retired.source().id, id);
    assert!(retired.confirmed_session().is_some());
    drop(registry);
    wait(
        &mut || !helper_pin.live() && !identity.record.holder.is_current(),
        "semantically retired helper did not exit and reap",
    );
    helper_pin.assert_reaped();
    let directory = registry::directory(Kind::Native).unwrap();
    assert!(
        registry::discover(&directory, &proof.boot_id, Kind::Native)
            .unwrap()
            .is_empty()
    );
    drop(cleanup);
}
