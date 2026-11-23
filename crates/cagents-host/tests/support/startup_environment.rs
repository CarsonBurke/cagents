//! Each case of the startup environment contracts starts a real executable
//! inside a planted parent session. The library's executables are checked in
//! `tests/startup_environment_contract.rs`, the window in the UI crate's.

use std::{
    collections::HashMap,
    fs,
    os::unix::fs::{DirBuilderExt, PermissionsExt},
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

/// A parent session from each provider, as its shells export it.
pub const PARENT_SESSION: [(&str, &str); 7] = [
    ("CLAUDECODE", "1"),
    ("CLAUDE_CODE_SESSION_ID", "parent-session"),
    ("CLAUDE_CODE_MESSAGING_SOCKET", "/run/parent/messaging.sock"),
    ("CLAUDE_CODE_MESSAGING_TOKEN", "parent-token"),
    ("CODEX_THREAD_ID", "parent-thread"),
    ("CODEX_ESCALATE_SOCKET", "/run/parent/escalate.sock"),
    ("PI_TOOL_BRIDGE_SOCKET", "/run/parent/bridge.sock"),
];
/// The person's own provider configuration, which every child keeps.
pub const USER_CONFIGURATION: (&str, &str) = ("CLAUDE_CODE_TMPDIR", "/var/tmp/claude-scratch");

/// Private XDG roots, so no case reads or writes the person's own state.
pub struct Home(tempfile::TempDir);

impl Home {
    pub fn new() -> Self {
        let home = Self(tempfile::tempdir().unwrap());
        for directory in ["home", "config", "data", "state", "cache", "runtime"] {
            fs::DirBuilder::new()
                .mode(0o700)
                .create(home.path().join(directory))
                .unwrap();
        }
        home
    }

    pub fn path(&self) -> &Path {
        self.0.path()
    }

    /// Exactly the planted session, the person's configuration and the
    /// private roots: nothing the test itself runs inside reaches the case.
    pub fn command(&self, program: &Path) -> Command {
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

pub fn assert_detached(environ: &[u8]) {
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
pub struct Installation {
    bin: tempfile::TempDir,
    executable: PathBuf,
}

impl Installation {
    pub fn new(built: &str, companion: &str, then: &str) -> Self {
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

    /// The linked executable, which runs from this installation.
    pub fn executable(&self) -> &Path {
        &self.executable
    }

    pub fn recorded(&self, companion: &str) -> PathBuf {
        self.bin.path().join(format!("{companion}.environ"))
    }
}
