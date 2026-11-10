#![cfg(feature = "host")]

#[path = "support/hosted_agent_cleanup.rs"]
mod hosted_agent_cleanup;

#[path = "support/private_cli_capture.rs"]
mod private_cli_capture;
#[path = "support/retained_fixture.rs"]
mod retained_fixture;

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
        // These console scenarios use a private checkout with a HEAD. Keep
        // its Git identity, configuration and initial commit in this fixture.
        for arguments in [
            vec!["init", "--initial-branch=main", "--template="],
            vec!["commit", "--allow-empty", "-m", "fixture"],
        ] {
            let output = Self::command(root.path(), Path::new("/usr/bin/git"))
                .current_dir(root.path().join("project"))
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .args([
                    "-c",
                    "core.hooksPath=/dev/null",
                    "-c",
                    "commit.gpgsign=false",
                    "-c",
                    "user.name=Fixture",
                    "-c",
                    "user.email=fixture@example.invalid",
                ])
                .args(arguments)
                .output()
                .unwrap();
            assert!(output.status.success(), "private Git setup: {output:?}");
        }
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

    /// Runs an `agent spawn` that names an idempotency key, repeating it with
    /// that key while the daemon's acceptance goes unanswered, as the CLI's
    /// error instructs. Acceptance commits durably before the daemon answers,
    /// and a stalled disk can hold that commit past even the CLI's 30 s
    /// acceptance budget, which this capacity contract does not test. The key
    /// keeps a repeat from starting the work twice. Says whether it repeated.
    fn answered_spawn(&self, arguments: &[&str]) -> (Output, bool) {
        let hang_guard = Instant::now() + Duration::from_secs(120);
        let mut repeated = false;
        loop {
            // Thirty seconds per submit, its built-in retry and the 10 s wait
            // can together outlast output()'s whole 20 s, so allow more here.
            let output = self.output_until(arguments, Instant::now() + Duration::from_secs(80));
            let unanswered = !output.status.success()
                && serde_json::from_slice::<Value>(&output.stderr).is_ok_and(|error| {
                    error["error"]["details"]["idempotency_key"].is_string()
                        && error["error"]["message"]
                            .as_str()
                            .is_some_and(|message| message.contains("the daemon did not answer"))
                });
            if !unanswered {
                return (output, repeated);
            }
            assert!(
                Instant::now() < hang_guard,
                "acceptance never answered: {output:?}"
            );
            repeated = true;
        }
    }

    /// Returns an `agent spawn --wait` receipt with its run, once settled, as
    /// `settled`. The CLI's wait budget is not the contract under test: under
    /// load a run can still be starting when it lapses, so this keeps waiting
    /// on that exact run.
    fn settled_spawn(&self, spawned: Output) -> Value {
        let json = |output: &Output| {
            // Exit 3 is the CLI's timed-out wait; anything else is a failure.
            assert!(matches!(output.status.code(), Some(0 | 3)), "{output:?}");
            let value: Value = serde_json::from_slice(&output.stdout).unwrap();
            assert_eq!(value["ok"], true, "{value}");
            value["data"].clone()
        };
        let mut receipt = json(&spawned);
        let run = receipt["run"]["id"].as_str().unwrap().to_owned();
        let mut settled = receipt.clone();
        let mut timed_out = !spawned.status.success();
        let hang_guard = Instant::now() + Duration::from_secs(120);
        while timed_out {
            assert!(Instant::now() < hang_guard, "run never settled: {settled}");
            let waited = self.output(&["agent", "wait", &run, "--timeout", "10"]);
            settled = json(&waited)["results"][0].clone();
            timed_out = !waited.status.success();
        }
        receipt["settled"] = settled;
        receipt
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
    if retained_fixture::run_isolated() {
        return;
    }
    let fixture = Fixture::with_provider(include_str!("fixtures/automation-claude.sh"));
    let project_path = fixture.root.path().join("project");
    let project = project_path.to_str().unwrap();
    fixture.cli(&["project", "open", project]);
    let mut completed = std::collections::BTreeMap::new();
    let mut first = None;
    for index in 0..100 {
        let name = format!("capacity-{index}");
        let (spawned, repeated) = fixture.answered_spawn(&[
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
        let receipt = fixture.settled_spawn(spawned);
        assert!(receipt["retried"].is_boolean(), "{receipt}");
        // A repeat, or the CLI's own retry after a lost answer, finds the
        // acceptance its unanswered attempt committed.
        assert!(
            receipt["created"] == true || receipt["retried"] == true || repeated,
            "conversation {index}: {receipt}"
        );
        let accepted = &receipt["settled"];
        assert_eq!(accepted["agent"]["origin"], "User");
        assert_eq!(accepted["agent"]["closed"], false);
        assert!(accepted["agent"]["session"]["Chat"].is_object());
        assert_eq!(accepted["run"]["state"], "Succeeded");
        assert_eq!(accepted["run"]["result"]["text"], "SCHEDULED_RESULT");
        let agent = accepted["agent"]["id"].as_str().unwrap().to_owned();
        let run = accepted["run"]["id"].as_str().unwrap().to_owned();
        assert!(completed.insert(agent, run).is_none());
        if index == 0 {
            first = Some(accepted.clone());
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
    if retained_fixture::run_isolated() {
        return;
    }
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
    // Wait for the actionable line that ends the console's request block, then
    // assert the choices it refers to.
    let displayed = fixture.cli(&[
        "terminal",
        "wait",
        terminal,
        "--text",
        "Use /answer console-approval",
        "--timeout",
        "10",
    ]);
    let screen = displayed["snapshot"]["text"].as_str().unwrap();
    assert!(
        screen.contains("Actions: allow (Allow once); deny (Deny)"),
        "actions in {displayed}"
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
        "Finished (7 input, 3 output tokens)",
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

/// The shipped daemon and console over a provider that streams its answer and
/// runs tools with large outputs: the pane shows what the agent said and one
/// line per step, each once, and none of what those steps carried.
#[test]
fn terminal_console_prints_each_line_once_without_tool_payloads() {
    if retained_fixture::run_isolated() {
        return;
    }
    let fixture = Fixture::with_provider(include_str!("fixtures/console-transcript-claude.sh"));
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
        "console-transcript",
    ]);
    let agent = accepted["agent"]["id"].as_str().unwrap();
    let run = accepted["run"]["id"].as_str().unwrap();
    let finished = fixture.wait(&["agent", "wait", run, "--timeout", "30"], 0);
    assert_eq!(finished["run"]["state"], "Succeeded", "{finished}");
    let terminal = finished["agent"]["console"]["id"].as_str().unwrap();
    let displayed = fixture.cli(&[
        "terminal",
        "wait",
        terminal,
        "--text",
        "Finished",
        "--timeout",
        "10",
    ]);
    let text = displayed["snapshot"]["text"].as_str().unwrap();
    for once in [
        "Assistant: Looking at the failing test.",
        "Ran cargo test --locked (failed)",
        "Called Agent",
        "Assistant: The second paragraph follows.",
    ] {
        assert_eq!(text.matches(once).count(), 1, "{once}: {text}");
    }
    for hidden in [
        "PAYLOAD_OUTPUT",
        "PAYLOAD_INPUT",
        "Current transcript",
        "\"input_tokens\"",
        run,
        agent,
    ] {
        assert!(!text.contains(hidden), "{hidden}: {text}");
    }
    fixture.cli(&["agent", "close", agent]);
}

#[test]
fn retained_agent_closes_during_a_delayed_fixture_stream() {
    if retained_fixture::run_isolated() {
        return;
    }
    use cagents::{
        chat::{ItemContent, NativeContinuity},
        control::ControlClient,
        terminal::ipc::{
            ClientEnvelope, ClientRequest, PROTOCOL_VERSION, ServerEnvelope, ServerMessage,
            ServerResponse, read_frame, write_frame,
        },
    };
    let fixture = Fixture::with_provider(include_str!("fixtures/chat-composer-claude.sh"));
    let config = fixture.root.path().join("home/.claude");
    let reply = "Offline background update for responsiveness comparison.\n".repeat(40000);
    assert_eq!(reply.len(), 2_280_000);
    fs::write(config.join("fixture-reply.md"), &reply).unwrap();
    fs::write(config.join("fixture-stream-reply"), "0.5\n").unwrap();
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
        "Stream the offline background fixture.",
        "--idempotency-key",
        "close-delayed-stream",
    ]);
    let agent = accepted["agent"]["id"].as_str().unwrap();
    let status = fixture.cli(&["daemon", "status"]);
    let socket = Path::new(status["socket"].as_str().unwrap());
    let mut inventory = ControlClient::connect(socket).unwrap();
    let session = inventory.list_chats().unwrap().into_iter().next().unwrap();
    assert!(matches!(
        &session.native_continuity,
        NativeContinuity::RetainedClaudeActor { .. }
    ));
    let chat_id = session.id;
    let mut observer = std::os::unix::net::UnixStream::connect(socket).unwrap();
    observer
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    observer
        .set_write_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut proof = None;
    let mut sequence = 0;
    let mut request = |value| {
        sequence += 1;
        write_frame(
            &mut observer,
            &ClientEnvelope {
                version: PROTOCOL_VERSION,
                request_id: sequence,
                proof: proof.clone(),
                request: value,
            },
        )
        .unwrap();
        loop {
            let envelope: ServerEnvelope = read_frame(&mut observer).unwrap().unwrap();
            if let Some(expected) = &proof {
                assert_eq!(&envelope.proof, expected);
            } else {
                proof = Some(envelope.proof.clone());
            }
            if envelope.request_id == Some(sequence) {
                let ServerMessage::Response { response } = envelope.message else {
                    panic!("unexpected private daemon response");
                };
                break response;
            }
        }
    };
    assert!(matches!(
        request(ClientRequest::Connect {
            expected_host: None,
            resume: Vec::new(),
        }),
        ServerResponse::Connected { .. }
    ));
    assert!(matches!(
        request(ClientRequest::ChatAttach { session }),
        ServerResponse::ChatAttached { .. }
    ));
    assert!(matches!(
        request(ClientRequest::ChatVisibility {
            chat_id,
            visible: true,
        }),
        ServerResponse::Accepted
    ));
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let ServerResponse::ChatSnapshot { snapshot, .. } = request(ClientRequest::ChatRead {
            chat_id,
            after_generation: None,
        }) else {
            panic!("expected the actual retained actor snapshot");
        };
        let snapshot = snapshot.unwrap();
        if let Some(text) = snapshot.items.iter().find_map(|item| match &item.content {
            ItemContent::Text { text, .. } if text.starts_with("Offline background update") => {
                Some(text)
            }
            _ => None,
        }) {
            assert!(snapshot.busy, "close must start during the active stream");
            assert!(text.len() < reply.len());
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the actor never received a delta"
        );
        thread::sleep(Duration::from_millis(10));
    }
    let began = Instant::now();
    let closed = fixture.cli(&["agent", "close", agent]);
    assert_eq!(closed["agent"]["closed"], true);
    assert!(
        began.elapsed() < Duration::from_secs(10),
        "close must serve control, not wait for the roughly 70-second reply"
    );
    assert!(inventory.list_chats().unwrap().is_empty());
    assert!(
        fs::read_dir(config).unwrap().all(|entry| !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".fixture-stream.")),
        "provider teardown must remove its private stream state"
    );
}

#[test]
fn repeated_private_cli_capture_does_not_renew_original_deadline() {
    let root = tempfile::tempdir().unwrap();
    let command = |script: &str| {
        let mut spec = private_cli_capture::private_cli(root.path());
        spec.program = "/bin/sh".into();
        spec.arguments.clear();
        spec.args(["-c", script])
    };
    let deadline = Instant::now() + Duration::from_secs(4);
    let output =
        private_cli_capture::capture(command("sleep 2; printf first-poll"), deadline).unwrap();
    assert!(output.status.success());
    assert_eq!(output.stdout, b"first-poll");

    // This finite command fits a renewed four-second budget, but cannot
    // finish in what remains after the first poll. Unlike a 300-second
    // stall, it distinguishes a renewed budget from the original deadline.
    let failure = private_cli_capture::capture(command("printf next-poll; exec sleep 3"), deadline)
        .unwrap_err();
    assert_eq!(failure.kind, cagents::process::ProcessFailureKind::TimedOut);
    assert!(failure.started, "first poll exhausted the fixture budget");
    assert_eq!(failure.stdout, b"next-poll");
}

#[test]
fn failed_console_publication_retires_terminal_and_retains_idle_provider() {
    if retained_fixture::run_isolated() {
        return;
    }
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
    if retained_fixture::run_isolated() {
        return;
    }
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
    if retained_fixture::run_isolated() {
        return;
    }
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
    if retained_fixture::run_isolated() {
        return;
    }
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
    if retained_fixture::run_isolated() {
        return;
    }
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
