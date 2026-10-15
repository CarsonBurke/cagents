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
            .current_dir(root)
            .stdin(Stdio::null());
        command
    }

    fn new() -> Self {
        Self::with_provider(include_str!("fixtures/execution-console-claude.sh"))
    }

    fn with_provider(script: &str) -> Self {
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
        fs::write(&provider, script).unwrap();
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
        // Explicit close includes the provider's bounded interrupt/teardown
        // budget, which is deliberately longer than ordinary RPCs.
        let timeout = if arguments.starts_with(&["agent", "close"]) {
            80
        } else {
            20
        };
        let deadline = Instant::now() + Duration::from_secs(timeout);
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

    fn wait(&self, arguments: &[&str], expected_code: i32) -> Value {
        let output = self.output(arguments);
        assert_eq!(
            output.status.code(),
            Some(expected_code),
            "{arguments:?}: {output:?}"
        );
        let value: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(value["ok"], true);
        assert_eq!(value["data"]["results"].as_array().unwrap().len(), 1);
        value["data"]["results"][0].clone()
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
    let waiting = fixture.wait(&["agent", "wait", first_run, "--timeout", "10"], 5);
    assert_eq!(waiting["run"]["state"], "waiting", "{waiting}");
    let terminal = waiting["agent"]["console"]["id"].as_str().unwrap();
    // PTY snapshots can split the heading from its following instructions.
    // Wait for the actionable line before asserting the complete request.
    let displayed = fixture.cli(&[
        "terminal",
        "wait",
        terminal,
        "--text",
        "Use /answer console-approval",
        "--timeout",
        "10",
    ]);
    assert!(
        displayed["snapshot"]["text"]
            .as_str()
            .unwrap()
            .contains("Input requested")
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
    let completed = fixture.wait(&["agent", "wait", first_run, "--timeout", "10"], 0);
    assert_eq!(completed["run"]["state"], "succeeded", "{completed}");
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
        if second["run"]["id"] != first_run && second["run"]["state"] == "succeeded" {
            break second;
        }
        assert!(
            Instant::now() < deadline,
            "console followup did not finish: {second}"
        );
        thread::sleep(Duration::from_millis(10));
    };
    assert_ne!(second["run"]["id"], first_run);
    assert_eq!(second["run"]["state"], "succeeded", "{second}");
    assert_eq!(second["run"]["result"]["usage"]["input_tokens"], 7);
    assert_eq!(
        fs::read_to_string(fixture.root.path().join("spawns")).unwrap(),
        "spawn\n"
    );
    assert!(
        fixture
            .root
            .path()
            .join("home/.claude/skills/cagents/SKILL.md")
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
    let failed = fixture.wait(&["agent", "wait", run, "--timeout", "10"], 4);
    assert_eq!(failed["run"]["state"], "failed", "{failed}");
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

#[test]
fn cli_checkout_and_linked_worktree_share_project_and_keep_retry_receipts() {
    let fixture = Fixture::new();
    let project = fixture.root.path().join("project");
    let worktree = fixture.root.path().join("linked");
    for arguments in [
        vec!["init", "--initial-branch=main"],
        vec![
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "--allow-empty",
            "-m",
            "fixture",
        ],
        vec![
            "worktree",
            "add",
            "-b",
            "linked",
            worktree.to_str().unwrap(),
        ],
    ] {
        let output = Fixture::command(fixture.root.path(), Path::new("/usr/bin/git"))
            .current_dir(&project)
            .args(arguments)
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
    }
    let opened = fixture.cli(&["project", "open", project.to_str().unwrap()]);
    let key = opened["project"]["repository"].clone();
    let mut first = None;
    for (path, request_key) in [(&project, "checkout"), (&worktree, "worktree")] {
        let path = path.to_str().unwrap();
        let arguments = [
            "agent",
            "spawn",
            "--provider",
            "claude",
            "--repository",
            path,
            "--directory",
            path,
            "--prompt",
            "wait for consent",
            "--idempotency-key",
            request_key,
        ];
        let accepted = fixture.cli(&arguments);
        assert_eq!(accepted["agent"]["spec"]["target"]["repository"], key);
        assert_eq!(accepted["agent"]["spec"]["target"]["directory"], path);
        let agent = accepted["agent"]["id"].as_str().unwrap();
        let run = accepted["run"]["id"].as_str().unwrap();
        assert_eq!(
            fixture.wait(&["agent", "wait", run, "--timeout", "10"], 5)["run"]["state"],
            "waiting"
        );
        fixture.cli(&["agent", "close", agent]);
        if request_key == "checkout" {
            first = Some((accepted, arguments.map(str::to_owned)));
        }
    }
    fs::rename(&project, fixture.root.path().join("retired-project")).unwrap();
    let (first, arguments) = first.unwrap();
    let retry = fixture.cli(&arguments.iter().map(String::as_str).collect::<Vec<_>>());
    assert_eq!(retry["created"], false);
    assert_eq!(retry["agent"]["id"], first["agent"]["id"]);
    assert_eq!(retry["run"]["id"], first["run"]["id"]);
}

#[test]
fn an_owner_drives_its_agent_by_short_handles() {
    let fixture = Fixture::new();
    let project = fixture.root.path().join("project");
    let project = project.to_str().unwrap();
    fixture.cli(&["project", "open", project]);
    let accepted = fixture.cli(&[
        "agent",
        "spawn",
        "--provider",
        "claude",
        "--project",
        project,
        "first",
    ]);
    assert_eq!(accepted["agent"]["spec"]["target"]["directory"], project);
    assert!(
        accepted["idempotency_key"]
            .as_str()
            .is_some_and(|key| !key.is_empty())
    );
    let agent = &accepted["agent"]["id"].as_str().unwrap()[..8];
    let run = &accepted["run"]["id"].as_str().unwrap()[..8];

    let waiting = fixture.wait(&["agent", "wait", agent, "--timeout", "10"], 5);
    assert_eq!(waiting["run"]["state"], "waiting", "{waiting}");
    let requests = fixture.cli(&["agent", "requests", agent]);
    assert_eq!(
        requests["requests"][0]["id"], "console-approval",
        "{requests}"
    );
    let refused = fixture.output(&["agent", "answer", agent, "console-approval", "always"]);
    assert!(!refused.status.success(), "{refused:?}");
    let answered = fixture.cli(&["agent", "answer", agent, "console-approval", "allow"]);
    assert_eq!(answered["requests"], serde_json::json!([]));
    let completed = fixture.wait(&["agent", "wait", run, "--timeout", "10"], 0);
    assert_eq!(completed["run"]["state"], "succeeded", "{completed}");
    assert_eq!(completed["run"]["result"]["text"], "CONSOLE_RESULT_1");

    let followed = fixture.cli(&[
        "agent",
        "send",
        agent,
        "second",
        "--wait",
        "--timeout",
        "10",
    ]);
    assert_eq!(followed["run"]["state"], "succeeded", "{followed}");
    assert_eq!(followed["timed_out"], false);
    let idle = fixture.output(&["agent", "interrupt", agent]);
    assert!(!idle.status.success(), "nothing to interrupt: {idle:?}");
    assert_eq!(
        fixture.cli(&["agent", "close", agent])["agent"]["closed"],
        true
    );
}

#[test]
fn waiting_cli_commands_report_timeout_cancellation_failure_and_success() {
    let fixture = Fixture::with_provider(include_str!("fixtures/wait-outcomes-claude.sh"));
    let project_path = fixture.root.path().join("project");
    let project = project_path.to_str().unwrap();
    fixture.cli(&["project", "open", project]);
    let output = fixture.output(&[
        "agent",
        "spawn",
        "CLI_WAIT_BUSY",
        "--provider",
        "claude",
        "--project",
        project,
        "--wait",
        "--timeout",
        "1",
    ]);
    assert_eq!(output.status.code(), Some(3), "{output:?}");
    let accepted: Value = serde_json::from_slice(&output.stdout).unwrap();
    let accepted = &accepted["data"];
    let agent = accepted["agent"]["id"].as_str().unwrap();
    let run = accepted["run"]["id"].as_str().unwrap();
    assert_eq!(accepted["outcome"], "timed_out");
    let timed = fixture.output(&["agent", "wait", run, "--timeout", "1"]);
    assert_eq!(timed.status.code(), Some(3), "{timed:?}");
    let one: Value = serde_json::from_slice(&timed.stdout).unwrap();
    let many = fixture.output(&["agent", "wait", run, run, "--timeout", "0.1"]);
    assert_eq!(many.status.code(), Some(3), "{many:?}");
    let many: Value = serde_json::from_slice(&many.stdout).unwrap();
    assert_eq!(
        one["data"].as_object().unwrap().keys().collect::<Vec<_>>(),
        many["data"].as_object().unwrap().keys().collect::<Vec<_>>()
    );
    fixture.cli(&["agent", "interrupt", agent]);
    let cancelled = fixture.wait(&["agent", "wait", run, "--timeout", "10"], 4);
    assert_eq!(cancelled["run"]["state"], "cancelled");
    let follow = fixture.output(&[
        "agent",
        "send",
        agent,
        "CLI_WAIT_FAIL",
        "--wait",
        "--timeout",
        "10",
    ]);
    assert_eq!(follow.status.code(), Some(4), "{follow:?}");
    let follow: Value = serde_json::from_slice(&follow.stdout).unwrap();
    assert_eq!(follow["data"]["outcome"], "failed");
    let success = fixture.cli(&[
        "agent",
        "send",
        agent,
        "CLI_WAIT_SUCCESS",
        "--wait",
        "--timeout",
        "10",
    ]);
    assert_eq!(success["outcome"], "succeeded");
    let succeeded_run = success["run"]["id"].as_str().unwrap();
    let failed_run = follow["data"]["run"]["id"].as_str().unwrap();
    let first_success = fixture.output(&[
        "agent",
        "wait",
        succeeded_run,
        failed_run,
        "--any",
        "--timeout",
        "1",
    ]);
    assert_eq!(first_success.status.code(), Some(0), "{first_success:?}");
    let first_failure = fixture.output(&[
        "agent",
        "wait",
        failed_run,
        succeeded_run,
        "--any",
        "--timeout",
        "1",
    ]);
    assert_eq!(first_failure.status.code(), Some(4), "{first_failure:?}");
    let all = fixture.output(&["agent", "wait", succeeded_run, failed_run, "--timeout", "1"]);
    assert_eq!(all.status.code(), Some(4), "{all:?}");
    fixture.cli(&["agent", "close", agent]);
}

#[test]
fn scripting_account_defaults_and_settings_use_the_shared_persistent_configuration() {
    let fixture = Fixture::new();
    let accounts = fixture.cli(&["account", "list"]);
    assert!(accounts["accounts"].as_array().unwrap().iter().any(|account| account["id"] == "claude/system"));
    let selected = fixture.cli(&["account", "default", "claude", "system"]);
    assert_eq!(selected["account"], "claude/system");
    let accounts = fixture.cli(&["account", "list"]);
    assert!(accounts["accounts"].as_array().unwrap().iter().any(|account| account["id"] == "claude/system" && account["default"] == true));
    let automatic = fixture.cli(&["account", "default", "claude", "automatic"]);
    assert_eq!(automatic["automatic"], true);
    let changed = fixture.cli(&["settings", "set", "terminal_font_size", "15"]);
    assert_eq!(changed["value"], 15.0);
    let path = changed["path"].as_str().unwrap();
    assert_eq!(cagents::settings::load(path).unwrap().terminal_font_size, 15.0,
        "the same loader used by running windows sees the persisted change");
    assert_eq!(fixture.cli(&["settings", "get", "terminal_font_size"])["value"], 15.0);
    let invalid = fixture.output(&["settings", "set", "terminal_font_size", "1"]);
    assert_eq!(invalid.status.code(), Some(1));
    let error: Value = serde_json::from_slice(&invalid.stderr).unwrap();
    let mut settings = cagents::settings::AppSettings::default(); settings.terminal_font_size = 1.0;
    assert_eq!(error["error"]["message"], settings.validate().unwrap_err().to_string());
}

struct FollowGuard(Child);
impl Drop for FollowGuard { fn drop(&mut self) { let _ = self.0.kill(); let _ = self.0.wait(); } }

#[test]
fn transcript_follow_streams_appended_items_and_exits_after_agent_close() {
    use std::io::BufRead;
    let fixture = Fixture::new();
    let project_path = fixture.root.path().join("project"); let project = project_path.to_str().unwrap();
    fixture.cli(&["project", "open", project]);
    let accepted = fixture.cli(&["agent", "spawn", "first", "--provider", "claude", "--project", project]);
    let agent = accepted["agent"]["id"].as_str().unwrap();
    fixture.wait(&["agent", "wait", agent, "--timeout", "10"], 5);
    let exported = fixture.cli(&["agent", "transcript", agent, "--format", "json"]);
    assert!(!exported["parts"].as_array().unwrap().is_empty());
    let markdown = Fixture::command(fixture.root.path(), Path::new(env!("CARGO_BIN_EXE_cagents")))
        .args(["agent", "transcript", agent, "--format", "md"]).output().unwrap();
    assert!(markdown.status.success(), "{markdown:?}");
    assert!(String::from_utf8_lossy(&markdown.stdout).contains("## User"));
    let child = Fixture::command(fixture.root.path(), Path::new(env!("CARGO_BIN_EXE_cagents")))
        .args(["agent", "transcript", agent, "--follow", "--format", "json"])
        .stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
    let mut child = FollowGuard(child);
    let stdout = child.0.stdout.take().unwrap();
    let (sent, received) = std::sync::mpsc::channel();
    let reader = thread::spawn(move || for line in std::io::BufReader::new(stdout).lines() {
        if sent.send(line.unwrap()).is_err() { break; }
    });
    let initial = received.recv_timeout(Duration::from_secs(10)).expect("initial transcript page");
    let initial: Value = serde_json::from_str(&initial).unwrap();
    assert_eq!(initial["command"], "agent.transcript");
    fixture.cli(&["agent", "answer", agent, "console-approval", "allow"]);
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let line = received.recv_timeout(deadline.saturating_duration_since(Instant::now())).expect("appended assistant result");
        if line.contains("CONSOLE_RESULT_1") { break; }
    }
    fixture.cli(&["agent", "close", agent]);
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(status) = child.0.try_wait().unwrap() { assert!(status.success()); break; }
        assert!(Instant::now() < deadline, "transcript follow did not exit after close");
        thread::sleep(Duration::from_millis(10));
    }
    reader.join().unwrap();
}

#[test]
fn terminal_lists_show_native_titles_and_exit_status_without_launch_secrets() {
    let fixture = Fixture::new();
    let project_path = fixture.root.path().join("project");
    let project = project_path.to_str().unwrap();
    let spawned = fixture.cli(&["terminal", "spawn", "--cwd", project, "--", "sh", "-c",
        "printf '\\033]0;Fixture shell\\007'; exit 3"]);
    let id = spawned["terminal"]["session"]["id"].as_str().unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let completed = loop {
        let listed = fixture.cli(&["terminal", "list", "--all"]);
        let terminal = listed["terminals"].as_array().unwrap().iter().find(|terminal| terminal["id"] == id).unwrap();
        if terminal["state"] == "exited" { break terminal.clone(); }
        assert!(Instant::now() < deadline, "terminal did not exit: {listed}");
        thread::sleep(Duration::from_millis(10));
    };
    assert_eq!(completed["exit_code"], 3);
    assert_eq!(completed["title"], "Fixture shell");
    assert!(!completed.to_string().contains("environment"));
    assert!(!completed.to_string().contains("resume_hint"));
    assert!(fixture.cli(&["terminal", "list"])["terminals"].as_array().unwrap().iter().all(|terminal| terminal["id"] != id));
    assert!(fixture.cli(&["terminal", "list", "--exited"])["terminals"].as_array().unwrap().iter().any(|terminal| terminal["id"] == id));
    let shown = fixture.cli(&["terminal", "show", id]);
    assert!(shown["terminal"]["session"]["launch"]["environment"].is_object());
}
