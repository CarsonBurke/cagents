#![cfg(all(feature = "host", target_os = "linux"))]

//! Private-daemon preference persistence and real CLI authority behavior, no providers.
//!
//! With inherited delegation the real CLI must refuse this unrelated daemon.
//! A distinct explicit fixture User protocol client then tests persistence. Real
//! CLI User success/output is exercised only with a genuinely nondelegated caller;
//! the delegated lane never claims those CLI writes ran or substitutes authority.
use cagents::{
    account::{
        preferences::DefaultSelection,
        registry::{Registry, Store},
    },
    agent::Provider,
    agent_execution::Authority,
    control::ControlClient,
    device_service::{Operation, Reply},
    process::{
        CancellationToken, CommandSpec, ProcessExecutor, ProcessOutput, SystemProcessExecutor,
    },
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
fn private_default_preferences_persist_and_cli_respects_inherited_authority() {
    // Public equality selects the expected contract before any operation. Never
    // catch an authentication failure and retry it with a different authority.
    let delegated = Authority::from_environment() != Authority::default();
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
        // This client is an explicit synthetic fixture User, not the delegated
        // CLI caller. No environment variable or issued capability is changed.
        observer
            .authenticate(Authority::default())
            .map_err(|error| error.to_string())?;
        let original_proof = observer.proof().clone();
        let before = registry(&mut observer)?;
        let store_path = root.join("config").join(cagents::instance::STORAGE_NAME);
        let store = Store::at(store_path.clone());
        let disk_bytes = || -> Result<Option<Vec<u8>>, String> {
            match fs::read(store_path.join("accounts.toml")) {
                Ok(bytes) => Ok(Some(bytes)),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(error) => Err(error.to_string()),
            }
        };
        let cli = |choice: &str| -> Result<ProcessOutput, String> {
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
            SystemProcessExecutor
                .execute(&spec, &CancellationToken::new())
                .map_err(|error| error.to_string())
        };
        if delegated {
            let original_disk_bytes = disk_bytes()?;
            for choice in ["claude/system", "automatic"] {
                let output = cli(choice)?;
                if output.success
                    || output.exit_code != Some(1)
                    || output.signal.is_some()
                    || !output.stdout.is_empty()
                {
                    return Err("delegated CLI did not exit with its expected refusal".into());
                }
                let error: serde_json::Value = serde_json::from_slice(&output.stderr)
                    .map_err(|_| "delegated CLI did not return structured error JSON")?;
                let expected_message = "daemon rejected the request (PermissionDenied): This agent session authority is no longer registered";
                if error["ok"] != false
                    || error["error"]["kind"] != "protocol"
                    || error["error"]["details"]
                        != serde_json::json!({"code":"permission_denied", "retryable":false})
                    || error["error"]["message"] != expected_message
                {
                    return Err(
                        "delegated CLI did not report the precise foreign-authority denial".into(),
                    );
                }
                // Check after EACH refused invocation, before any fixture User
                // write. Matching logical state alone would miss a disk rewrite.
                if registry(&mut observer)? != before
                    || store.load().map_err(|error| error.to_string())? != before
                    || disk_bytes()? != original_disk_bytes
                    || observer.proof() != &original_proof
                {
                    return Err(
                        "delegated CLI refusal changed the private registry or proof".into(),
                    );
                }
            }
            eprintln!(
                "coverage: both inherited CLI writes denied unchanged; real CLI User success UNEXECUTED; explicit fixture User RPC persistence follows"
            );
        } else {
            eprintln!(
                "coverage lane: genuinely nondelegated real CLI User writes and output checks"
            );
        }
        // A separate fixture User connection is used only in the declared RPC
        // lane. It never authenticates the CLI or registers its inherited token.
        let mut fixture_user = if delegated {
            let mut client = ControlClient::connect(&socket).map_err(|error| error.to_string())?;
            client
                .authenticate(Authority::default())
                .map_err(|error| error.to_string())?;
            if client.proof() != &original_proof {
                return Err("fixture User connected to a different private daemon".into());
            }
            Some(client)
        } else {
            None
        };
        let mut cli_replies = Vec::new();
        let mut saved = Vec::new();
        let mut expected_revision = before.revision();
        for choice in ["claude/system", "automatic"] {
            let rpc_saved = if let Some(client) = fixture_user.as_mut() {
                let selection: DefaultSelection = choice.parse()?;
                let Reply::AccountRegistry(value) = client
                    .device_operation(Operation::AccountDefault {
                        expected_revision,
                        provider: Provider::Claude,
                        selection,
                    })
                    .map_err(|error| error.to_string())?
                else {
                    return Err("unexpected fixture User preference response".into());
                };
                Some(serde_json::from_value::<Registry>(value).map_err(|error| error.to_string())?)
            } else {
                let output = cli(choice)?
                    .require_success(Path::new(env!("CARGO_BIN_EXE_cagents")))
                    .map_err(|error| error.to_string())?;
                cli_replies.push(
                    serde_json::from_slice::<serde_json::Value>(&output.stdout)
                        .map_err(|error| error.to_string())?,
                );
                None
            };
            let observed = registry(&mut observer)?;
            let disk = store.load().map_err(|error| error.to_string())?;
            if expected_revision.checked_add(1) != Some(observed.revision())
                || observed != disk
                || rpc_saved.as_ref().is_some_and(|reply| reply != &observed)
                || observer.proof() != &original_proof
                || fixture_user
                    .as_ref()
                    .is_some_and(|client| client.proof() != &original_proof)
            {
                return Err("private preference write lost its exact revision, persistence or original proof".into());
            }
            expected_revision = observed.revision();
            saved.push((observed, disk));
        }
        let (automatic, automatic_disk) = saved.pop().unwrap();
        let (explicit, explicit_disk) = saved.pop().unwrap();
        let proof_unchanged = observer.proof() == &original_proof;
        Ok((
            before,
            cli_replies,
            explicit,
            explicit_disk,
            automatic,
            automatic_disk,
            proof_unchanged,
        ))
    })();
    // Close the original observer and reap the original daemon before asserting.
    // The outer queue guardian separately verifies full descendant settlement.
    drop(daemon);
    let (before, cli_replies, explicit, explicit_disk, automatic, automatic_disk, proof_unchanged) =
        observations.unwrap();
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
    assert_eq!(cli_replies.len(), if delegated { 0 } else { 2 });
    // Only actual successful CLI stdout enters this vector; RPC replies are
    // checked as Registry values above and never dressed up as CLI JSON.
    for (reply, (choice, revision)) in cli_replies.into_iter().zip([
        ("claude/system", explicit.revision()),
        ("automatic", automatic.revision()),
    ]) {
        assert_eq!(reply["ok"], true);
        assert_eq!(reply["command"], "account.default");
        assert_eq!(
            reply["data"],
            serde_json::json!({"provider":"claude", "selection":choice, "revision":revision})
        );
    }
}
