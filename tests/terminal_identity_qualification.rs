#![cfg(all(feature = "host", target_os = "linux"))]
//! Run only with explicitly supplied, matched production products. Every
//! process and signal belongs to a private disk-backed fixture, never an
//! installed runtime. Build/run through the campaign's common resource lock.
//!
//! CAGENTS_QUALIFY_BROWSERLESS_CLI and CAGENTS_QUALIFY_BROWSERLESS_DAEMON
//! identify the old matched pair. CAGENTS_QUALIFY_FULL_DAEMON identifies the new
//! full daemon. CAGENTS_QUALIFY_EVIDENCE selects a persistent evidence directory.
//! The old CLI speaks its own protocol; the new library speaks the new one.
use cagents::{control::ControlClient, model::{LaunchRecipe, TerminalContinuity, TerminalId, TerminalSession}, terminal::TerminalSpawnOptions,
    terminal_ipc::TerminalCloseResult};
use serde_json::{Value, json};
use std::{fs, io, os::{fd::{AsRawFd, FromRawFd, OwnedFd}, unix::fs::PermissionsExt},
    path::{Path, PathBuf}, process::{Child, Command, Stdio}, thread,
    time::{Duration, Instant}};

fn process_start(pid: i32) -> io::Result<u64> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat"))?;
    stat.rsplit_once(')').and_then(|(_, tail)| tail.split_whitespace().nth(19))
        .and_then(|v| v.parse().ok()).ok_or_else(|| io::Error::other("invalid /proc identity"))
}

/// Opening a pidfd once prevents every later signal, including failure cleanup,
/// from targeting a recycled PID. Callers first bind the PID/start to evidence
/// created by this fixture or its authenticated holder record.
struct OwnedProcess { pid: i32, start: u64, fd: OwnedFd }
impl OwnedProcess {
    fn capture(pid: i32, expected_start: u64) -> Self {
        assert_eq!(process_start(pid).unwrap(), expected_start);
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) as i32 };
        assert!(fd >= 0, "pidfd_open: {}", io::Error::last_os_error());
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        assert_eq!(process_start(pid).unwrap(), expected_start);
        Self { pid, start: expected_start, fd }
    }
    fn signal(&self, signal: i32) {
        let result = unsafe { libc::syscall(libc::SYS_pidfd_send_signal, self.fd.as_raw_fd(), signal, std::ptr::null::<libc::siginfo_t>(), 0) };
        assert!(result == 0 || io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH), "fixture signal failed: {}", io::Error::last_os_error());
    }
    fn current(&self) -> bool { process_start(self.pid).is_ok_and(|start| start == self.start) }
}
struct Paused(OwnedProcess);
impl Paused {
    fn new(process: &OwnedProcess) -> Self {
        let duplicate = OwnedProcess { pid: process.pid, start: process.start, fd: process.fd.try_clone().unwrap() };
        duplicate.signal(libc::SIGSTOP); Self(duplicate)
    }
}
impl Drop for Paused { fn drop(&mut self) { self.0.signal(libc::SIGCONT); } }

struct Fixture { root: PathBuf, daemon: Option<Child>, owned: Vec<OwnedProcess> }
impl Fixture {
    fn command(&self, executable: &Path) -> Command {
        let mut command = Command::new(executable);
        command.env_clear().env("HOME", self.root.join("home"))
            .env("XDG_RUNTIME_DIR", self.root.join("run"))
            .env("XDG_DATA_HOME", self.root.join("data"))
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("XDG_CACHE_HOME", self.root.join("cache"))
            .env("TMPDIR", self.root.join("tmp"))
            .env("ACCOUNT_HOME", self.root.join("account"))
            .env("PATH", "/usr/bin:/bin").env("TERM", "xterm-256color")
            .current_dir(&self.root).stdin(Stdio::null());
        command
    }
    fn socket(&self) -> PathBuf { self.root.join("run/cagents/cagentsd.sock") }
    fn start(&mut self, executable: &Path, label: &str) {
        assert!(self.daemon.is_none());
        let log = fs::OpenOptions::new().create_new(true).write(true).open(self.root.join(format!("{label}.log"))).unwrap();
        self.daemon = Some(self.command(executable).stdout(log.try_clone().unwrap()).stderr(log).spawn().unwrap());
    }
    fn stop_daemon(&mut self) {
        if let Some(mut child) = self.daemon.take() {
            if child.try_wait().unwrap().is_none() { child.kill().unwrap(); }
            child.wait().unwrap();
        }
    }
    fn client(&mut self) -> ControlClient {
        let deadline = Instant::now() + Duration::from_secs(120);
        loop {
            if let Ok(client) = ControlClient::connect(&self.socket()) { return client; }
            assert!(self.daemon.as_mut().unwrap().try_wait().unwrap().is_none(), "fixture daemon exited; see persistent logs");
            assert!(Instant::now() < deadline, "fixture daemon did not become ready");
            thread::sleep(Duration::from_millis(50));
        }
    }
    fn await_file(&self, path: &Path) -> Value {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if let Ok(bytes) = fs::read(path) { if let Ok(value) = serde_json::from_slice(&bytes) { return value; } }
            assert!(Instant::now() < deadline, "fixture evidence missing: {}", path.display());
            thread::sleep(Duration::from_millis(25));
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.stop_daemon();
        // All descriptors were captured from exact private fixture identities.
        // Resume first so no stopped helper can outlive failed qualification.
        for process in &self.owned { process.signal(libc::SIGCONT); }
        for process in &self.owned { process.signal(libc::SIGKILL); }
    }
}
fn executable(variable: &str) -> PathBuf {
    let path = PathBuf::from(std::env::var_os(variable).unwrap_or_else(|| panic!("set {variable} to an explicit product")));
    assert!(path.is_absolute() && path.is_file());
    path
}
fn binary(path: &Path) -> Value {
    use sha2::{Digest, Sha256};
    json!({"path": path, "sha256": format!("{:x}", Sha256::digest(fs::read(path).unwrap()))})
}

#[test]
#[ignore = "requires matched old Browserless and new Full production binaries; private native lifecycle qualification"]
fn old_browserless_holder_retries_and_closes_in_full_without_replacing_pty_or_account() {
    qualify("CAGENTS_QUALIFY_BROWSERLESS_CLI", "CAGENTS_QUALIFY_BROWSERLESS_DAEMON", "CAGENTS_QUALIFY_FULL_DAEMON", "dev.cagents.Cagents.Browserless", "dev.cagents.Cagents");
}

#[test]
#[ignore = "requires matched old Full and new Browserless production binaries; private native lifecycle qualification"]
fn old_full_holder_retries_and_closes_in_browserless_without_replacing_pty_or_account() {
    qualify("CAGENTS_QUALIFY_FULL_CLI", "CAGENTS_QUALIFY_FULL_OLD_DAEMON", "CAGENTS_QUALIFY_BROWSERLESS_NEW_DAEMON", "dev.cagents.Cagents", "dev.cagents.Cagents.Browserless");
}

fn qualify(cli_variable: &str, old_variable: &str, new_variable: &str, old_application: &str, new_application: &str) {
    let cli = executable(cli_variable);
    let old = executable(old_variable);
    let new = executable(new_variable);
    let evidence = PathBuf::from(std::env::var_os("CAGENTS_QUALIFY_EVIDENCE").expect("set persistent evidence path"));
    fs::create_dir_all(&evidence).unwrap();
    let root = tempfile::Builder::new().prefix("cagents-family-").tempdir_in("/var/tmp").unwrap().keep();
    let mut fixture = Fixture { root, daemon: None, owned: Vec::new() };
    for name in ["home", "run", "data", "config", "cache", "tmp", "account", "cwd"] {
        let path = fixture.root.join(name); fs::create_dir(&path).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    }
    let receipt_path = evidence.join(format!("{}.json", fixture.root.file_name().unwrap().to_string_lossy()));
    fs::write(&receipt_path, serde_json::to_vec_pretty(&json!({"state":"started", "private_root":fixture.root,
        "old_cli":binary(&cli), "old_daemon":binary(&old), "new_daemon":binary(&new)})).unwrap()).unwrap();
    let script = fixture.root.join("terminal.py");
    fs::write(&script, r#"import json, os, signal, subprocess, sys
child = subprocess.Popen(['/bin/sleep', '600'], start_new_session=True)
root = os.getpid()
def start(pid):
    return int(open('/proc/%d/stat' % pid).read().rsplit(')',1)[1].split()[19])
def draw(*unused):
    print('\033[2J\033[Horiginal-private-pty', flush=True)
signal.signal(signal.SIGWINCH, draw)
with open(sys.argv[1], 'x') as stream:
    json.dump({'root':root,'root_start':start(root),'child':child.pid,'child_start':start(child.pid),'root_pgid':os.getpgid(root),'child_pgid':os.getpgid(child.pid),'tty':os.readlink('/proc/self/fd/0'),'account':os.environ['ACCOUNT_HOME']}, stream)
draw()
for line in sys.stdin:
    draw()
    print('ack:' + line.strip(), flush=True)
"#).unwrap();
    fixture.start(&old, "old-source");
    // The old CLI handles its own protocol and serializes only the stable
    // TerminalSession JSON. Do not deserialize an old postcard descriptor.
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let output = fixture.command(&cli).args(["--json", "daemon", "status"]).output().unwrap();
        if output.status.success() && serde_json::from_slice::<Value>(&output.stdout).is_ok_and(|value| value["data"]["running"] == true) { break; }
        assert!(Instant::now() < deadline, "old fixture daemon did not start");
        thread::sleep(Duration::from_millis(50));
    }
    let identity_path = fixture.root.join("identity.json");
    let output = fixture.command(&cli).args(["--json", "terminal", "spawn", "--cwd"])
        .arg(fixture.root.join("cwd")).arg("--").arg("/usr/bin/python3").arg(&script).arg(&identity_path).output().unwrap();
    assert!(output.status.success(), "old spawn failed: {}", String::from_utf8_lossy(&output.stderr));
    let descriptor: Value = serde_json::from_slice(&output.stdout).unwrap();
    let source: TerminalSession = serde_json::from_value(descriptor["data"]["terminal"]["session"].clone()).unwrap();
    let identity = fixture.await_file(&identity_path);
    assert_eq!(identity["account"], fixture.root.join("account").to_string_lossy().as_ref());
    let record_path = fixture.root.join("run/cagents/holders").join(format!("{}.json", source.id));
    let record = fixture.await_file(&record_path);
    assert_eq!(record["application"], old_application, "supplied old product must really have the source identity");
    assert_eq!(record["root"]["pid"], identity["root"]);
    assert_eq!(record["root"]["start"], identity["root_start"]);
    assert_ne!(identity["root_pgid"], identity["child_pgid"], "fixture background child must escape the root process group");
    for (pid, start) in [("root", "root_start"), ("child", "child_start")] {
        fixture.owned.push(OwnedProcess::capture(identity[pid].as_i64().unwrap() as i32, identity[start].as_u64().unwrap()));
    }
    fixture.owned.push(OwnedProcess::capture(record["holder"]["pid"].as_i64().unwrap() as i32, record["holder"]["start"].as_u64().unwrap()));
    // Stop only our own holder so startup cannot adopt it before the explicit
    // Retry being qualified. No product hook or registry edits are needed.
    let paused = Paused::new(fixture.owned.last().unwrap());
    fixture.stop_daemon();
    fs::remove_dir(fixture.root.join("cwd")).unwrap();
    fixture.start(&new, "new-destination");
    let mut client = fixture.client();
    assert!(!client.list().unwrap().iter().any(|terminal| terminal.session.id == source.id), "test must exercise explicit live Retry, not startup adoption");
    drop(paused);
    // Verify the actual destination application too, using its own new holder.
    // This catches mislabeled bin directories rather than trusting build names.
    let probe_recipe = LaunchRecipe { program: "/bin/sh".into(), arguments: vec!["-c".into(), "read -r fixture_value".into()],
        working_directory: fixture.root.clone(), environment: Default::default(), unset_environment: Default::default() };
    let probe = client.spawn(TerminalSession { id: TerminalId::new(), launch: probe_recipe.clone(),
        continuity: TerminalContinuity::RelaunchAvailable { recipe: probe_recipe, last_exit: None },
        transcript_path: None, resume_hint: None, owned_codex_listener: None, recovery_origin: None, predecessor: None }, TerminalSpawnOptions::default()).unwrap();
    let probe_record = fixture.await_file(&fixture.root.join("run/cagents/holders").join(format!("{}.json", probe.session.id)));
    assert_eq!(probe_record["application"], new_application, "supplied successor must really have the destination identity");
    let probe_holder = OwnedProcess::capture(probe_record["holder"]["pid"].as_i64().unwrap() as i32, probe_record["holder"]["start"].as_u64().unwrap());
    let probe_root = OwnedProcess::capture(probe_record["root"]["pid"].as_i64().unwrap() as i32, probe_record["root"]["start"].as_u64().unwrap());
    fixture.owned.push(probe_holder); fixture.owned.push(probe_root);
    assert_eq!(client.close_terminal_checked(probe.session.id, true).unwrap(), TerminalCloseResult::Closed);
    let probe_deadline = Instant::now() + Duration::from_secs(10);
    while fixture.owned[3..].iter().any(OwnedProcess::current) {
        assert!(Instant::now() < probe_deadline, "destination probe did not settle");
        thread::sleep(Duration::from_millis(25));
    }
    // Destination probe processes have settled; keep the three original
    // worker identities for all continuity assertions below.
    fixture.owned.truncate(3);
    let recovered = client.reconcile_terminal(source.clone()).unwrap();
    assert_eq!(recovered.session.launch, source.launch);
    assert_eq!(recovered.session.id, source.id);
    let retried = client.reconcile_terminal(source.clone()).unwrap();
    assert_eq!(recovered.session, retried.session, "second Retry must reuse the same worker");
    let mut forged = source.clone();
    forged.launch.environment.insert("ACCOUNT_HOME".into(), "/unissued/account".into());
    assert!(client.reconcile_terminal(forged).is_err());
    assert!(fixture.owned.iter().all(OwnedProcess::current));
    assert_eq!(unsafe { libc::getpgid(identity["root"].as_i64().unwrap() as i32) }, identity["root_pgid"].as_i64().unwrap() as i32);
    assert_eq!(unsafe { libc::getpgid(identity["child"].as_i64().unwrap() as i32) }, identity["child_pgid"].as_i64().unwrap() as i32);
    assert_eq!(fs::read_link(format!("/proc/{}/fd/0", identity["root"].as_i64().unwrap())).unwrap(), PathBuf::from(identity["tty"].as_str().unwrap()));
    let environment = fs::read(format!("/proc/{}/environ", identity["root"].as_i64().unwrap())).unwrap();
    let account_entry = format!("ACCOUNT_HOME={}", fixture.root.join("account").display());
    assert!(environment.split(|byte| *byte == 0).any(|entry| entry == account_entry.as_bytes()), "original process account environment changed");
    assert_eq!(serde_json::from_slice::<Value>(&fs::read(&record_path).unwrap()).unwrap(), record, "adoption must not rewrite discovery identity");
    client.write(source.id, b"same-pty-after-retry\n".to_vec()).unwrap();
    let screen = client.wait_for_text(source.id, "ack:same-pty-after-retry", Duration::from_secs(15)).unwrap();
    assert!(screen.text.contains("original-private-pty"));
    assert_eq!(client.reconcile_terminal_close(source.clone(), false).unwrap(), TerminalCloseResult::Active);
    assert!(fixture.owned.iter().all(OwnedProcess::current));
    assert_eq!(client.reconcile_terminal_close(source.clone(), true).unwrap(), TerminalCloseResult::Closed);
    assert_eq!(client.reconcile_terminal_close(source.clone(), true).unwrap(), TerminalCloseResult::Closed, "lost close acknowledgement must be idempotent");
    let deadline = Instant::now() + Duration::from_secs(10);
    while fixture.owned.iter().any(OwnedProcess::current) {
        assert!(Instant::now() < deadline, "original fixture process survived explicit Close");
        thread::sleep(Duration::from_millis(25));
    }
    fs::write(&receipt_path, serde_json::to_vec_pretty(&json!({"state":"passed", "private_root":fixture.root,
        "old_cli":binary(&cli), "old_daemon":binary(&old), "new_daemon":binary(&new),
        "source":source, "process_identity":identity,"holder":record,"recovered":recovered.session,"screen":screen.text,
        "contracts":["actual source and destination application namespaces", "explicit Retry after destination startup", "same PID/start/PTY/account", "deleted cwd adoption", "idempotent Retry", "wrong account refused", "live Close requires consent", "lost-ack Close receipt"]})).unwrap()).unwrap();
}
