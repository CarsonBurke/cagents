#![cfg(feature = "host")]

#[path = "support/hosted_agent_cleanup.rs"]
mod hosted_agent_cleanup;

#[path = "support/private_cli_capture.rs"]
mod private_cli_capture;

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
    agents: hosted_agent_cleanup::Agents,
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
        let fixture = Self {
            root,
            daemon,
            agents: Default::default(),
        };
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let output = fixture.output_until(&["daemon", "status"], deadline);
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
        // Explicit close includes the provider's bounded interrupt/teardown
        // budget, which is deliberately longer than ordinary RPCs.
        let timeout = if arguments.starts_with(&["agent", "close"]) {
            80
        } else {
            20
        };
        self.output_until(arguments, Instant::now() + Duration::from_secs(timeout))
    }

    fn output_until(&self, arguments: &[&str], deadline: Instant) -> Output {
        let spec = private_cli_capture::private_cli(self.root.path())
            .env("TERM", "xterm-256color")
            .args(arguments.iter().copied());
        let output = private_cli_capture::capture(spec, deadline)
            .unwrap_or_else(|failure| panic!("CLI failed: {arguments:?}: {failure:?}"));
        self.agents.observe(&output);
        output
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
        let panicking = thread::panicking();
        if panicking {
            self.root.disable_cleanup(true);
            hosted_agent_cleanup::diagnostics(self.root.path());
        }
        // Daemon stop deliberately preserves retained actors. Only after all
        // assertions, explicitly retire exact agents this private CLI accepted.
        let result = self
            .agents
            .close(self.root.path())
            .and_then(|()| hosted_agent_cleanup::stop(self.root.path()));
        let deadline = Instant::now() + Duration::from_secs(3);
        while self.daemon.try_wait().ok().flatten().is_none() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        let daemon_stopped = self.daemon.try_wait().ok().flatten().is_some();
        if !daemon_stopped {
            let _ = self.daemon.kill();
        }
        let _ = self.daemon.wait();
        if let Err(error) = result.and_then(|()| {
            if daemon_stopped {
                Ok(())
            } else {
                Err("Private daemon did not exit after public cleanup".into())
            }
        }) {
            self.root.disable_cleanup(true);
            hosted_agent_cleanup::diagnostics(self.root.path());
            eprintln!(
                "Private cleanup evidence retained at {}: {error}",
                self.root.path().display()
            );
            if !panicking {
                panic!("private fixture cleanup failed: {error}");
            }
        }
    }
}

#[test]
fn user_cli_keeps_100_completed_chats_open_and_replays_without_another_prompt() {
    let fixture = Fixture::with_provider(include_str!("fixtures/automation-claude.sh"));
    let project_path = fixture.root.path().join("project");
    let project = project_path.to_str().unwrap();
    fixture.cli(&["project", "open", project]);
    let mut completed = std::collections::BTreeMap::new();
    let mut first = None;
    for index in 0..100 {
        let name = format!("capacity-{index}");
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
            "chat",
            "--name",
            &name,
            "--prompt",
            "capacity fixture",
            "--idempotency-key",
            &name,
            "--wait",
            "--timeout",
            "10",
        ]);
        assert_eq!(accepted["created"], true, "conversation {index}");
        assert_eq!(accepted["timed_out"], false);
        assert_eq!(accepted["agent"]["origin"], "User");
        assert_eq!(accepted["agent"]["closed"], false);
        assert!(accepted["agent"]["session"]["Chat"].is_object());
        assert_eq!(accepted["run"]["state"], "Succeeded");
        assert_eq!(accepted["run"]["result"]["text"], "SCHEDULED_RESULT");
        let agent = accepted["agent"]["id"].as_str().unwrap().to_owned();
        let run = accepted["run"]["id"].as_str().unwrap().to_owned();
        assert!(completed.insert(agent, run).is_none());
        if index == 0 {
            first = Some(accepted);
        }
    }
    assert_eq!(completed.len(), 100);
    // `agent list` currently has no cursor and reads at most 1024 summaries.
    // Query every exact ID through its documented bounded status command:
    // this avoids aggregating 100 verbose JSON rows in the fixture's stdout pipe.
    // All 100 remain open; neither this loop nor setup closes an agent.
    for (agent, run) in &completed {
        let retained = fixture.cli(&["agent", "status", agent]);
        assert_eq!(retained["agent"]["id"].as_str(), Some(agent.as_str()));
        assert_eq!(retained["run"]["id"].as_str(), Some(run.as_str()));
        assert_eq!(retained["agent"]["origin"], "User");
        assert_eq!(retained["agent"]["closed"], false);
        assert_eq!(retained["run"]["state"], "Succeeded");
    }
    let retried = fixture.cli(&[
        "agent",
        "spawn",
        "--provider",
        "claude",
        "--repository",
        project,
        "--directory",
        project,
        "--surface",
        "chat",
        "--name",
        "capacity-0",
        "--prompt",
        "capacity fixture",
        "--idempotency-key",
        "capacity-0",
        "--wait",
        "--timeout",
        "10",
    ]);
    let first = first.unwrap();
    assert_eq!(retried["created"], false);
    assert_eq!(retried["agent"]["id"], first["agent"]["id"]);
    assert_eq!(retried["run"]["id"], first["run"]["id"]);
    assert_eq!(retried["run"]["state"], "Succeeded");
    assert_eq!(retried["agent"]["closed"], false);
    let prompts = fs::read_to_string(fixture.root.path().join("prompts")).unwrap();
    assert_eq!(prompts.lines().count(), 100);
    assert!(prompts.lines().all(|line| line == "prompt"));
    // Existing fixture Drop retires the private counted inventory only after
    // all retained-conversation and exactly-once provider assertions above.
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
    assert_eq!(waiting["run"]["state"], "Waiting", "{waiting}");
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
        let arguments = ["agent", "status", agent];
        let output = fixture.output_until(&arguments, deadline);
        assert!(output.status.success(), "{arguments:?}: {output:?}");
        let second: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(second["ok"], true, "{second}");
        let second = second["data"].clone();
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
            "Waiting"
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
    assert_eq!(waiting["run"]["state"], "Waiting", "{waiting}");
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
    assert_eq!(completed["run"]["state"], "Succeeded", "{completed}");
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
    assert_eq!(followed["run"]["state"], "Succeeded", "{followed}");
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
    assert_eq!(cancelled["run"]["state"], "Cancelled");
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
fn active_native_chat_moves_without_restarting_and_accepts_followups() {
    use cagents::{
        control::{ControlClient, ControlError},
        model::{CardId, CardKind, ChannelId},
        terminal::ipc::ProtocolErrorCode,
        workspace_model::{
            ModelEdit,
            views::{Change, Pane, Scope},
        },
    };

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
        "--surface",
        "chat",
        "--prompt",
        "first",
        "--idempotency-key",
        "native-move",
    ]);
    let agent = accepted["agent"]["id"].as_str().unwrap();
    let run = accepted["run"]["id"].as_str().unwrap();
    let waiting = fixture.wait(&["agent", "wait", run, "--timeout", "10"], 5);
    assert_eq!(waiting["run"]["state"], "Waiting", "{waiting}");
    let requests = fixture.cli(&["agent", "requests", agent]);
    let destination = fixture.cli(&["channel", "spawn", "--repository", project]);
    let destination = destination["channel"]["id"].as_str().unwrap();
    let status = fixture.cli(&["daemon", "status"]);
    let socket = Path::new(status["socket"].as_str().unwrap());
    let mut client = ControlClient::connect(socket).unwrap();
    let session = client.list_chats().unwrap().into_iter().next().unwrap();
    let model = client.read_workspace_model().unwrap();
    let placement = model
        .agents
        .iter()
        .find(|entry| entry.id.to_string() == agent)
        .unwrap()
        .placement
        .clone();
    let pane = Pane {
        id: CardId::from_uuid(agent.parse().unwrap()),
        title: "Native chat".into(),
        user_title: Some("Keep this identity".into()),
        pending_terminal_start: false,
        width_fraction: 0.6,
        scope: Some(project.into()),
        kind: CardKind::Chat(session.clone()),
    };
    client
        .edit_workspace_model(ModelEdit::SeedView {
            scope: placement.clone(),
            panes: vec![pane.clone()],
            closed: vec![],
        })
        .unwrap();
    // Exercise actual framed control requests against this private daemon:
    // invalid membership must be refused, leave every field unchanged, and
    // keep the connection usable for the valid move that follows.
    let foreign_project = fixture.root.path().join("other-project");
    fs::create_dir(&foreign_project).unwrap();
    let foreign_project = foreign_project.to_str().unwrap();
    fixture.cli(&["project", "open", foreign_project]);
    let foreign = fixture.cli(&["channel", "spawn", "--repository", foreign_project]);
    let foreign: ChannelId = foreign["channel"]["id"].as_str().unwrap().parse().unwrap();
    let before = client.read_workspace_model().unwrap();
    let revision = before
        .views
        .iter()
        .find(|view| view.scope == placement)
        .unwrap()
        .revision;
    for (scope, id, target) in [
        (
            Scope::Channel(ChannelId::new()),
            pane.id,
            destination.parse().unwrap(),
        ),
        (placement.clone(), pane.id, ChannelId::new()),
        (
            placement.clone(),
            CardId::new(),
            destination.parse().unwrap(),
        ),
        (placement.clone(), pane.id, foreign),
    ] {
        let error = client
            .edit_workspace_model(ModelEdit::ChangeView {
                scope,
                expected_revision: revision,
                operation: uuid::Uuid::new_v4(),
                change: Change::Transfer {
                    id,
                    destination: Scope::Channel(target),
                    before: None,
                },
            })
            .unwrap_err();
        assert!(matches!(
            error,
            ControlError::Remote {
                code: ProtocolErrorCode::InvalidRequest,
                retryable: false,
                ..
            }
        ));
        assert_eq!(client.read_workspace_model().unwrap(), before);
    }
    let moved = fixture.cli(&[
        "channel",
        "move",
        &session.id.to_string(),
        "--to",
        destination,
    ]);
    assert_eq!(
        moved["placement"],
        serde_json::to_value(Scope::Channel(destination.parse().unwrap())).unwrap()
    );
    assert_eq!(moved["pane"], serde_json::to_value(&pane).unwrap());
    let no_op = fixture.cli(&[
        "channel",
        "move",
        &session.id.to_string(),
        "--to",
        destination,
    ]);
    assert_eq!(no_op["placement"], moved["placement"]);
    let still_waiting = fixture.cli(&["agent", "status", agent]);
    assert_eq!(
        still_waiting["run"], waiting["run"],
        "moving must not change the active run"
    );
    assert_eq!(
        still_waiting["agent"]["session"],
        waiting["agent"]["session"]
    );
    assert_eq!(still_waiting["agent"]["spec"], waiting["agent"]["spec"]);
    assert_eq!(fixture.cli(&["agent", "requests", agent]), requests);
    // A fresh client reads the committed placement and exact descriptor.
    drop(client);
    let mut client = ControlClient::connect(socket).unwrap();
    let model = client.read_workspace_model().unwrap();
    let target = Scope::Channel(destination.parse().unwrap());
    assert!(
        model
            .views
            .iter()
            .find(|view| view.scope == placement)
            .unwrap()
            .panes
            .is_empty()
    );
    assert_eq!(
        model
            .views
            .iter()
            .find(|view| view.scope == target)
            .unwrap()
            .panes,
        vec![pane]
    );
    assert_eq!(client.list_chats().unwrap(), vec![session]);
    fixture.cli(&["agent", "answer", agent, "console-approval", "allow"]);
    let completed = fixture.wait(&["agent", "wait", run, "--timeout", "10"], 0);
    assert_eq!(completed["run"]["state"], "Succeeded");
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
    assert_eq!(followed["run"]["state"], "Succeeded", "{followed}");
    assert_eq!(followed["run"]["result"]["text"], "CONSOLE_RESULT_2");
    assert_eq!(
        fs::read_to_string(fixture.root.path().join("spawns")).unwrap(),
        "spawn\n"
    );
    assert_eq!(
        fs::read_to_string(fixture.root.path().join("prompts")).unwrap(),
        "prompt\nprompt\n"
    );
    fixture.cli(&["agent", "close", agent]);
}
