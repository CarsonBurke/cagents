//! Actual private HOST/RPC exit coverage. Each function self-execs before it
//! closes process-global reservation admission; no test resets/reopens SERVICE.
//! RPC clients are predetermined fixture User clients, not delegated CLI proof.
use super::*;
use crate::{
    daemon::{child_reaper, holder_registry::Process},
    model::{LaunchRecipe, TerminalContinuity, TerminalId, TerminalSession},
    terminal::TerminalSpawnOptions,
    terminal_ipc::{ClientEnvelope, ClientRequest, ServerResponse, read_frame},
};
use std::{
    fs::File,
    io::Read,
    os::fd::{AsRawFd, FromRawFd},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{OnceLock, mpsc},
    time::{Duration, Instant},
};

const ROOT: &str = "CAGENTS_EXIT_DRAIN_TEST_ROOT";
const TEST: &str = "CAGENTS_EXIT_DRAIN_TEST_NAME";
const SCOPE: &str = "CAGENTS_EXIT_DRAIN_INHERITED_SCOPE";
static READY: OnceLock<mpsc::Sender<Result<SharedHost, String>>> = OnceLock::new();

pub(super) fn observe_ready(host: &SharedHost) {
    if let Some(ready) = READY.get() {
        let _ = ready.send(Ok(host.clone()));
    }
}

struct TestChild(Child);
impl Drop for TestChild {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}
struct EvidenceDirectory(Option<tempfile::TempDir>);
impl Drop for EvidenceDirectory {
    fn drop(&mut self) {
        if let Some(root) = self.0.take() {
            let _ = root.keep();
        }
    }
}

fn private_runtime(name: &str) -> Option<PathBuf> {
    private_runtime_case(name, None)
}

pub(super) fn private_runtime_case(name: &str, case: Option<&str>) -> Option<PathBuf> {
    private_runtime_options(name, case, false)
}

fn private_runtime_retained_actor(name: &str) -> Option<PathBuf> {
    let helper = PathBuf::from(
        std::env::var_os("CAGENTS_TEST_RETAINED_ACTOR_HELPER")
            .expect("real startup exit fixture requires the same-head cagentsd executable"),
    );
    assert!(
        helper.is_absolute(),
        "same-head helper path must be absolute"
    );
    private_runtime_options(name, None, true)
}

fn private_runtime_options(
    name: &str,
    case: Option<&str>,
    retained_actor: bool,
) -> Option<PathBuf> {
    const CASE: &str = "CAGENTS_EXIT_DRAIN_TEST_CASE";
    if let Some(root) = std::env::var_os(ROOT) {
        assert_eq!(std::env::var(TEST).as_deref(), Ok(name));
        assert_eq!(std::env::var(CASE).ok().as_deref(), case);
        let expected = match std::env::var(SCOPE).as_deref() {
            Ok("present") => true,
            Ok("absent") => false,
            _ => panic!("missing original inherited authority presence"),
        };
        assert_eq!(
            crate::agent_execution::Authority::from_environment()
                != crate::agent_execution::Authority::default(),
            expected
        );
        let root = PathBuf::from(root).canonicalize().unwrap();
        assert_eq!(std::env::current_dir().unwrap(), root);
        return Some(root);
    }
    let directory = tempfile::Builder::new().prefix("ed-").tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let _retained = EvidenceDirectory(Some(directory));
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
    for name in ["home", "run", "data", "config", "cache", "tmp"] {
        fs::create_dir(root.join(name)).unwrap();
        fs::set_permissions(root.join(name), fs::Permissions::from_mode(0o700)).unwrap();
    }
    let mut command = Command::new(std::env::current_exe().unwrap());
    // Leave the actual caller's opaque delegation inherited. Only a public
    // equality-derived presence boolean crosses this fixture's boundary.
    for (key, _) in std::env::vars_os() {
        if key != std::ffi::OsStr::new(crate::agent_execution::AUTHORITY_ENV)
            && !(retained_actor
                && key == std::ffi::OsStr::new("CAGENTS_TEST_RETAINED_ACTOR_HELPER"))
        {
            command.env_remove(key);
        }
    }
    let stdout = root.join("test.stdout");
    let stderr = root.join("test.stderr");
    command
        .args(["--exact", name, "--nocapture", "--test-threads=1"])
        .current_dir(&root)
        .env(ROOT, &root)
        .env(TEST, name)
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
    if let Some(case) = case {
        command.env(CASE, case);
    }
    if retained_actor {
        fs::create_dir(root.join("home/.claude")).unwrap();
        fs::create_dir(root.join("project")).unwrap();
        command
            .env("CAGENTS_RETAINED_ACTOR_FIXTURE", "1")
            .env("CLAUDE_CONFIG_DIR", root.join("home/.claude"));
    }
    let mut child = TestChild(command.spawn().unwrap());
    let deadline = Instant::now() + Duration::from_secs(60);
    let status = loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            break Some(status);
        }
        if Instant::now() >= deadline {
            child.0.kill().unwrap();
            child.0.wait().unwrap();
            break None;
        }
        thread::sleep(Duration::from_millis(5));
    };
    let out = fs::read_to_string(&stdout).unwrap();
    let err = fs::read_to_string(&stderr).unwrap();
    assert!(
        status.is_some_and(|s| s.success()),
        "private HOST exit child failed/timed out; evidence {}:\n{out}\n{err}",
        root.display()
    );
    assert!(out.contains(&format!("test {name} ... ok")));
    assert!(out.contains("test result: ok. 1 passed; 0 failed; 0 ignored;"));
    None
}

/// Same public framing as an ordinary client. Authority is fixed as fixture
/// User BEFORE operations; there is no refusal/catch/retry or inherited token use.
struct Rpc {
    stream: UnixStream,
    proof: HostSessionProof,
    next: u64,
}
impl Rpc {
    fn connect(socket: &Path) -> Self {
        let stream = UnixStream::connect(socket).unwrap();
        Self::from_stream(stream)
    }
    fn from_stream(mut stream: UnixStream) -> Self {
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        write_frame(
            &mut stream,
            &ClientEnvelope {
                version: PROTOCOL_VERSION,
                request_id: 1,
                proof: None,
                request: ClientRequest::Connect {
                    expected_host: None,
                    resume: Vec::new(),
                },
            },
        )
        .unwrap();
        let envelope: ServerEnvelope = read_frame(&mut stream).unwrap().unwrap();
        let ServerMessage::Response {
            response: ServerResponse::Connected { proof, .. },
        } = envelope.message
        else {
            panic!("actual private HOST must complete a fresh Connect");
        };
        assert_eq!(envelope.proof, proof);
        let mut rpc = Self {
            stream,
            proof,
            next: 2,
        };
        assert!(matches!(
            rpc.request(ClientRequest::Authenticate {
                authority: crate::agent_execution::Authority::default(),
            })
            .unwrap(),
            ServerResponse::Accepted
        ));
        rpc
    }
    fn request(&mut self, request: ClientRequest) -> Result<ServerResponse, ProtocolError> {
        let id = self.next;
        self.next += 1;
        write_frame(
            &mut self.stream,
            &ClientEnvelope {
                version: PROTOCOL_VERSION,
                request_id: id,
                proof: Some(self.proof.clone()),
                request,
            },
        )
        .unwrap();
        loop {
            let envelope: ServerEnvelope = read_frame(&mut self.stream).unwrap().unwrap();
            assert_eq!(envelope.version, PROTOCOL_VERSION);
            assert_eq!(envelope.proof, self.proof);
            if envelope.request_id != Some(id) {
                continue;
            }
            return match envelope.message {
                ServerMessage::Response { response } => Ok(response),
                ServerMessage::Error(error) => Err(error),
                other => panic!("unexpected correlated response {other:?}"),
            };
        }
    }
}

struct OriginalPin {
    process: Process,
    pidfd: File,
    retired: bool,
}
impl OriginalPin {
    fn new(pid: u32) -> Self {
        let process = Process::read(pid as i32).unwrap();
        let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
        assert!(raw >= 0 && process.is_current());
        Self {
            process,
            pidfd: unsafe { File::from_raw_fd(raw as i32) },
            retired: false,
        }
    }
    fn exited(&self) -> bool {
        let mut poll = libc::pollfd {
            fd: self.pidfd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        assert!(unsafe { libc::poll(&mut poll, 1, 0) } >= 0);
        poll.revents & libc::POLLIN != 0
    }
    fn prove_reaped(&mut self) {
        assert!(self.exited());
        match Process::read(self.process.pid) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Ok(current) if current != self.process => {}
            Ok(_) => panic!("exit without the original Child.wait is not reaping"),
            Err(error) => panic!("original reaping could not be observed: {error}"),
        }
        self.retired = true;
    }
}
impl Drop for OriginalPin {
    fn drop(&mut self) {
        if !self.retired && !self.exited() {
            // Failure-only cleanup addresses ONLY this already pinned original.
            unsafe {
                libc::syscall(
                    libc::SYS_pidfd_send_signal,
                    self.pidfd.as_raw_fd(),
                    libc::SIGKILL,
                    std::ptr::null::<libc::siginfo_t>(),
                    0,
                );
            }
        }
    }
}
fn assert_lease_held(paths: &RuntimePaths) {
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&paths.lock)
        .unwrap();
    assert_eq!(
        unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        -1
    );
    assert_eq!(io::Error::last_os_error().kind(), io::ErrorKind::WouldBlock);
}
fn wait_for(mut predicate: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !predicate() {
        assert!(
            Instant::now() < deadline,
            "bounded actual HOST transition did not settle"
        );
        thread::sleep(Duration::from_millis(5));
    }
}
fn session(root: &Path) -> TerminalSession {
    let recipe = LaunchRecipe {
        program: "/bin/cat".into(),
        arguments: Vec::new(),
        working_directory: root.into(),
        environment: Default::default(),
        unset_environment: Default::default(),
    };
    TerminalSession {
        id: TerminalId::new(),
        launch: recipe.clone(),
        continuity: TerminalContinuity::RelaunchAvailable {
            recipe,
            last_exit: None,
        },
        resume_hint: None,
        transcript_path: None,
        owned_codex_listener: None,
        recovery_origin: None,
        predecessor: None,
    }
}
fn run_host() -> (
    SharedHost,
    mpsc::Receiver<Result<(), String>>,
    JoinHandle<()>,
) {
    let (ready, observed) = mpsc::channel();
    READY.set(ready.clone()).unwrap();
    // A real private launch fence/HostLease is acquired; no daemon guard is
    // fabricated and no process-wide authority marker is removed or changed.
    let admission = DaemonAdmission {
        _guard: crate::instance::DaemonAdmissionGuard::acquire().unwrap(),
    };
    let (complete, completion) = mpsc::channel();
    let thread = thread::spawn(move || {
        let result = run(admission).map_err(|e| e.to_string());
        // The same thread publishes Ready before run can return. Its later
        // diagnostic cannot overtake readiness or consume the completion result.
        let diagnostic = match &result {
            Err(error) => format!("actual host::run failed before readiness: {error}"),
            Ok(()) => "actual host::run returned before readiness without an error".into(),
        };
        let _ = ready.send(Err(diagnostic));
        let _ = complete.send(result);
    });
    let host = observed
        .recv_timeout(Duration::from_secs(8))
        .expect("actual host::run ready");
    let host = match host {
        Ok(host) => host,
        Err(error) => {
            // Receiving this diagnostic proves run already returned; the
            // remaining unbounded-channel send does not wait for a reader.
            thread.join().unwrap();
            panic!("{error}");
        }
    };
    (host, completion, thread)
}

#[test]
fn startup_error_reports_the_actual_host_failure_instead_of_ready_timeout() {
    const NAME: &str = "host::exit_drain_tests::startup_error_reports_the_actual_host_failure_instead_of_ready_timeout";
    let Some(_root) = private_runtime(NAME) else {
        return;
    };
    let paths = RuntimePaths::discover().unwrap();
    let proof = HostSessionProof {
        host_session_id: HostSessionId::new("startup-error-original"),
        host_incarnation: HostIncarnation::new("held-private-lease"),
        boot_id: daemon::read_boot_id().unwrap(),
    };
    // Actual run must refuse this independently opened kernel flock before any
    // producer can start. Neither a synthetic run result nor a sleep supplies it.
    let lease = HostLease::acquire(&paths, &proof).unwrap();
    let expected = PlatformError::AlreadyRunning {
        path: paths.socket.clone(),
    }
    .to_string();
    let panic = match std::panic::catch_unwind(run_host) {
        Err(panic) => panic,
        Ok(_) => panic!("actual host::run unexpectedly acquired the held lease"),
    };
    let message = panic
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| panic.downcast_ref::<&str>().copied())
        .expect("startup diagnostic must be a string panic");
    assert!(
        message.contains(&expected),
        "actual startup cause: {message}"
    );
    assert!(
        !message.contains("Timeout"),
        "masked startup cause: {message}"
    );
    assert_lease_held(&paths);
    assert!(!paths.socket.exists());
    assert!(!child_reaper::has_outstanding_ownership());
    drop(lease);
    assert!(!HostLease::is_held(&paths).unwrap());
}

#[test]
fn ordinary_host_exit_keeps_fresh_clients_and_lease_until_late_original_is_reaped() {
    const NAME: &str = "host::exit_drain_tests::ordinary_host_exit_keeps_fresh_clients_and_lease_until_late_original_is_reaped";
    let Some(root) = private_runtime(NAME) else {
        return;
    };
    let paths = RuntimePaths::discover().unwrap();
    let ticket = child_reaper::reserve().unwrap();
    let mut child = Command::new("/bin/cat")
        .current_dir(&root)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let stdin = child.stdin.take().unwrap();
    let mut original = OriginalPin::new(child.id());
    assert!(
        !child_reaper::has_retained_ownership(),
        "the outstanding original is not yet published"
    );
    let (host, completed, running) = run_host();
    let mut before = Rpc::connect(&paths.socket);
    assert!(matches!(
        before.request(ClientRequest::Shutdown).unwrap(),
        ServerResponse::ShutdownAccepted
    ));
    wait_for(|| host.lock().unwrap().ordinary_exit_cleanup_pending());
    assert!(
        child_reaper::reserve().is_err(),
        "the real caller must close new reservations"
    );
    assert!(completed.try_recv().is_err());
    assert!(original.process.is_current() && !original.exited());
    assert_lease_held(&paths);
    // This new post-pending handshake exercises prepare_resume, not just a
    // process_request call on an already authenticated fixture connection.
    let mut fresh = Rpc::connect(&paths.socket);
    assert!(
        matches!(fresh.request(ClientRequest::List).unwrap(), ServerResponse::Listed { terminals } if terminals.is_empty())
    );
    for request in [
        ClientRequest::Input {
            terminal_id: TerminalId::new(),
            bytes: b"not admitted".to_vec(),
        },
        ClientRequest::Spawn {
            session: session(&root),
            options: TerminalSpawnOptions::default(),
        },
        ClientRequest::Shutdown,
    ] {
        assert_eq!(
            fresh.request(request).unwrap_err().code,
            ProtocolErrorCode::TerminalUnavailable
        );
    }
    // Publish the SAME original only after real ordinary shutdown admitted the
    // empty inventory. No producer takes/blocks on the host control writer.
    ticket.quarantine();
    ticket
        .retain(child, original.pidfd.try_clone().unwrap())
        .unwrap();
    assert!(child_reaper::has_retained_ownership());
    assert!(completed.try_recv().is_err());
    assert!(original.process.is_current());
    assert_lease_held(&paths);
    assert!(
        matches!(fresh.request(ClientRequest::List).unwrap(), ServerResponse::Listed { terminals } if terminals.is_empty())
    );
    drop(stdin); // natural EOF, no normal-path signal or invented cleanup ACK
    assert!(
        completed
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .is_ok()
    );
    // Keep BOTH clients open through completion: final current-delivery closure
    // must release their actual reader/writer joins without peer cooperation.
    running.join().unwrap();
    original.prove_reaped();
    assert!(!paths.socket.exists());
    assert!(!child_reaper::has_retained_ownership());
    drop(fresh);
    drop(before);
}

#[test]
fn healthy_host_exit_joins_its_actual_terminal_without_reopening_reservations() {
    const NAME: &str = "host::exit_drain_tests::healthy_host_exit_joins_its_actual_terminal_without_reopening_reservations";
    let Some(root) = private_runtime(NAME) else {
        return;
    };
    let paths = RuntimePaths::discover().unwrap();
    let (host, completed, running) = run_host();
    let mut rpc = Rpc::connect(&paths.socket);
    let source = session(&root);
    assert!(
        matches!(rpc.request(ClientRequest::Spawn { session: source.clone(), options: TerminalSpawnOptions::default() }).unwrap(), ServerResponse::Spawned { terminal } if terminal.session.id == source.id)
    );
    let owner = host.lock().unwrap().terminals[&source.id]
        .handle
        .holder_identity()
        .unwrap()
        .clone();
    let mut root_pin = OriginalPin::new(owner.root.pid as u32);
    let mut holder_pin = OriginalPin::new(owner.holder.pid as u32);
    assert!(matches!(
        rpc.request(ClientRequest::Shutdown).unwrap(),
        ServerResponse::ShutdownAccepted
    ));
    assert!(
        completed
            .recv_timeout(Duration::from_secs(10))
            .unwrap()
            .is_ok()
    );
    running.join().unwrap();
    root_pin.prove_reaped();
    holder_pin.prove_reaped();
    assert!(!paths.socket.exists());
    assert!(
        child_reaper::reserve().is_err(),
        "sealed exit never reopens new admission"
    );
    drop(rpc);
}

#[test]
fn pending_exit_keeps_actual_checked_close_and_fences_late_registration() {
    const NAME: &str = "host::exit_drain_tests::pending_exit_keeps_actual_checked_close_and_fences_late_registration";
    let Some(root) = private_runtime(NAME) else {
        return;
    };
    let proof = HostSessionProof {
        host_session_id: HostSessionId::new("exit-close"),
        host_incarnation: HostIncarnation::new("exit-close"),
        boot_id: daemon::read_boot_id().unwrap(),
    };
    let paths = RuntimePaths::discover().unwrap();
    let lease = HostLease::acquire(&paths, &proof).unwrap();
    let mut state = HostState::new(proof);
    state.enable_holder_recovery();
    let host = Arc::new(state::HostLock::new(state));
    let (peer, server) = UnixStream::pair().unwrap();
    let serve_host = host.clone();
    let serving = thread::spawn(move || client::serve(server, 1, serve_host));
    let mut rpc = Rpc::from_stream(peer);
    let source = session(&root);
    rpc.request(ClientRequest::Spawn {
        session: source.clone(),
        options: TerminalSpawnOptions::default(),
    })
    .unwrap();
    let owner = host.lock().unwrap().terminals[&source.id]
        .handle
        .holder_identity()
        .unwrap()
        .clone();
    let mut root_pin = OriginalPin::new(owner.root.pid as u32);
    let mut holder_pin = OriginalPin::new(owner.holder.pid as u32);
    let drain = begin_ordinary_exit(&host).expect("ordinary fixture, no committed successor");
    assert!(!drain.try_seal_absence());
    assert!(
        matches!(rpc.request(ClientRequest::Attach { terminal_id: source.id, after_generation: 0, after_event_sequence: 0 }).unwrap(), ServerResponse::Attached { terminal } if terminal.session.id == source.id)
    );
    assert!(
        matches!(rpc.request(ClientRequest::Snapshot { terminal_id: source.id, after_generation: 0 }).unwrap(), ServerResponse::Snapshot { terminal_id, .. } if terminal_id == source.id)
    );
    assert_eq!(
        rpc.request(ClientRequest::Input {
            terminal_id: source.id,
            bytes: b"not executed".to_vec()
        })
        .unwrap_err()
        .code,
        ProtocolErrorCode::TerminalUnavailable
    );
    assert!(owner.root.is_current());
    assert!(matches!(
        rpc.request(ClientRequest::CloseTerminalChecked {
            terminal_id: source.id,
            allow_active: true
        })
        .unwrap(),
        ServerResponse::TerminalCloseChecked {
            outcome: crate::terminal_ipc::TerminalCloseResult::Closed,
            ..
        }
    ));
    wait_for(|| drain.try_seal_absence());
    root_pin.prove_reaped();
    holder_pin.prove_reaped();
    let deliveries = host.lock().unwrap().close_ordinary_exit_registration();
    close_shutdown_deliveries(&host, &deliveries);
    wait_for(|| serving.is_finished());
    serving.join().unwrap();
    // A connection accepted BEFORE the final snapshot could start its thread
    // AFTER it. Actual Registration and writer closure must not wait for peer EOF.
    let (mut late, server) = UnixStream::pair().unwrap();
    late.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
    let serve_host = host.clone();
    let late_server = thread::spawn(move || client::serve(server, 2, serve_host));
    let mut byte = [0];
    assert_eq!(late.read(&mut byte).unwrap(), 0);
    wait_for(|| late_server.is_finished());
    late_server.join().unwrap();
    assert!(!host.lock().unwrap().clients.contains_key(&2));
    drop(lease);
}

mod native_startup_shutdown_tests;
mod startup_recovery_tests;
