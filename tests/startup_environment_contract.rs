#![cfg(target_os = "linux")]
//! Each executable forgets the provider session it was started inside before
//! it starts anything (#2699). A `cagents` typed in an agent's shell, or a
//! window or daemon launched from one, would otherwise hand that session's
//! messaging socket to every process it starts.
//!
//! Each case starts the real executable inside a planted parent session and
//! reads what one of its children was actually given, from that child's own
//! `/proc/PID/environ`. Removing a variable from a running process does not
//! change its own `/proc` entry, so a child is the only honest witness.

use std::{
    collections::HashMap,
    fs,
    os::unix::fs::{DirBuilderExt, PermissionsExt},
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

/// A parent session from each provider, as its shells export it.
const PARENT_SESSION: [(&str, &str); 7] = [
    ("CLAUDECODE", "1"),
    ("CLAUDE_CODE_SESSION_ID", "parent-session"),
    ("CLAUDE_CODE_MESSAGING_SOCKET", "/run/parent/messaging.sock"),
    ("CLAUDE_CODE_MESSAGING_TOKEN", "parent-token"),
    ("CODEX_THREAD_ID", "parent-thread"),
    ("CODEX_ESCALATE_SOCKET", "/run/parent/escalate.sock"),
    ("PI_TOOL_BRIDGE_SOCKET", "/run/parent/bridge.sock"),
];
/// The person's own provider configuration, which every child keeps.
const USER_CONFIGURATION: (&str, &str) = ("CLAUDE_CODE_TMPDIR", "/var/tmp/claude-scratch");

/// Private XDG roots, so no case reads or writes the person's own state.
struct Home(tempfile::TempDir);

impl Home {
    fn new() -> Self {
        let home = Self(tempfile::tempdir().unwrap());
        for directory in ["home", "config", "data", "state", "cache", "runtime"] {
            fs::DirBuilder::new()
                .mode(0o700)
                .create(home.path().join(directory))
                .unwrap();
        }
        home
    }

    fn path(&self) -> &Path {
        self.0.path()
    }

    /// Exactly the planted session, the person's configuration and the
    /// private roots: nothing the test itself runs inside reaches the case.
    fn command(&self, program: &Path) -> Command {
        for (name, _) in PARENT_SESSION {
            assert!(
                cagents::agent::is_session_variable(name),
                "{name} is no longer a provider session variable; plant a current one"
            );
        }
        assert!(!cagents::agent::is_session_variable(USER_CONFIGURATION.0));
        let mut command = Command::new(program);
        command
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", self.path().join("home"))
            .env("XDG_CONFIG_HOME", self.path().join("config"))
            .env("XDG_DATA_HOME", self.path().join("data"))
            .env("XDG_STATE_HOME", self.path().join("state"))
            .env("XDG_CACHE_HOME", self.path().join("cache"))
            .env("XDG_RUNTIME_DIR", self.path().join("runtime"))
            .envs(PARENT_SESSION)
            .env(USER_CONFIGURATION.0, USER_CONFIGURATION.1)
            .stdin(Stdio::null());
        command
    }
}

fn assert_detached(environ: &[u8]) {
    let environment: HashMap<&str, &str> = environ
        .split(|byte| *byte == 0)
        .filter(|entry| !entry.is_empty())
        .map(|entry| std::str::from_utf8(entry).unwrap().split_once('=').unwrap())
        .collect();
    for (name, _) in PARENT_SESSION {
        assert!(
            !environment.contains_key(name),
            "a child was handed the parent session's {name}: {environment:?}"
        );
    }
    assert_eq!(
        environment.get(USER_CONFIGURATION.0),
        Some(&USER_CONFIGURATION.1),
        "the person's own configuration was lost"
    );
}

/// A private installation beside the built executable: a hard link to it,
/// which keeps `current_exe` there, and a stand-in companion that records the
/// environment it was started with. A link stays on the build's filesystem
/// rather than copying an executable into the RAM-backed temporary directory.
struct Installation {
    bin: tempfile::TempDir,
    executable: PathBuf,
}

impl Installation {
    fn new(built: &str, companion: &str, then: &str) -> Self {
        let built = Path::new(built);
        let bin = tempfile::Builder::new()
            .prefix(".startup-environment-")
            .tempdir_in(built.parent().unwrap())
            .unwrap();
        let executable = bin.path().join(built.file_name().unwrap());
        fs::hard_link(built, &executable).unwrap();
        let companion = bin.path().join(companion);
        fs::write(
            &companion,
            format!(
                "#!/bin/sh\n\
                 cat /proc/$$/environ > \"$0.environ.next\" && mv \"$0.environ.next\" \"$0.environ\"\n\
                 {then}\n"
            ),
        )
        .unwrap();
        fs::set_permissions(&companion, fs::Permissions::from_mode(0o755)).unwrap();
        Self { bin, executable }
    }

    fn recorded(&self, companion: &str) -> PathBuf {
        self.bin.path().join(format!("{companion}.environ"))
    }
}

#[test]
fn the_command_line_never_hands_a_parent_session_to_the_daemon() {
    let home = Home::new();
    // `daemon store-schema` runs its matched daemon directly and synchronously.
    let installation = Installation::new(env!("CARGO_BIN_EXE_cagents"), "cagentsd", "exit 0");
    let output = home
        .command(&installation.executable)
        .args(["daemon", "store-schema"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_detached(&fs::read(installation.recorded("cagentsd")).unwrap());
}

#[cfg(feature = "ui")]
#[test]
fn a_window_never_hands_a_parent_session_to_the_daemon_it_starts() {
    use std::{
        process::Child,
        time::{Duration, Instant},
    };
    const DEADLINE: Duration = Duration::from_secs(30);
    const POLL: Duration = Duration::from_millis(20);
    fn wait_for(path: &Path, owner: &mut Child) -> Vec<u8> {
        let deadline = Instant::now() + DEADLINE;
        loop {
            if let Ok(bytes) = fs::read(path) {
                return bytes;
            }
            if let Some(status) = owner.try_wait().unwrap() {
                // A child can publish just as its owner exits.
                if let Ok(bytes) = fs::read(path) {
                    return bytes;
                }
                panic!("the executable exited with {status} before starting a child");
            }
            assert!(
                Instant::now() < deadline,
                "the executable started no child within {DEADLINE:?}"
            );
            std::thread::sleep(POLL);
        }
    }
    struct Window(Child);
    impl Drop for Window {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let home = Home::new();
    // The stand-in daemon stays until the record is collected, so the window
    // keeps waiting for it instead of reporting a failed start.
    let installation = Installation::new(
        env!("CARGO_BIN_EXE_cagents-ui"),
        "cagentsd",
        "for _ in $(seq 600); do [ -e \"$0.environ\" ] || exit 0; sleep 0.05; done",
    );
    let mut window = Window(
        home.command(&installation.executable)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let recorded = installation.recorded("cagentsd");
    let environ = wait_for(&recorded, &mut window.0);
    drop(window);
    fs::remove_file(recorded).unwrap();
    assert_detached(&environ);
}

#[cfg(feature = "host")]
#[test]
fn the_daemon_never_hands_a_parent_session_to_an_automation_precheck() {
    use std::os::unix::{
        fs::{MetadataExt, OpenOptionsExt},
        process::CommandExt,
    };
    let home = Home::new();
    // The supervisor is `cagentsd` itself, past the same startup as a serving
    // daemon; the precheck is the person's shell command it runs.
    let firing = uuid::Uuid::new_v4();
    let directory = home.path().join(firing.to_string());
    fs::DirBuilder::new()
        .mode(0o700)
        .create(&directory)
        .unwrap();
    let identity = fs::metadata(&directory).unwrap();
    let request = serde_json::json!({
        "firing": firing, "revision": 1, "directory": directory,
        "target_identity": {"device": identity.dev(), "inode": identity.ino()},
        "precheck": {"command": "cat /proc/$$/environ > environ", "timeout_seconds": 5}
    });
    fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(directory.join("request"))
        .and_then(|mut file| std::io::Write::write_all(&mut file, request.to_string().as_bytes()))
        .unwrap();
    let lease = fs::OpenOptions::new()
        .create_new(true)
        .read(true)
        .write(true)
        .mode(0o600)
        .open(directory.join("lease"))
        .unwrap();
    let fd = std::os::fd::AsRawFd::as_raw_fd(&lease);
    let mut command = home.command(Path::new(env!("CARGO_BIN_EXE_cagentsd")));
    command
        .arg("--automation-precheck-supervisor")
        .arg(&directory)
        .arg(fd.to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    // SAFETY: only an async-signal-safe fcntl between fork and exec, handing
    // the supervisor the lease it is required to own.
    unsafe {
        command.pre_exec(move || {
            if libc::fcntl(fd, libc::F_SETFD, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let output = command.output().unwrap();
    drop(lease);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let receipt = fs::read_to_string(directory.join("receipt")).unwrap();
    assert!(receipt.contains("Passed"), "{receipt}");
    assert_detached(&fs::read(directory.join("environ")).unwrap());
}
