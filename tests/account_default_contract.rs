#![cfg(all(feature = "host", target_os = "linux"))]

//! Real CLI preference writes against one private daemon/profile, no providers.
use cagents::{
    account::registry::{Registry, Store},
    agent::Provider,
    agent_execution::Authority,
    control::ControlClient,
    device_service::{Operation, Reply},
    process::{CancellationToken, CommandSpec, ProcessExecutor, SystemProcessExecutor},
};
use std::{
    fs,
    os::unix::fs::DirBuilderExt,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

struct PrivateDaemon(Child);
impl Drop for PrivateDaemon {
    fn drop(&mut self) {
        // An unreaped original child reserves its PID through this signal.
        if self.0.try_wait().ok().flatten().is_none() {
            unsafe { libc::kill(self.0.id() as libc::pid_t, libc::SIGTERM) };
        }
        let deadline = Instant::now() + Duration::from_secs(3);
        while self.0.try_wait().ok().flatten().is_none() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn private_environment(root: &Path) -> Vec<(&'static str, PathBuf)> {
    [
        ("HOME", "home"),
        ("XDG_RUNTIME_DIR", "runtime"),
        ("XDG_CONFIG_HOME", "config"),
        ("XDG_DATA_HOME", "data"),
        ("XDG_STATE_HOME", "state"),
        ("XDG_CACHE_HOME", "cache"),
        ("CLAUDE_CONFIG_DIR", "claude"),
        ("CODEX_HOME", "codex"),
    ]
    .into_iter()
    .map(|(name, relative)| (name, root.join(relative)))
    .collect()
}

fn registry(client: &mut ControlClient) -> Result<Registry, String> {
    let Reply::Accounts(value) = client
        .device_operation(Operation::AccountsRead)
        .map_err(|error| error.to_string())?
    else {
        return Err("unexpected account snapshot".into());
    };
    serde_json::from_value(value["registry"].clone()).map_err(|error| error.to_string())
}

#[test]
fn cli_default_writes_are_persisted_and_visible_to_the_original_private_observer() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let environment = private_environment(root);
    for (_, path) in &environment {
        fs::DirBuilder::new().mode(0o700).create(path).unwrap();
    }
    let mut command = Command::new(env!("CARGO_BIN_EXE_cagentsd"));
    for (name, path) in &environment {
        command.env(name, path);
    }
    // Inherited delegation stays unchanged; private HOME/provider homes keep
    // every account observation and preference write inside this fixture.
    let mut daemon = PrivateDaemon(
        command
            .env("PATH", "/usr/bin:/bin")
            .env("SHELL", "/bin/sh")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(fs::File::create(root.join("daemon.log")).unwrap())
            .spawn()
            .unwrap(),
    );
    let socket = root
        .join("runtime")
        .join(cagents::instance::RUNTIME_SUBDIRECTORY)
        .join(cagents::terminal_ipc::SOCKET_FILE_NAME);
    let deadline = Instant::now() + Duration::from_secs(20);
    let observations = (|| -> Result<_, String> {
        let mut observer = loop {
            if let Ok(client) = ControlClient::connect(&socket) {
                break client;
            }
            if daemon
                .0
                .try_wait()
                .map_err(|error| error.to_string())?
                .is_some()
            {
                return Err("private daemon exited before readiness".into());
            }
            if Instant::now() >= deadline {
                return Err("private daemon readiness deadline expired".into());
            }
            thread::sleep(Duration::from_millis(10));
        };
        observer
            .authenticate(Authority::from_environment())
            .map_err(|error| error.to_string())?;
        let original_proof = observer.proof().clone();
        let before = registry(&mut observer)?;
        let cli = |choice: &str| -> Result<serde_json::Value, String> {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err("private CLI deadline expired".into());
            }
            let executable = Path::new(env!("CARGO_BIN_EXE_cagents"));
            let mut spec = CommandSpec::new(executable)
                .args([
                    "--device",
                    "local",
                    "--no-start",
                    "--json",
                    "account",
                    "default",
                    "claude",
                    choice,
                ])
                .env("PATH", "/usr/bin:/bin")
                .env("SHELL", "/bin/sh")
                .timeout(remaining.min(Duration::from_secs(5)))
                .output_limits(64 * 1024, 64 * 1024);
            for (name, path) in &environment {
                spec = spec.env(*name, path);
            }
            let output = SystemProcessExecutor
                .execute(&spec, &CancellationToken::new())
                .and_then(|output| output.require_success(executable))
                .map_err(|error| error.to_string())?;
            serde_json::from_slice(&output.stdout).map_err(|error| error.to_string())
        };
        let explicit_reply = cli("claude/system")?;
        let explicit = registry(&mut observer)?;
        let store = Store::at(root.join("config").join(cagents::instance::STORAGE_NAME));
        let explicit_disk = store.load().map_err(|error| error.to_string())?;
        let automatic_reply = cli("automatic")?;
        let automatic = registry(&mut observer)?;
        let automatic_disk = store.load().map_err(|error| error.to_string())?;
        let proof_unchanged = observer.proof() == &original_proof;
        Ok((
            before,
            explicit_reply,
            explicit,
            explicit_disk,
            automatic_reply,
            automatic,
            automatic_disk,
            proof_unchanged,
        ))
    })();
    // Close the original observer and reap the original daemon before asserting.
    // The outer queue guardian separately verifies full descendant settlement.
    drop(daemon);
    let (
        before,
        explicit_reply,
        explicit,
        explicit_disk,
        automatic_reply,
        automatic,
        automatic_disk,
        proof_unchanged,
    ) = observations.unwrap();
    assert!(before.automatic(Provider::Claude));
    assert!(!explicit.automatic(Provider::Claude));
    assert_eq!(
        explicit
            .default_account(Provider::Claude, None)
            .id()
            .as_str(),
        "claude/system"
    );
    assert!(explicit.revision() > before.revision());
    assert_eq!(explicit, explicit_disk);
    assert!(automatic.automatic(Provider::Claude));
    assert!(automatic.revision() > explicit.revision());
    assert_eq!(automatic, automatic_disk);
    assert!(automatic.registered().is_empty());
    assert!(proof_unchanged);
    for (reply, choice, revision) in [
        (explicit_reply, "claude/system", explicit.revision()),
        (automatic_reply, "automatic", automatic.revision()),
    ] {
        assert_eq!(reply["command"], "account.default");
        assert_eq!(
            reply["data"],
            serde_json::json!({"provider":"claude", "selection":choice, "revision":revision})
        );
    }
}
