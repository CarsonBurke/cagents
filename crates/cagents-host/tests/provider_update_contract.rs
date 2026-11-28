#![cfg(all(
    target_os = "linux",
    target_env = "gnu",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]

#[path = "support/cli.rs"]
mod cli;

use cagents::{
    account::{
        cache::Cache,
        probe::login_path::ADOPTED_ENV,
        provider_install::{ProviderInstallMethod, ProviderStatus, ProviderUpdateResult},
    },
    agent::Provider,
    agent_execution::Authority,
    control::ControlClient,
    instance::{RUNTIME_SUBDIRECTORY, STORAGE_NAME},
    process::{CancellationToken, CommandSpec, ProcessExecutor, SystemProcessExecutor},
    terminal_ipc::{SOCKET_FILE_NAME, StartupState},
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    ffi::OsString,
    fs,
    io::Write,
    os::unix::fs::{DirBuilderExt, PermissionsExt},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

const CURL: &str = r#"#!/bin/sh
set -eu
while [ "$#" -gt 0 ]; do
    case "$1" in
        --output) output=$2; shift ;;
        https://downloads.claude.ai/claude-code-releases/*)
            request=${1#https://downloads.claude.ai/claude-code-releases/} ;;
    esac
    shift
done
releases="$HOME/../releases"
case "$request" in
    latest|stable|*/manifest.json|*/manifest.json.sig) ;;
    2.0.0/linux-*/claude)
        : > "$releases/downloading"
        attempts=0
        while [ ! -f "$releases/proceed" ] && [ "$attempts" -lt 400 ]; do
            /bin/sleep 0.05
            attempts=$((attempts + 1))
        done
        test -f "$releases/proceed"
        request=2.0.0/claude ;;
    */linux-*/claude) request=${request%%/*}/claude ;;
    *) exit 90 ;;
esac
/bin/cp -- "$releases/$request" "$output"
"#;

const CLAUDE: &str = r#"#!/bin/sh
set -eu
case "$1" in
    --version) printf '%s (Claude Code)\n' "$version" ;;
    hold)
        test "$DISABLE_UPDATES" = 1
        : > "$2"
        read -r proceed
        test "$proceed" = proceed
        test -r "$0"
        printf '%s\n' "$version" ;;
    -p)
        test "$DISABLE_UPDATES" = 1
        test ! -e "$HOME/../releases/reject-catalog"
        read -r init
        case "$init" in *cagents-usage-initialize*) ;; *) exit 91 ;; esac
        printf '{"type":"control_response","response":{"subtype":"success","request_id":"cagents-usage-initialize","response":{"models":[{"value":"fixture-%s","displayName":"Fixture model"}]}}}\n' "$version"
        /bin/cat >/dev/null ;;
    *) exit 92 ;;
esac
"#;

struct OwnedChild(Child);

impl Drop for OwnedChild {
    fn drop(&mut self) {
        // These are direct fixture children; reaping also prevents signaling
        // a recycled PID. Closing a provider's stdin releases its hold.
        self.0.stdin.take();
        if self.0.try_wait().ok().flatten().is_none() {
            unsafe { libc::kill(self.0.id() as libc::pid_t, libc::SIGTERM) };
            let deadline = Instant::now() + Duration::from_secs(3);
            while self.0.try_wait().ok().flatten().is_none() && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(20));
            }
        }
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct Fixture(tempfile::TempDir);

#[derive(Clone, Copy)]
enum Operation {
    Manage,
    Update,
}

impl Fixture {
    fn new() -> Self {
        let fixture = Self(tempfile::tempdir().unwrap());
        for directory in [
            "home", "config", "data", "cache", "state", "run", "tmp", "bin", "releases",
        ] {
            fs::DirBuilder::new()
                .mode(0o700)
                .create(fixture.path(directory))
                .unwrap();
        }
        let config = fixture.path("config").join(STORAGE_NAME);
        fs::DirBuilder::new().mode(0o700).create(&config).unwrap();
        fs::write(
            config.join("settings.toml"),
            "update_providers_automatically = false\n",
        )
        .unwrap();
        executable(&fixture.path("bin/curl"), CURL);
        executable(
            &fixture.path("bin/gpgv"),
            "#!/bin/sh\ntest -s \"$4\" && test -s \"$6\" && test \"$(/bin/cat -- \"$5\")\" = 'test signature'\n",
        );
        for name in ["agy", "claude", "codex", "muse", "muse-upstream", "omp"] {
            executable(
                &fixture.path(&format!("bin/{name}")),
                "#!/bin/sh\necho 0.1.0\n",
            );
        }
        fixture
    }

    fn path(&self, leaf: &str) -> PathBuf {
        self.0.path().join(leaf)
    }

    fn install(&self) -> PathBuf {
        self.path("data")
            .join(STORAGE_NAME)
            .join("provider-clis/claude")
    }

    fn environment(&self) -> Vec<(&'static str, OsString)> {
        let mut variables: Vec<_> = [
            ("HOME", "home"),
            ("CLAUDE_CONFIG_DIR", "home/.claude"),
            ("CODEX_HOME", "home/.codex"),
            ("XDG_CONFIG_HOME", "config"),
            ("XDG_DATA_HOME", "data"),
            ("XDG_CACHE_HOME", "cache"),
            ("XDG_STATE_HOME", "state"),
            ("XDG_RUNTIME_DIR", "run"),
            ("TMPDIR", "tmp"),
        ]
        .into_iter()
        .map(|(name, leaf)| (name, self.path(leaf).into_os_string()))
        .collect();
        variables.push((ADOPTED_ENV, "1".into()));
        variables.push(("HTTPS_PROXY", "http://127.0.0.1:1".into()));
        let mut paths = Vec::new();
        if self.path("use-managed-bin").is_file() {
            paths.push(self.install().join("bin"));
        }
        paths.extend([self.path("bin"), "/usr/bin".into(), "/bin".into()]);
        variables.push(("PATH", std::env::join_paths(paths).unwrap()));
        variables
    }

    fn start(&self) -> OwnedChild {
        let mut daemon = OwnedChild(
            Command::new(env!("CARGO_BIN_EXE_cagentsd"))
                .env_clear()
                .envs(self.environment())
                .current_dir(self.0.path())
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(fs::File::create(self.path("daemon.log")).unwrap())
                .spawn()
                .unwrap(),
        );
        until(
            || {
                assert!(
                    daemon.0.try_wait().unwrap().is_none(),
                    "{}",
                    fs::read_to_string(self.path("daemon.log")).unwrap()
                );
                ControlClient::connect(&self.socket())
                    .ok()
                    .and_then(|mut client| client.startup_state().ok())
                    == Some(StartupState::Ready)
            },
            "private daemon readiness",
        );
        daemon
    }

    fn socket(&self) -> PathBuf {
        self.path("run")
            .join(RUNTIME_SUBDIRECTORY)
            .join(SOCKET_FILE_NAME)
    }

    fn observer(&self) -> ControlClient {
        // An explicit User for this synthetic private daemon. The CLI continues
        // to use its real authority if the test runs under delegation.
        let mut client = ControlClient::connect(&self.socket()).unwrap();
        client.authenticate(Authority::default()).unwrap();
        client
    }

    fn maintain(&self, operation: Operation) -> ProviderStatus {
        if Authority::from_environment() != Authority::default() {
            let mut client = self.observer();
            return match operation {
                Operation::Manage => client.provider_manage(Provider::Claude),
                Operation::Update => client.provider_update(Provider::Claude),
            }
            .unwrap()
            .pop()
            .unwrap();
        }
        let operation = match operation {
            Operation::Manage => "manage",
            Operation::Update => "update",
        };
        let mut command = CommandSpec::new(cli::cagents())
            .args([
                "--device",
                "local",
                "--no-start",
                "--json",
                "provider",
                operation,
                "claude",
            ])
            .cwd(self.0.path())
            .timeout(Duration::from_secs(35))
            .output_limits(64 * 1024, 64 * 1024);
        for (name, _) in std::env::vars_os() {
            command = command.env_remove(name);
        }
        for (name, value) in self.environment() {
            command = command.env(name, value);
        }
        let output = SystemProcessExecutor
            .execute(&command, &CancellationToken::new())
            .unwrap();
        let response: Value = serde_json::from_slice(&output.stdout)
            .unwrap_or_else(|error| panic!("{error}: {}", String::from_utf8_lossy(&output.stderr)));
        let status: ProviderStatus =
            serde_json::from_value(response["data"]["providers"][0].clone()).unwrap();
        let (outcome, code) = match status.last_result {
            Some(ProviderUpdateResult::Failed) => ("failed", 4),
            Some(ProviderUpdateResult::Deferred) => ("pending", 3),
            _ => ("succeeded", 0),
        };
        assert_eq!(response["data"]["outcome"], outcome);
        assert_eq!(output.exit_code, Some(code));
        status
    }

    fn status(&self) -> ProviderStatus {
        self.observer()
            .provider_status()
            .unwrap()
            .into_iter()
            .find(|status| status.provider == Provider::Claude)
            .unwrap()
    }

    fn expose(&self, version: &str, reported_version: &str) {
        let source = self.path("releases");
        let release = source.join(version);
        fs::create_dir(&release).unwrap();
        let binary = CLAUDE.replacen(
            "set -eu\n",
            &format!("set -eu\nversion={reported_version}\n"),
            1,
        );
        fs::write(release.join("claude"), &binary).unwrap();
        fs::write(release.join("manifest.json.sig"), "test signature\n").unwrap();
        let artifact = json!({"binary": "claude", "checksum": format!("{:x}", Sha256::digest(binary.as_bytes())), "size": binary.len()});
        let manifest = json!({"version": version, "platforms": {"linux-x64": artifact, "linux-arm64": artifact}});
        fs::write(release.join("manifest.json"), manifest.to_string()).unwrap();
        fs::write(source.join("latest"), version).unwrap();
        fs::write(source.join("stable"), version).unwrap();
    }

    fn launcher(&self) -> Command {
        let mut command = Command::new(self.install().join("bin/claude"));
        command.env_clear().envs(self.environment());
        command
    }

    fn catalog_id(&self) -> String {
        let cache = Cache::at(self.path("cache").join(STORAGE_NAME));
        cache.catalogs(cagents::chat::now_ms().unwrap())[&Provider::Claude].models[0]
            .id
            .clone()
    }
}

fn executable(path: &Path, body: &str) {
    fs::write(path, body).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}

fn until(mut ready: impl FnMut() -> bool, detail: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !ready() {
        assert!(Instant::now() < deadline, "timed out waiting for {detail}");
        thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn the_daemon_keeps_running_claude_versions_and_preserves_update_failures() {
    use ProviderUpdateResult::{Current, Deferred, Failed, Skipped, Updated};

    let fixture = Fixture::new();
    fixture.expose("1.0.0", "1.0.0");
    let daemon = fixture.start();
    let external = fixture.maintain(Operation::Update);
    assert_eq!(external.last_result, Some(Skipped));
    assert_eq!(external.version.as_deref(), Some("0.1.0"));
    assert!(!fixture.install().exists());
    let managed = fixture.maintain(Operation::Manage);
    assert_eq!(managed.last_result, Some(Updated), "{managed:?}");
    assert_eq!(
        managed.install_method,
        ProviderInstallMethod::CagentsManaged
    );
    assert_eq!(managed.version.as_deref(), Some("1.0.0"));
    assert!(
        managed
            .error
            .as_deref()
            .unwrap()
            .contains("first on your PATH")
    );
    assert_eq!(fixture.status().version.as_deref(), Some("0.1.0"));

    fs::write(fixture.path("use-managed-bin"), "").unwrap();
    drop(daemon);
    let daemon = fixture.start();
    let managed = fixture.maintain(Operation::Update);
    assert_eq!(managed.last_result, Some(Current), "{managed:?}");
    assert_eq!(fixture.status(), managed);
    assert_eq!(fixture.catalog_id(), "fixture-1.0.0");

    let ready = fixture.path("provider-running");
    let mut running = OwnedChild(
        fixture
            .launcher()
            .arg("hold")
            .arg(&ready)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    until(|| ready.is_file(), "old provider entry point");
    fixture.expose("2.0.0", "2.0.0");
    thread::scope(|scope| {
        let update = scope.spawn(|| fixture.maintain(Operation::Update));
        until(
            || fixture.path("releases/downloading").is_file(),
            "update download",
        );
        assert_eq!(
            fixture.maintain(Operation::Update).last_result,
            Some(Deferred)
        );
        assert_eq!(fixture.status(), managed);
        fs::write(fixture.path("releases/proceed"), "").unwrap();
        let updated = update.join().unwrap();
        assert_eq!(updated.last_result, Some(Updated), "{updated:?}");
        assert_eq!(updated.version.as_deref(), Some("2.0.0"));
    });

    running
        .0
        .stdin
        .take()
        .unwrap()
        .write_all(b"proceed\n")
        .unwrap();
    until(
        || running.0.try_wait().unwrap().is_some(),
        "old provider release",
    );
    assert!(running.0.wait().unwrap().success());
    let old = std::io::read_to_string(running.0.stdout.take().unwrap()).unwrap();
    assert_eq!(old, "1.0.0\n");
    let new = fixture.launcher().arg("--version").output().unwrap();
    assert!(new.status.success());
    assert_eq!(new.stdout, b"2.0.0 (Claude Code)\n");
    assert_eq!(fixture.catalog_id(), "fixture-2.0.0");

    fixture.expose("3.0.0", "3.5.0");
    let failed = fixture.maintain(Operation::Update);
    assert_eq!(failed.last_result, Some(Failed), "{failed:?}");
    assert!(
        failed
            .error
            .as_deref()
            .unwrap()
            .contains("Claude Code 3.0.0 reported version 3.5.0")
    );
    assert_eq!(failed.version.as_deref(), Some("2.0.0"));
    assert!(!fixture.install().join("versions/3.0.0").exists());
    drop(daemon);
    let daemon = fixture.start();
    assert_eq!(fixture.status(), failed);
    assert_eq!(fixture.catalog_id(), "fixture-2.0.0");

    fixture.expose("4.0.0", "4.0.0");
    let reject_catalog = fixture.path("releases/reject-catalog");
    fs::write(&reject_catalog, "").unwrap();
    let updated = fixture.maintain(Operation::Update);
    assert_eq!(updated.last_result, Some(Updated), "{updated:?}");
    assert!(
        updated
            .error
            .as_deref()
            .unwrap()
            .contains("Could not refresh")
    );
    assert_eq!(fixture.catalog_id(), "fixture-2.0.0");
    fs::remove_file(reject_catalog).unwrap();
    let repaired = fixture.maintain(Operation::Update);
    assert_eq!(repaired.last_result, Some(Current), "{repaired:?}");
    assert_eq!(repaired.error, None);
    assert_eq!(fixture.catalog_id(), "fixture-4.0.0");

    drop(daemon);
    fixture.expose("5.0.0", "5.0.0");
    fs::remove_file(
        fixture
            .path("config")
            .join(STORAGE_NAME)
            .join("settings.toml"),
    )
    .unwrap();
    let _daemon = fixture.start();
    until(
        || {
            fs::read_to_string(fixture.install().join("catalog-version"))
                .is_ok_and(|version| version == "5.0.0")
        },
        "scheduled provider activation and catalog refresh",
    );
    assert_eq!(fixture.status().version.as_deref(), Some("5.0.0"));
    assert_eq!(fixture.catalog_id(), "fixture-5.0.0");
}
