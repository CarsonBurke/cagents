#![cfg(target_os = "linux")]
//! CLI-only behavior: reads can refuse startup even when a launchable sibling exists.
#[path = "support/user_fixture.rs"]
mod user_fixture;
use cagents::process::{
    CancellationToken, CommandSpec, ProcessExecutor, ProcessOutput, SystemProcessExecutor,
};
use serde_json::Value;
use std::{
    ffi::OsStr,
    fs,
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    sync::{Mutex, MutexGuard},
    time::Duration,
};

// A concurrent child spawn can inherit another fixture's still-writable
// executable copy until exec closes it. Keep creation and child execution in
// one fixture lane so private binaries never race that writable descriptor.
static FIXTURE_EXECUTION: Mutex<()> = Mutex::new(());

struct Fixture {
    root: tempfile::TempDir,
    cli: PathBuf,
    _execution: MutexGuard<'static, ()>,
}
impl Fixture {
    fn new() -> Self {
        let execution = FIXTURE_EXECUTION
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let root = tempfile::tempdir().unwrap();
        for path in ["bin", "home", "run", "data", "config", "cache"] {
            fs::create_dir(root.path().join(path)).unwrap();
        }
        fs::set_permissions(root.path().join("run"), fs::Permissions::from_mode(0o700)).unwrap();
        let cli = root.path().join("bin/cagents");
        if fs::hard_link(env!("CARGO_BIN_EXE_cagents"), &cli).is_err() {
            fs::copy(env!("CARGO_BIN_EXE_cagents"), &cli).unwrap();
        }
        let daemon = root.path().join("bin/cagentsd");
        fs::write(
            &daemon,
            "#!/bin/sh\nprintf invoked > \"$HOME/../spawned\"\nexit 72\n",
        )
        .unwrap();
        fs::set_permissions(daemon, fs::Permissions::from_mode(0o700)).unwrap();
        Self {
            root,
            cli,
            _execution: execution,
        }
    }
    fn command(&self, args: &[&str]) -> CommandSpec {
        let mut command = CommandSpec::new(&self.cli);
        // Keep delegation authority inherited unchanged. All other child state
        // is private to this fixture, including the only possible SSH target.
        for (name, _) in std::env::vars_os() {
            if name != OsStr::new("CAGENTS_AGENT_AUTHORITY") {
                command = command.env_remove(name);
            }
        }
        command
            .env("HOME", self.root.path().join("home"))
            .env("XDG_RUNTIME_DIR", self.root.path().join("run"))
            .env("XDG_DATA_HOME", self.root.path().join("data"))
            .env("XDG_CONFIG_HOME", self.root.path().join("config"))
            .env("XDG_CACHE_HOME", self.root.path().join("cache"))
            .env("PATH", "/usr/bin:/bin")
            .cwd(self.root.path())
            .timeout(Duration::from_secs(15))
            .output_limits(64 * 1024, 64 * 1024)
            .arg("--json")
            .args(args.iter().copied())
    }
    fn run(&self, command: CommandSpec) -> ProcessOutput {
        SystemProcessExecutor
            .execute(&command, &CancellationToken::new())
            .unwrap()
    }
    fn output(&self, args: &[&str], environment_policy: bool) -> ProcessOutput {
        let mut command = self.command(args);
        if environment_policy {
            command = command.env("CAGENTS_NO_START", "1");
        }
        self.run(command)
    }
}

#[test]
fn every_read_can_skip_daemon_start_through_flag_or_environment() {
    if !user_fixture::enter() {
        return;
    }
    let fixture = Fixture::new();
    for environment in [false, true] {
        for args in [
            vec!["agent", "list"],
            vec!["automation", "list"],
            vec!["channel", "list"],
            vec!["terminal", "list"],
            vec!["project", "list"],
            vec!["sessions"],
            vec!["whoami"],
            vec!["account", "info"],
            vec!["daemon", "start"],
        ] {
            let mut arguments = args;
            if !environment {
                arguments.push("--no-start");
            }
            let output = fixture.output(&arguments, environment);
            assert_eq!(output.exit_code, Some(1), "{arguments:?}: {output:?}");
            let value: Value = serde_json::from_slice(&output.stderr).unwrap();
            assert_eq!(value["error"]["kind"], "daemon_not_running", "{value}");
            assert!(
                !fixture.root.path().join("spawned").exists(),
                "daemon was spawned: {arguments:?}"
            );
        }
    }
}

#[test]
fn stopping_an_absent_daemon_twice_succeeds_and_has_a_stable_shape() {
    if !user_fixture::enter() {
        return;
    }
    let fixture = Fixture::new();
    for _ in 0..2 {
        let output = fixture.output(&["daemon", "stop"], false);
        assert!(output.success, "{output:?}");
        let value: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(
            value["data"],
            serde_json::json!({"state":"already_stopped"})
        );
        assert!(!fixture.root.path().join("spawned").exists());
    }
}

fn configure_remote_fixture(fixture: &Fixture, failing_ssh: bool) {
    let config = fixture
        .root
        .path()
        .join("config")
        .join(cagents::instance::STORAGE_NAME);
    fs::create_dir_all(&config).unwrap();
    fs::write(config.join("devices.json"),serde_json::to_vec(&serde_json::json!({"devices":[{"name":"fixture","host":"fixture-host","executable":fixture.cli}]})).unwrap()).unwrap();
    let ssh = fixture.root.path().join("bin/ssh");
    fs::write(&ssh,if failing_ssh {"#!/bin/sh\nprintf 'genuine ssh failure\\n' >&2\nexit 255\n"} else {"#!/bin/sh\nset -eu\nlast=''\nfor arg do last=$arg; done\nprintf '%s\\n' \"$last\" >> \"$HOME/../ssh-calls\"\nexec /bin/sh -c \"$last\"\n"}).unwrap();
    fs::set_permissions(ssh, fs::Permissions::from_mode(0o700)).unwrap();
}

#[test]
fn protocol_version_is_local_before_device_selection_or_service_preparation() {
    if !user_fixture::enter() {
        return;
    }
    let fixture = Fixture::new();
    configure_remote_fixture(&fixture, false);
    let path = format!(
        "{}:/usr/bin:/bin",
        fixture.root.path().join("bin").display()
    );
    for device in ["fixture", "unknown-device"] {
        for policy in ["none", "flag", "environment"] {
            let mut args = vec!["--device", device, "daemon", "protocol-version"];
            if policy == "flag" {
                args.push("--no-start");
            }
            let mut command = fixture.command(&args).env("PATH", &path);
            if policy == "environment" {
                command = command.env("CAGENTS_NO_START", "1");
            }
            let output = fixture.run(command);
            assert!(output.success, "{device}/{policy}: {output:?}");
            let value: Value = serde_json::from_slice(&output.stdout).unwrap();
            assert_eq!(value["command"], "daemon.protocol_version");
            assert_eq!(
                value["data"]["protocol_version"],
                cagents::terminal::ipc::PROTOCOL_VERSION
            );
            assert!(output.stderr.is_empty());
            assert!(!fixture.root.path().join("spawned").exists());
            assert!(!fixture.root.path().join("ssh-calls").exists());
        }
    }
}

#[test]
fn local_lifecycle_refuses_remote_selection_before_local_or_remote_effects() {
    if !user_fixture::enter() {
        return;
    }
    let fixture = Fixture::new();
    configure_remote_fixture(&fixture, false);
    let path = format!(
        "{}:/usr/bin:/bin",
        fixture.root.path().join("bin").display()
    );
    for device in ["fixture", "unknown-device"] {
        for command in [
            vec!["daemon", "upgrade", "--yes"],
            vec!["daemon", "abandon-upgrade", "--yes"],
            vec!["daemon", "stop-for-rebuild", "--including-subagents"],
            vec!["daemon", "bridge"],
            vec![
                "coordinate-install",
                "--prefix",
                "/missing-prefix",
                "--installer",
                "/missing-installer",
            ],
            vec!["restart-windows", "--instance", "missing-instance"],
        ] {
            let mut args = vec!["--device", device];
            args.extend(command);
            let output = fixture.run(fixture.command(&args).env("PATH", &path));
            assert_eq!(output.exit_code, Some(1), "{args:?}: {output:?}");
            let value: Value = serde_json::from_slice(&output.stderr).unwrap();
            assert_eq!(
                value["error"]["kind"], "daemon_local_only",
                "{args:?}: {value}"
            );
            assert!(output.stdout.is_empty());
            assert!(!fixture.root.path().join("spawned").exists());
            assert!(!fixture.root.path().join("ssh-calls").exists());
        }
    }
    assert_eq!(
        fs::read_dir(fixture.root.path().join("run"))
            .unwrap()
            .count(),
        0
    );
}

#[test]
fn remote_stop_is_idempotent_and_no_start_reads_report_remote_absence() {
    if !user_fixture::enter() {
        return;
    }
    let fixture = Fixture::new();
    configure_remote_fixture(&fixture, false);
    let path = format!(
        "{}:/usr/bin:/bin",
        fixture.root.path().join("bin").display()
    );
    for _ in 0..2 {
        let output = fixture.run(
            fixture
                .command(&["--device", "fixture", "daemon", "stop"])
                .env("PATH", &path),
        );
        assert!(output.success, "{output:?}");
        let value: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(
            value["data"],
            serde_json::json!({"state":"already_stopped"})
        );
    }
    for environment in [false, true] {
        let mut command = fixture.command(&["--device", "fixture", "agent", "list"]);
        if environment {
            command = command.env("CAGENTS_NO_START", "1");
        } else {
            command = command.arg("--no-start");
        }
        let output = fixture.run(command.env("PATH", &path));
        assert_eq!(output.exit_code, Some(1), "{output:?}");
        let error: Value = serde_json::from_slice(&output.stderr).unwrap();
        assert_eq!(error["error"]["kind"], "daemon_not_running");
    }
    assert!(!fixture.root.path().join("spawned").exists());
    let calls = fs::read_to_string(fixture.root.path().join("ssh-calls")).unwrap();
    assert!(
        calls
            .lines()
            .all(|line| line.contains(" --no-start daemon bridge")),
        "{calls}"
    );
}

#[test]
fn a_genuine_ssh_failure_is_not_an_already_stopped_remote_daemon() {
    if !user_fixture::enter() {
        return;
    }
    let fixture = Fixture::new();
    configure_remote_fixture(&fixture, true);
    let path = format!(
        "{}:/usr/bin:/bin",
        fixture.root.path().join("bin").display()
    );
    let output = fixture.run(
        fixture
            .command(&["--device", "fixture", "daemon", "stop"])
            .env("PATH", path),
    );
    assert_eq!(output.exit_code, Some(1), "{output:?}");
    assert!(output.stdout.is_empty());
    let error: Value = serde_json::from_slice(&output.stderr).unwrap();
    assert_eq!(error["error"]["kind"], "device");
    assert!(!fixture.root.path().join("spawned").exists());
}

#[test]
fn unresolved_runtime_is_an_error_rather_than_an_already_stopped_daemon() {
    if !user_fixture::enter() {
        return;
    }
    let fixture = Fixture::new();
    for runtime in [None, Some("relative-runtime")] {
        let mut command = fixture
            .command(&["daemon", "stop"])
            .env_remove("XDG_RUNTIME_DIR");
        // CommandSpec removes inherited variables before applying its explicit
        // environment map. Remove the fixture override as well for this case.
        command.environment.remove(OsStr::new("XDG_RUNTIME_DIR"));
        if let Some(runtime) = runtime {
            command = command.env("XDG_RUNTIME_DIR", runtime);
        }
        let output = fixture.run(command);
        assert_eq!(output.exit_code, Some(1), "{output:?}");
        assert!(output.stdout.is_empty());
        let error: Value = serde_json::from_slice(&output.stderr).unwrap();
        assert_eq!(error["error"]["kind"], "socket_path", "{error}");
        assert!(!fixture.root.path().join("spawned").exists());
    }
}

#[test]
fn a_closed_listener_with_a_held_inventory_lease_is_not_daemon_absence() {
    if !user_fixture::enter() {
        return;
    }
    use std::os::{fd::AsRawFd, unix::fs::OpenOptionsExt};
    let fixture = Fixture::new();
    let runtime = fixture
        .root
        .path()
        .join("run")
        .join(cagents::instance::RUNTIME_SUBDIRECTORY);
    fs::create_dir(&runtime).unwrap();
    fs::set_permissions(&runtime, fs::Permissions::from_mode(0o700)).unwrap();
    let lock = fs::OpenOptions::new()
        .create_new(true)
        .read(true)
        .write(true)
        .mode(0o600)
        .open(runtime.join("cagentsd.lock"))
        .unwrap();
    // SAFETY: lock is an owned regular private fixture descriptor.
    assert_eq!(
        unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0
    );
    for arguments in [["daemon", "stop"], ["agent", "list"], ["daemon", "start"]] {
        let output = fixture.output(&arguments, true);
        assert_eq!(output.exit_code, Some(1), "{output:?}");
        let error: Value = serde_json::from_slice(&output.stderr).unwrap();
        assert_eq!(error["error"]["kind"], "daemon_busy", "{error}");
    }
    configure_remote_fixture(&fixture, false);
    let path = format!(
        "{}:/usr/bin:/bin",
        fixture.root.path().join("bin").display()
    );
    let output = fixture.run(
        fixture
            .command(&["--device", "fixture", "daemon", "stop"])
            .env("PATH", path),
    );
    assert_eq!(output.exit_code, Some(1), "{output:?}");
    let error: Value = serde_json::from_slice(&output.stderr).unwrap();
    assert_eq!(error["error"]["kind"], "device", "{error}");
    assert!(!fixture.root.path().join("spawned").exists());
}

#[test]
fn no_start_daemon_start_still_requires_a_protocol_handshake() {
    if !user_fixture::enter() {
        return;
    }
    use std::{
        os::unix::net::UnixListener,
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        thread,
        time::Instant,
    };
    let fixture = Fixture::new();
    let runtime = fixture
        .root
        .path()
        .join("run")
        .join(cagents::instance::RUNTIME_SUBDIRECTORY);
    fs::create_dir(&runtime).unwrap();
    let listener =
        UnixListener::bind(runtime.join(cagents::terminal_ipc::SOCKET_FILE_NAME)).unwrap();
    listener.set_nonblocking(true).unwrap();
    let finished = Arc::new(AtomicBool::new(false));
    let peer_finished = finished.clone();
    let worker = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut accepted = 0;
        while !peer_finished.load(Ordering::Relaxed) && Instant::now() < deadline {
            match listener.accept() {
                Ok((stream, _)) => {
                    accepted += 1;
                    drop(stream);
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(5))
                }
                Err(error) => panic!("{error}"),
            }
        }
        accepted
    });
    let output = fixture.output(&["daemon", "start", "--no-start"], false);
    finished.store(true, Ordering::Relaxed);
    let accepted = worker.join().unwrap();
    assert!(
        accepted > 0,
        "the private listener must actually receive a connection"
    );
    assert_eq!(output.exit_code, Some(1), "{output:?}");
    assert!(output.stdout.is_empty());
    let error: Value = serde_json::from_slice(&output.stderr).unwrap();
    assert_eq!(error["error"]["kind"], "connection", "{error}");
    assert!(!fixture.root.path().join("spawned").exists());
}
