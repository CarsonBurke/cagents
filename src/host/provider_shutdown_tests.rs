//! Real cleanup refusal must reach the host result without skipping other owners.
use super::*;
use crate::{
    chat::{self, protocol::{Adapter, ProcessSpec, Step, TransportSpec}, runtime::Handle},
    daemon::holder_registry::{self as registry, Kind, Process},
    model::{LaunchRecipe, TerminalContinuity, TerminalId, TerminalSession},
    terminal::{TerminalConfig, TerminalHandle},
};
use std::{
    fs::File,
    io::Write,
    os::{fd::{AsRawFd, FromRawFd}, unix::fs::OpenOptionsExt},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

const TEST: &str = "host::provider_shutdown_tests::cleanup_refusal_is_reported_and_healthy_owners_still_stop";
const ROOT: &str = "CAGENTS_PROVIDER_SHUTDOWN_TEST_ROOT";

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

fn private_runtime() -> Option<PathBuf> {
    if let Some(root) = std::env::var_os(ROOT) {
        return Some(PathBuf::from(root).canonicalize().unwrap());
    }
    use crate::process::{CancellationToken, CommandSpec, ProcessExecutor, SystemProcessExecutor};
    let directory = tempfile::tempdir_in("/var/tmp").unwrap();
    let root = directory.path().canonicalize().unwrap();
    let _evidence = EvidenceDirectory(Some(directory));
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
    for name in ["home", "run", "data", "config", "cache", "tmp"] {
        fs::create_dir(root.join(name)).unwrap();
        fs::set_permissions(root.join(name), fs::Permissions::from_mode(0o700)).unwrap();
    }
    let executable = std::env::current_exe().unwrap();
    let mut command = CommandSpec::new(&executable);
    // CommandSpec removes these before applying the explicit private values;
    // no process-wide environment mutation races another libtest thread.
    for (name, _) in std::env::vars_os() {
        command = command.env_remove(name);
    }
    command = command
        .args(["--exact", TEST, "--nocapture", "--test-threads=1"])
        .cwd(&root)
        .env(ROOT, &root)
        .env("HOME", root.join("home"))
        .env("XDG_RUNTIME_DIR", root.join("run"))
        .env("XDG_DATA_HOME", root.join("data"))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_CACHE_HOME", root.join("cache"))
        .env("TMPDIR", root.join("tmp"))
        .env("PATH", "/usr/bin:/bin")
        .timeout(Duration::from_secs(40))
        .output_limits(256 * 1024, 256 * 1024);
    let result = SystemProcessExecutor.execute(&command, &CancellationToken::new());
    let (stdout, stderr) = match &result {
        Ok(output) => (&output.stdout, &output.stderr),
        Err(error) => (&error.stdout, &error.stderr),
    };
    for (name, bytes) in [("stdout", stdout), ("stderr", stderr)] {
        let mut file = fs::OpenOptions::new().write(true).create_new(true)
            .mode(0o600).open(root.join(name)).unwrap();
        file.write_all(bytes).unwrap();
    }
    let output = result.unwrap_or_else(|error| panic!("private shutdown fixture retained at {}: {error}", root.display()));
    let output = output.require_success(&executable).unwrap_or_else(|error| panic!("private shutdown fixture retained at {}: {error}", root.display()));
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains(&format!("test {TEST} ... ok")), "wrong test output: {stdout}");
    assert!(stdout.contains("test result: ok. 1 passed; 0 failed; 0 ignored;"), "{stdout}");
    None
}

/// Observational incarnation plus retained pidfd. No signal uses a numeric PID.
struct ProcessPin { process: Process, descriptor: File }
impl ProcessPin {
    fn capture(process: Process) -> Self {
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, process.pid, 0) };
        assert!(fd >= 0, "{}", io::Error::last_os_error());
        let descriptor = unsafe { File::from_raw_fd(fd as i32) };
        assert_eq!(Process::read(process.pid).unwrap(), process);
        Self { process, descriptor }
    }
    fn live(&self) -> bool {
        let mut poll = libc::pollfd { fd:self.descriptor.as_raw_fd(), events:libc::POLLIN, revents:0 };
        let ready = unsafe { libc::poll(&mut poll, 1, 0) };
        assert!(ready >= 0, "{}", io::Error::last_os_error());
        ready == 0 && self.process.is_current()
    }
    fn assert_reaped(&self) {
        assert!(!self.live(), "original fixture process remains live");
        match Process::read(self.process.pid) {
            Ok(current) => assert_ne!(current, self.process, "original remains unreaped"),
            Err(error) if error.kind() == io::ErrorKind::NotFound || error.raw_os_error() == Some(libc::ESRCH) => {}
            Err(error) => panic!("inspect original fixture process: {error}"),
        }
    }
}
impl Drop for ProcessPin {
    fn drop(&mut self) {
        unsafe { libc::syscall(libc::SYS_pidfd_send_signal, self.descriptor.as_raw_fd(), libc::SIGKILL, std::ptr::null::<libc::siginfo_t>(), 0); }
    }
}

fn quiet_chat(root: &Path) -> Handle {
    struct Quiet;
    impl Adapter for Quiet {
        fn initialize(&mut self) -> Result<Step, String> { Ok(Step::default()) }
        fn command(&mut self, _:chat::Command) -> Result<Step, String> { Err("no provider work in this fixture".into()) }
        fn receive(&mut self, _:serde_json::Value) -> Result<Step, String> { Ok(Step::default()) }
    }
    let spec = ProcessSpec { execution:None, messages:None, unset_environment:Default::default(), transport:TransportSpec::JsonLines, program:"/bin/cat".into(), arguments:Vec::new(), working_directory:root.to_owned(), environment:Default::default() };
    let mut initial = Step::default();
    let store = chat::runtime::preflight(&spec, &mut initial).unwrap();
    Handle::start(spec, Box::new(Quiet), initial, store, None, |_| {}).unwrap()
}

fn record(session: chat::Session, handle: Handle) -> super::chat::Record {
    super::chat::Record { execution_instance:Uuid::new_v4(), console_recipe:None, paired_terminal:None, session, handle, source_process:None, subscribers:Default::default(), observations:Default::default(), completion_observed:false, returning_terminal:false }
}

fn terminal(root: &Path) -> TerminalSession {
    let recipe = LaunchRecipe { program:"/bin/sh".into(), arguments:vec!["-c".into(), "printf %s \"$$\" > root-pid; exec /bin/sleep 60".into()], working_directory:root.to_owned(), environment:Default::default(), unset_environment:Default::default() };
    TerminalSession { id:TerminalId::from_uuid(Uuid::from_u128(3)), launch:recipe.clone(), transcript_path:None, resume_hint:None, owned_codex_listener:None, recovery_origin:None, predecessor:None, continuity:TerminalContinuity::RelaunchAvailable { recipe,last_exit:None } }
}

#[test]
fn cleanup_refusal_is_reported_and_healthy_owners_still_stop() {
    let Some(root) = private_runtime() else { return; };
    let proof = HostSessionProof { host_session_id:HostSessionId::new("shutdown-error"), boot_id:daemon::read_boot_id().unwrap(), host_incarnation:HostIncarnation::new("shutdown-error") };
    let paths = daemon::RuntimePaths::discover().unwrap();
    let _lease = daemon::HostLease::acquire(&paths, &proof).unwrap();
    let id = TerminalId::from_uuid(Uuid::from_u128(1));
    let socket = root.join(format!("cagents-codex-{id}/control.sock"));
    let source = chat::Session { id, launch:chat::Launch { provider:crate::agent::Provider::Codex, recipe:LaunchRecipe { program:"codex".into(),arguments:vec!["--remote".into(),format!("unix://{}",socket.display())],working_directory:root.clone(),environment:Default::default(),unset_environment:Default::default() }, session_id:None },proof:Some(proof.clone()),terminal:None,native_continuity:chat::NativeContinuity::ReconnectOnly,owned_codex_listener:Some(chat::OwnedCodexListener { socket:socket.clone(),allocator:proof.clone() }) };
    // Genuine execed helper and owned root; this is not a Codex conversation fixture.
    let holder = chat::native_holder::spawn(ProcessSpec { execution:None,messages:None,unset_environment:Default::default(),transport:TransportSpec::OwnedUnixWebSocket { socket },program:"/bin/sleep".into(),arguments:vec!["60".into()],working_directory:root.clone(),environment:Default::default() }, source.clone()).unwrap();
    let directory = registry::directory(Kind::Native).unwrap();
    let evidence = registry::evidence(&registry::socket(&directory,id),Kind::Native).unwrap().unwrap();
    let failed_pin = ProcessPin::capture(evidence.root.clone());
    assert!(failed_pin.live());
    let receipt = directory.join(format!("{id}-{}-{}.closed",evidence.holder.pid,evidence.holder.start));
    let failed = quiet_chat(&root);
    failed.close().unwrap();
    struct Cleanup { handle:Handle, receipt:PathBuf }
    impl Drop for Cleanup {
        fn drop(&mut self) { let _ = fs::remove_file(&self.receipt); let _ = self.handle.close(); }
    }
    let cleanup = Cleanup { handle:failed.clone(),receipt:receipt.clone() };
    let mut invalid = fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&receipt).unwrap();
    invalid.write_all(b"invalid cleanup receipt").unwrap();
    drop(invalid);
    failed.fixture_retained_native_holder(holder,"Native teardown remains unconfirmed".into());
    let healthy = quiet_chat(&root);
    let mut healthy_source = source.clone();
    healthy_source.id = TerminalId::from_uuid(Uuid::from_u128(2));
    healthy_source.launch.recipe.program = "/bin/cat".into();
    healthy_source.launch.recipe.arguments.clear();
    healthy_source.owned_codex_listener = None;
    let session = terminal(&root);
    let pty = TerminalHandle::spawn(session.clone(),TerminalConfig::default(),|_| {}).unwrap();
    let deadline = Instant::now()+Duration::from_secs(5);
    let pid:u32 = loop {
        if let Ok(text) = fs::read_to_string(root.join("root-pid")) {
            if let Ok(pid) = text.parse() { break pid; }
        }
        assert!(Instant::now()<deadline,"healthy PTY did not start");
        thread::sleep(Duration::from_millis(5));
    };
    let pty_pin = ProcessPin::capture(Process::read(pid).unwrap());
    assert_eq!(fs::read_link(format!("/proc/{pid}/cwd")).unwrap(),root);
    let mut state = HostState::new(proof);
    state.register_client(1);
    state.finish_spawn(1,session,pty.clone(),true).unwrap();
    state.chats.insert(id,record(source,failed.clone()));
    state.chats.insert(healthy_source.id,record(healthy_source,healthy.clone()));
    let (handles,_) = state.graceful_shutdown();
    let host:SharedHost = Arc::new(state::HostLock::new(state));
    let error = finish_chat_shutdown(&host).unwrap_err();
    assert!(error.contains(&format!("Provider {id} shutdown was not confirmed")),"{error}");
    assert!(!failed.is_completed());
    assert!(failed_pin.live(),"refused cleanup must retain the original owner");
    assert!(healthy.is_completed(),"later healthy provider must still close");
    // Same terminal request-all/wait-all order as the actual run tail, after Err.
    for handle in &handles { handle.request_shutdown(); }
    for handle in handles { handle.shutdown().unwrap(); }
    assert!(pty.is_completed());
    pty_pin.assert_reaped();
    assert!(failed_pin.live(),"healthy teardown must not release failed custody");
    fs::remove_file(&receipt).unwrap();
    failed.close().unwrap();
    assert!(failed.is_completed());
    failed_pin.assert_reaped();
    drop(cleanup);
}
