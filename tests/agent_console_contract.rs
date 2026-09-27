#![cfg(feature = "host")]

use serde_json::Value;
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::Path,
    process::{Child, Command, Output, Stdio},
    thread,
    time::{Duration, Instant},
};

// Exercise the shipped CLI, daemon, actual PTY console and provider protocol
// together. Every filesystem path and subprocess belongs to this fixture.
struct Fixture {
    root: tempfile::TempDir,
    daemon: Child,
}

impl Fixture {
    fn command(root: &Path, program: &Path) -> Command {
        let mut command = Command::new(program);
        command
            .env_clear()
            .env("HOME", root.join("home"))
            .env("XDG_RUNTIME_DIR", root.join("run"))
            .env("XDG_DATA_HOME", root.join("data"))
            .env("XDG_CONFIG_HOME", root.join("config"))
            .env("XDG_CACHE_HOME", root.join("cache"))
            .env("TMPDIR", root.join("tmp"))
            .env("TERM", "xterm-256color")
            .env(
                "PATH",
                format!("{}:/usr/bin:/bin", root.join("bin").display()),
            )
            .current_dir(root.join("project"))
            .stdin(Stdio::null());
        command
    }

    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        for directory in [
            "home/.claude",
            "run",
            "data",
            "config",
            "cache",
            "tmp",
            "bin",
            "project",
        ] {
            fs::create_dir_all(root.path().join(directory)).unwrap();
        }
        fs::set_permissions(root.path().join("run"), fs::Permissions::from_mode(0o700)).unwrap();
        let provider = root.path().join("bin/claude");
        fs::write(
            &provider,
            include_str!("fixtures/execution-console-claude.sh"),
        )
        .unwrap();
        fs::set_permissions(provider, fs::Permissions::from_mode(0o700)).unwrap();
        let log = fs::File::create(root.path().join("daemon.log")).unwrap();
        let daemon = Self::command(root.path(), Path::new(env!("CARGO_BIN_EXE_cagentsd")))
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .spawn()
            .unwrap();
        let fixture = Self { root, daemon };
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let output = fixture.output(&["daemon", "status"]);
            if output.status.success()
                && serde_json::from_slice::<Value>(&output.stdout).unwrap()["data"]["running"]
                    == true
            {
                break;
            }
            assert!(Instant::now() < deadline, "fixture daemon did not start");
            thread::sleep(Duration::from_millis(20));
        }
        fixture
    }

    fn output(&self, arguments: &[&str]) -> Output {
        let mut child = Self::command(self.root.path(), Path::new(env!("CARGO_BIN_EXE_cagents")))
            .arg("--json")
            .args(arguments)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(20);
        while child.try_wait().unwrap().is_none() {
            if Instant::now() >= deadline {
                let _ = child.kill();
                let output = child.wait_with_output().unwrap();
                panic!("CLI timed out: {arguments:?}: {output:?}");
            }
            thread::sleep(Duration::from_millis(10));
        }
        child.wait_with_output().unwrap()
    }

    fn cli(&self, arguments: &[&str]) -> Value {
        let output = self.output(arguments);
        assert!(output.status.success(), "{arguments:?}: {output:?}");
        let value: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(value["ok"], true, "{value}");
        value["data"].clone()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        // Confirm only this isolated daemon's exact counted inventory. This
        // also retires the provider if an assertion fails while it is waiting.
        let preflight = self.output(&["daemon", "preflight"]);
        if let Ok(value) = serde_json::from_slice::<Value>(&preflight.stdout)
            && let Some(capability) = value["data"].get("capability")
        {
            let _ = self.output(&["daemon", "stop", "--confirm", &capability.to_string()]);
        }
        let _ = self.daemon.kill();
        let _ = self.daemon.wait();
    }
}

#[test]
fn terminal_console_answers_requests_and_submits_durable_followups() {
    let fixture = Fixture::new();
    let project = fixture.root.path().join("project");
    let project = project.to_str().unwrap();
    fixture.cli(&["project", "open", project]);
    let accepted = fixture.cli(&[
        "agent",
        "spawn",
        "--provider",
        "claude",
        "--repository",
        project,
        "--directory",
        project,
        "--surface",
        "terminal",
        "--prompt",
        "first",
        "--idempotency-key",
        "console-first",
    ]);
    let agent = accepted["agent"]["id"].as_str().unwrap();
    let first_run = accepted["run"]["id"].as_str().unwrap();
    let waiting = fixture.cli(&["agent", "wait", first_run, "--timeout", "10"]);
    assert_eq!(waiting["run"]["state"], "Waiting", "{waiting}");
    let terminal = waiting["agent"]["console"]["id"].as_str().unwrap();
    let displayed = fixture.cli(&[
        "terminal",
        "wait",
        terminal,
        "--text",
        "Input requested",
        "--timeout",
        "10",
    ]);
    assert!(
        displayed["snapshot"]["text"]
            .as_str()
            .unwrap()
            .contains("/answer")
    );
    fixture.cli(&[
        "terminal",
        "write",
        terminal,
        "/answer console-approval allow\n",
    ]);
    fixture.cli(&[
        "terminal",
        "wait",
        terminal,
        "--text",
        "Succeeded",
        "--timeout",
        "10",
    ]);
    let completed = fixture.cli(&["agent", "wait", first_run, "--timeout", "10"]);
    assert_eq!(completed["run"]["state"], "Succeeded", "{completed}");
    assert_eq!(completed["run"]["result"]["text"], "CONSOLE_RESULT_1");
    fixture.cli(&["terminal", "write", terminal, "second prompt\n"]);
    fixture.cli(&[
        "terminal",
        "wait",
        terminal,
        "--text",
        "CONSOLE_RESULT_2",
        "--timeout",
        "10",
    ]);
    let deadline = Instant::now() + Duration::from_secs(10);
    let second = loop {
        let second = fixture.cli(&["agent", "status", agent]);
        if second["run"]["id"] != first_run && second["run"]["state"] == "Succeeded" {
            break second;
        }
        assert!(
            Instant::now() < deadline,
            "console followup did not finish: {second}"
        );
        thread::sleep(Duration::from_millis(10));
    };
    assert_ne!(second["run"]["id"], first_run);
    assert_eq!(second["run"]["state"], "Succeeded", "{second}");
    assert_eq!(second["run"]["result"]["usage"]["input_tokens"], 7);
    assert_eq!(
        fs::read_to_string(fixture.root.path().join("spawns")).unwrap(),
        "spawn\n"
    );
    assert!(
        fixture
            .root
            .path()
            .join("home/.claude/skills/cagents-agents/SKILL.md")
            .is_file()
    );
    let closed = fixture.cli(&["agent", "close", agent]);
    assert_eq!(closed["agent"]["closed"], true);
}

#[test]
fn failed_console_publication_retires_terminal_and_retains_idle_provider() {
    let fixture = Fixture::new();
    let project = fixture.root.path().join("project");
    let project = project.to_str().unwrap();
    fixture.cli(&["project", "open", project]);
    let database = fixture
        .root
        .path()
        .join("data")
        .join(cagents::instance::STORAGE_NAME)
        .join("workspace-model.sqlite3");
    rusqlite::Connection::open(database)
        .unwrap()
        .execute_batch(
            "CREATE TRIGGER refuse_console BEFORE UPDATE OF record ON agents
         WHEN json_extract(NEW.record, '$.console') IS NOT NULL
         BEGIN SELECT RAISE(ABORT, 'injected console publication failure'); END;",
        )
        .unwrap();
    let accepted = fixture.cli(&[
        "agent",
        "spawn",
        "--provider",
        "claude",
        "--repository",
        project,
        "--directory",
        project,
        "--surface",
        "terminal",
        "--prompt",
        "must not send",
        "--idempotency-key",
        "failed-console",
    ]);
    let agent = accepted["agent"]["id"].as_str().unwrap();
    let run = accepted["run"]["id"].as_str().unwrap();
    let failed = fixture.cli(&["agent", "wait", run, "--timeout", "10"]);
    assert_eq!(failed["run"]["state"], "Failed", "{failed}");
    assert!(
        failed["run"]["result"]["diagnostic"]
            .as_str()
            .unwrap()
            .contains("injected console publication failure")
    );
    assert!(!failed["agent"]["session"].is_null());
    assert!(failed["agent"]["console"].is_null());
    assert_eq!(failed["agent"]["closed"], false);
    assert!(
        fixture.cli(&["terminal", "list"])["terminals"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert!(!fixture.root.path().join("prompts").exists());
    assert_eq!(
        fixture.cli(&["agent", "close", agent])["agent"]["closed"],
        true
    );
}
