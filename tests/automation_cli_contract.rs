#![cfg(feature = "host")]

use serde_json::{Value, json};
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::Path,
    process::{Child, Command, Output, Stdio},
    thread,
    time::{Duration, Instant},
};

struct Fixture {
    root: tempfile::TempDir,
    daemon: Child,
}

impl Fixture {
    fn command(root: &Path, program: &str) -> Command {
        let mut command = Command::new(program);
        command
            .env_clear()
            .env("HOME", root.join("home"))
            .env("XDG_RUNTIME_DIR", root.join("run"))
            .env("XDG_DATA_HOME", root.join("data"))
            .env("XDG_CONFIG_HOME", root.join("config"))
            .env("XDG_CACHE_HOME", root.join("cache"))
            .env("TMPDIR", root.join("tmp"))
            .env(
                "PATH",
                format!("{}:/usr/bin:/bin", root.join("bin").display()),
            )
            .current_dir(root)
            .stdin(Stdio::null());
        command
    }

    fn start(root: &Path) -> Child {
        let log = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(root.join("daemon.log"))
            .unwrap();
        Self::command(root, env!("CARGO_BIN_EXE_cagentsd"))
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .spawn()
            .unwrap()
    }

    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        for name in [
            "home", "run", "data", "config", "cache", "tmp", "project", "bin",
        ] {
            fs::create_dir(root.path().join(name)).unwrap();
        }
        fs::set_permissions(root.path().join("run"), fs::Permissions::from_mode(0o700)).unwrap();
        let daemon = Self::start(root.path());
        let mut fixture = Self { root, daemon };
        fixture.ready();
        fixture
    }

    fn ready_status(response: &Value) -> bool {
        // `running` includes a live process that has not bound its endpoint.
        // A version reply proves both the control handshake and a request.
        response["data"]["running"] == true
            && response["data"]["daemon_version"]
                .as_str()
                .is_some_and(|version| !version.is_empty())
    }

    fn ready(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            self.assert_daemon_alive();
            let output = self.output_until(&["daemon", "status"], deadline);
            self.assert_daemon_alive();
            if output.status.success()
                && serde_json::from_slice::<Value>(&output.stdout)
                    .is_ok_and(|v| Self::ready_status(&v))
            {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "daemon failed: {}",
                fs::read_to_string(self.root.path().join("daemon.log")).unwrap()
            );
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn assert_daemon_alive(&mut self) {
        assert!(
            self.daemon.try_wait().unwrap().is_none(),
            "fixture daemon exited before readiness: {}",
            fs::read_to_string(self.root.path().join("daemon.log")).unwrap()
        );
    }

    fn output(&self, arguments: &[&str]) -> Output {
        self.output_until(arguments, Instant::now() + Duration::from_secs(20))
    }

    fn output_until(&self, arguments: &[&str], deadline: Instant) -> Output {
        let mut child = Self::command(self.root.path(), env!("CARGO_BIN_EXE_cagents"))
            .arg("--json")
            .args(arguments)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        while child.try_wait().unwrap().is_none() {
            if Instant::now() >= deadline {
                let _ = child.kill();
                panic!(
                    "CLI timed out: {arguments:?}: {:?}",
                    child.wait_with_output()
                );
            }
            thread::sleep(Duration::from_millis(10));
        }
        child.wait_with_output().unwrap()
    }

    fn cli(&self, arguments: &[&str]) -> Value {
        let output = self.output(arguments);
        assert!(output.status.success(), "{arguments:?}: {output:?}");
        let response: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(response["ok"], true, "{response}");
        response["data"].clone()
    }

    fn restart(&mut self) {
        assert!(
            self.output(&["daemon", "stop", "--require-empty"])
                .status
                .success()
        );
        self.daemon.wait().unwrap();
        self.daemon = Self::start(self.root.path());
        self.ready();
    }
}

#[test]
fn a_live_process_without_a_control_reply_is_not_ready() {
    // This is the status response during the real pre-bind startup window.
    let mut status = json!({"ok":true,"data":{"running":true,"daemon_version":null}});
    assert!(!Fixture::ready_status(&status));
    status["data"]["daemon_version"] = "verified-daemon-build".into();
    assert!(Fixture::ready_status(&status));
    status["data"]["running"] = false.into();
    assert!(!Fixture::ready_status(&status));
}

#[test]
fn enabled_automation_dispatches_without_a_window_and_restart_does_not_repeat_it() {
    let mut fixture = Fixture::new();
    let project = fixture.root.path().join("project");
    fixture.cli(&["project", "open", project.to_str().unwrap()]);
    let provider = fixture.root.path().join("bin/claude");
    fs::write(&provider, include_str!("fixtures/automation-claude.sh")).unwrap();
    fs::set_permissions(&provider, fs::Permissions::from_mode(0o700)).unwrap();
    fs::create_dir(fixture.root.path().join("home/.claude")).unwrap();
    let now = jiff::Timestamp::now().to_zoned(jiff::tz::TimeZone::UTC);
    let minute = now.timestamp().as_millisecond() as u64 / 60_000 * 60_000;
    let local = now.datetime().strftime("%Y-%m-%dT%H:%M").to_string();
    let definition = json!({
        "name": "Scheduled provider fixture", "prompt": "Return a fixture result",
        "agent": {"provider": "Claude", "account": null,
            "target": {"repository": project, "directory": project, "channel": null},
            "surface": "Chat", "model": null, "permission_mode": null},
        "schedule": {"pattern": {"daily": {"hour": now.hour(), "minute": now.minute()}}, "timezone": "UTC"},
        "session_mode": "fresh", "visibility": "headless", "precheck": null
    });
    let path = fixture.root.path().join("scheduled.json");
    fs::write(&path, serde_json::to_vec(&definition).unwrap()).unwrap();
    let created = fixture.cli(&[
        "automation",
        "create",
        "--definition",
        path.to_str().unwrap(),
    ]);
    let id = created["automation"]["id"].as_str().unwrap().to_owned();
    let revision = created["automation"]["revision"]
        .as_u64()
        .unwrap()
        .to_string();
    let enabled = fixture.cli(&[
        "automation",
        "enable",
        &id,
        "--revision",
        &revision,
        "--confirm",
    ]);
    assert_eq!(enabled["automation"]["enabled"], true);
    assert!(
        fixture
            .output(&["daemon", "stop", "--require-empty"])
            .status
            .success()
    );
    fixture.daemon.wait().unwrap();
    // Simulate a persisted deadline that became due while the daemon was
    // absent. Only its clock cursor changes; the CLI already recorded consent
    // for this exact target/account/schedule. No wall-clock minute wait.
    let database = fixture
        .root
        .path()
        .join("data")
        .join(cagents::instance::STORAGE_NAME)
        .join("workspace-model.sqlite3");
    let connection = rusqlite::Connection::open(database).unwrap();
    let next = json!({"due_ms": minute, "local": local}).to_string();
    connection.execute("UPDATE automations SET next_due_ms=?2, record=json_set(record,'$.next',json(?3)), summary=json_set(summary,'$.next',json(?3)) WHERE id=?1",
        rusqlite::params![id, minute, next]).unwrap();
    drop(connection);
    fixture.daemon = Fixture::start(fixture.root.path());
    fixture.ready();
    let deadline = Instant::now() + Duration::from_secs(15);
    let firing = loop {
        let history = fixture.cli(&["automation", "history", &id]);
        let entries = history["history"]["firings"].as_array().unwrap();
        if let Some(firing) = entries.first() {
            if firing["state"] == "succeeded" {
                let shown = fixture.cli(&["automation", "show", &id]);
                if shown["automation"]["active_firing"].is_null() {
                    break firing["id"].as_str().unwrap().to_owned();
                }
            }
            assert!(
                !matches!(firing["state"].as_str(), Some("failed" | "interrupted")),
                "scheduled run failed: {history}"
            );
        }
        assert!(
            Instant::now() < deadline,
            "scheduled run did not settle: {history}"
        );
        thread::sleep(Duration::from_millis(50));
    };
    let detail = fixture.cli(&["automation", "run", &firing]);
    assert!(
        detail["run"]["run"]["result"]["text"]
            .as_str()
            .unwrap()
            .contains("SCHEDULED_RESULT")
    );
    assert_eq!(
        detail["run"]["agent"]["closed"], true,
        "fresh session cleanup is proved before releasing its slot"
    );
    let target = cagents::notifications::Target::AgentRun(
        detail["run"]["run"]["id"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap(),
    );
    let socket = fixture
        .root
        .path()
        .join("run")
        .join(cagents::instance::RUNTIME_SUBDIRECTORY)
        .join(cagents::terminal_ipc::SOCKET_FILE_NAME);
    let mut client = cagents::control::ControlClient::connect(&socket).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    let notice = loop {
        let snapshot = client.read_notifications().unwrap();
        if let Some(notice) = snapshot
            .notices
            .iter()
            .find(|notice| notice.event.target() == target)
        {
            assert_eq!(
                snapshot.notices.len(),
                1,
                "provider evidence must not create a duplicate session alert"
            );
            break notice.clone();
        }
        assert!(
            Instant::now() < deadline,
            "run attention was not journaled: {snapshot:?}"
        );
        thread::sleep(Duration::from_millis(20));
    };
    client.acknowledge_notification(notice.id).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !client
        .read_notifications()
        .unwrap()
        .notices
        .iter()
        .any(|current| current.id == notice.id && current.acknowledged)
    {
        assert!(
            Instant::now() < deadline,
            "acknowledgement was not journaled"
        );
        thread::sleep(Duration::from_millis(20));
    }
    drop(client);
    fixture.restart();
    let mut client = cagents::control::ControlClient::connect(&socket).unwrap();
    // The daemon accepts connections before its notification broker finishes
    // restoring the journal. Observe that asynchronous restoration explicitly.
    let deadline = Instant::now() + Duration::from_secs(5);
    let snapshot = loop {
        let snapshot = client.read_notifications().unwrap();
        if snapshot
            .notices
            .iter()
            .any(|current| current.id == notice.id)
        {
            break snapshot;
        }
        assert!(
            Instant::now() < deadline,
            "run attention was not restored: {snapshot:?}"
        );
        thread::sleep(Duration::from_millis(20));
    };
    assert_eq!(snapshot.notices.len(), 1);
    assert_eq!(snapshot.notices[0].id, notice.id);
    assert_eq!(snapshot.notices[0].event.target(), target);
    assert!(snapshot.notices[0].acknowledged);
    assert_eq!(
        fixture.cli(&["automation", "history", &id])["history"]["firings"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        fs::read_to_string(fixture.root.path().join("prompts")).unwrap(),
        "prompt\n"
    );
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.daemon.kill();
        let _ = self.daemon.wait();
    }
}

#[test]
fn automation_cli_runs_headless_precheck_once_and_keeps_history_after_restart_and_removal() {
    let mut fixture = Fixture::new();
    let project = fixture.root.path().join("project");
    fixture.cli(&["project", "open", project.to_str().unwrap()]);
    let definition = json!({
        "name": "Headless fixture", "prompt": "must never launch a provider",
        "agent": { "provider": "Omp", "account": null,
            "target": {"repository": project, "directory": project, "channel": null},
            "surface": "Chat", "model": null, "permission_mode": null },
        "schedule": {"pattern": {"daily": {"hour": 3, "minute": 0}}, "timezone": "UTC"},
        "session_mode": "fresh",
        "precheck": {"command": "printf 'checked\\n' >> observations; exit 1", "timeout_seconds": 2}
    });
    let path = fixture.root.path().join("definition.json");
    fs::write(&path, serde_json::to_vec(&definition).unwrap()).unwrap();
    let created = fixture.cli(&[
        "automation",
        "create",
        "--definition",
        path.to_str().unwrap(),
    ]);
    let id = created["automation"]["id"].as_str().unwrap().to_owned();
    assert_eq!(created["automation"]["enabled"], false);
    let revision = created["automation"]["execution_revision"]
        .as_u64()
        .unwrap()
        .to_string();
    let retry = uuid::Uuid::new_v4().to_string();
    let arguments = [
        "automation",
        "run-now",
        &id,
        "--execution-revision",
        &revision,
        "--request-id",
        &retry,
    ];
    assert!(
        !fixture.output(&arguments).status.success(),
        "unconfirmed run must fail"
    );
    assert!(!project.join("observations").exists());
    let mut confirmed = arguments.to_vec();
    confirmed.push("--confirm");
    let accepted = fixture.cli(&confirmed);
    let firing = accepted["firing"]["id"].as_str().unwrap().to_owned();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let detail = fixture.cli(&["automation", "run", &firing]);
        if detail["run"]["firing"]["state"] == "skipped" {
            assert!(
                detail["run"]["run"].is_null(),
                "false precheck must not accept an agent prompt"
            );
            break;
        }
        assert!(
            Instant::now() < deadline,
            "precheck did not settle: {detail}"
        );
        thread::sleep(Duration::from_millis(50));
    }
    fixture.restart();
    assert_eq!(fixture.cli(&confirmed)["firing"]["id"], firing);
    assert_eq!(
        fs::read_to_string(project.join("observations")).unwrap(),
        "checked\n"
    );
    let shown = fixture.cli(&["automation", "show", &id]);
    let revision = shown["automation"]["revision"]
        .as_u64()
        .unwrap()
        .to_string();
    fixture.cli(&["automation", "remove", &id, "--revision", &revision]);
    assert_eq!(
        fixture.cli(&["automation", "list"])["automations"],
        json!([])
    );
    assert_eq!(
        fixture.cli(&["automation", "history", &id])["history"]["firings"][0]["id"],
        firing
    );
    let history = fixture.cli(&["automation", "history"]);
    assert_eq!(history["history"]["firings"][0]["id"], firing);
    assert!(
        history["history"]["firings"][0]["name"] == "Headless fixture",
        "removed definitions retain their historical names"
    );
    fs::remove_dir_all(&project).unwrap();
    assert_eq!(
        fixture.cli(&confirmed)["firing"]["id"],
        firing,
        "receipt lookup survives target and definition removal without new work"
    );
    let changed = [
        "automation",
        "run-now",
        &id,
        "--execution-revision",
        "999",
        "--request-id",
        &retry,
        "--confirm",
    ];
    assert!(
        !fixture.output(&changed).status.success(),
        "retry identity cannot authorize another revision"
    );
}

#[test]
fn more_than_sixty_four_headfull_fresh_runs_retire_providers_and_keep_lazy_archives() {
    let fixture = Fixture::new();
    let project = fixture.root.path().join("project");
    fixture.cli(&["project", "open", project.to_str().unwrap()]);
    let provider = fixture.root.path().join("bin/claude");
    let source = format!(
        "{}\n{}",
        "#!/bin/sh\nprintf 'spawn\\n' >> \"$HOME/../spawns\"\nif [ -r /proc/$$/stat ]; then printf '%s ' \"$$\" >> \"$HOME/../provider-pids\"\nawk '{print $22}' /proc/$$/stat >> \"$HOME/../provider-pids\"; fi",
        include_str!("fixtures/automation-claude.sh").trim_start_matches("#!/bin/sh\n")
    );
    fs::write(&provider, source).unwrap();
    fs::set_permissions(&provider, fs::Permissions::from_mode(0o700)).unwrap();
    fs::create_dir(fixture.root.path().join("home/.claude")).unwrap();
    let definition = json!({
        "name":"Fresh visible archive fixture", "prompt":"Return a fixture result",
        "agent":{"provider":"Claude","account":null,
            "target":{"repository":project,"directory":project,"channel":null},
            "surface":"Chat","model":null,"permission_mode":null},
        "schedule":{"pattern":{"daily":{"hour":3,"minute":0}},"timezone":"UTC"},
        "session_mode":"fresh", "precheck":null
    });
    let path = fixture.root.path().join("definition.json");
    fs::write(&path, serde_json::to_vec(&definition).unwrap()).unwrap();
    let created = fixture.cli(&[
        "automation",
        "create",
        "--definition",
        path.to_str().unwrap(),
    ]);
    assert_eq!(
        created["automation"]["definition"]["visibility"],
        "headfull"
    );
    let id = created["automation"]["id"].as_str().unwrap();
    let revision = created["automation"]["execution_revision"]
        .as_u64()
        .unwrap()
        .to_string();
    let socket = fixture
        .root
        .path()
        .join("run")
        .join(cagents::instance::RUNTIME_SUBDIRECTORY)
        .join(cagents::terminal_ipc::SOCKET_FILE_NAME);
    let mut client = cagents::control::ControlClient::connect(&socket).unwrap();
    let mut last_agent = None;
    for index in 0..70 {
        let request = uuid::Uuid::new_v4().to_string();
        let accepted = fixture.cli(&[
            "automation",
            "run-now",
            id,
            "--execution-revision",
            &revision,
            "--request-id",
            &request,
            "--confirm",
        ]);
        let firing = accepted["firing"]["id"].as_str().unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        let detail = loop {
            let detail = fixture.cli(&["automation", "run", firing]);
            let shown = fixture.cli(&["automation", "show", id]);
            if detail["run"]["firing"]["state"] == "succeeded"
                && shown["automation"]["active_firing"].is_null()
            {
                break detail;
            }
            assert!(
                Instant::now() < deadline,
                "fresh visible run {index} did not settle: {detail}"
            );
            thread::sleep(Duration::from_millis(20));
        };
        assert_eq!(
            detail["run"]["agent"]["closed"], true,
            "completed providers must release child capacity"
        );
        assert!(
            client.list_chats().unwrap().is_empty(),
            "no hosted provider may accumulate after fresh completion"
        );
        #[cfg(target_os = "linux")]
        {
            let identities = fs::read_to_string(fixture.root.path().join("provider-pids")).unwrap();
            let latest = identities
                .lines()
                .last()
                .unwrap()
                .split_whitespace()
                .collect::<Vec<_>>();
            assert_eq!(latest.len(), 2);
            if let Ok(stat) = fs::read_to_string(format!("/proc/{}/stat", latest[0])) {
                let observed = stat
                    .rsplit_once(')')
                    .unwrap()
                    .1
                    .split_whitespace()
                    .nth(19)
                    .unwrap();
                assert_ne!(
                    observed, latest[1],
                    "the exact completed provider process must have exited, even if its PID was subsequently reused"
                );
            }
        }
        last_agent = Some(
            detail["run"]["agent"]["id"]
                .as_str()
                .unwrap()
                .parse()
                .unwrap(),
        );
    }
    let agent = last_agent.unwrap();
    let before = fs::read_to_string(fixture.root.path().join("prompts")).unwrap();
    let archive = client
        .automation(
            cagents::agent_execution::Authority::default(),
            cagents::automations::Request::ArchivedConversation { agent },
        )
        .unwrap();
    let cagents::automations::Response::ArchivedConversation(archive) = archive else {
        panic!("expected exact archive")
    };
    assert!(archive.agent.closed);
    assert!(!archive.snapshot.ready && archive.snapshot.exited);
    assert!(archive.snapshot.read_only_reason.is_some());
    assert!(
        archive
            .snapshot
            .items
            .iter()
            .any(|item| matches!(&item.content,
        cagents::chat::ItemContent::Text {text,..} if text.contains("SCHEDULED_RESULT")))
    );
    assert!(
        client.list_chats().unwrap().is_empty(),
        "reading an archive must not create a replacement provider"
    );
    assert_eq!(
        fs::read_to_string(fixture.root.path().join("prompts")).unwrap(),
        before
    );
    assert_eq!(before.lines().count(), 70);
    assert_eq!(
        fs::read_to_string(fixture.root.path().join("spawns"))
            .unwrap()
            .lines()
            .count(),
        70
    );
}

#[test]
fn actual_execution_console_roundtrips_to_the_same_saved_pane_without_provider_replay() {
    use cagents::{
        agent_execution::{Agent, HostedSession},
        model::{
            BootId, CardId, CardKind, CardState, Existence, Exposure, Residency, WorkspaceState,
        },
        persistence::{SnapshotBase, StateStore},
    };
    let fixture = Fixture::new();
    let project = fixture.root.path().join("project");
    fixture.cli(&["project", "open", project.to_str().unwrap()]);
    let provider = fixture.root.path().join("bin/claude");
    fs::write(&provider, include_str!("fixtures/automation-claude.sh")).unwrap();
    fs::set_permissions(&provider, fs::Permissions::from_mode(0o700)).unwrap();
    fs::create_dir(fixture.root.path().join("home/.claude")).unwrap();
    let definition = json!({"name":"Actual console archive","prompt":"Return a fixture result","agent":{"provider":"Claude","account":null,"target":{"repository":project,"directory":project,"placement":null},"surface":"Terminal","model":null,"permission_mode":null},"schedule":{"pattern":{"daily":{"hour":3,"minute":0}},"timezone":"UTC"},"session_mode":"fresh","visibility":"headfull","precheck":null});
    let path = fixture.root.path().join("terminal.json");
    fs::write(&path, serde_json::to_vec(&definition).unwrap()).unwrap();
    let created = fixture.cli(&[
        "automation",
        "create",
        "--definition",
        path.to_str().unwrap(),
    ]);
    let id = created["automation"]["id"].as_str().unwrap();
    let revision = created["automation"]["execution_revision"]
        .as_u64()
        .unwrap()
        .to_string();
    let request = uuid::Uuid::new_v4().to_string();
    let accepted = fixture.cli(&[
        "automation",
        "run-now",
        id,
        "--execution-revision",
        &revision,
        "--request-id",
        &request,
        "--confirm",
    ]);
    let firing = accepted["firing"]["id"].as_str().unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    let _detail = loop {
        let detail = fixture.cli(&["automation", "run", firing]);
        let shown = fixture.cli(&["automation", "show", id]);
        if detail["run"]["firing"]["state"] == "succeeded"
            && shown["automation"]["active_firing"].is_null()
        {
            break detail;
        }
        assert!(
            Instant::now() < deadline,
            "actual console did not settle: {detail}"
        );
        thread::sleep(Duration::from_millis(20));
    };
    let socket = fixture
        .root
        .path()
        .join("run")
        .join(cagents::instance::RUNTIME_SUBDIRECTORY)
        .join(cagents::terminal_ipc::SOCKET_FILE_NAME);
    let mut client = cagents::control::ControlClient::connect(&socket).unwrap();
    let cagents::automations::Response::Detail(saved) = client
        .automation(
            cagents::agent_execution::Authority::default(),
            cagents::automations::Request::ShowFiring {
                id: firing.parse().unwrap(),
            },
        )
        .unwrap()
    else {
        panic!("typed run receipt")
    };
    let agent: Agent = saved.agent.unwrap();
    assert!(agent.closed);
    let console = agent
        .console
        .clone()
        .expect("actual create_console descriptor");
    let Some(HostedSession::Chat(mut session)) = agent.session.clone() else {
        panic!("original native provider descriptor")
    };
    assert_eq!(console.id, session.id);
    assert_eq!(
        console.launch.arguments,
        vec![
            "agent".to_owned(),
            "console".to_owned(),
            agent.id.to_string()
        ]
    );
    let card = CardId::from_uuid(agent.id.as_uuid());
    let store = StateStore::open(
        fixture.root.path().join("pane.sqlite3"),
        BootId::new("console-fixture"),
    )
    .unwrap();
    let mut workspace = WorkspaceState::default();
    workspace.cards.push(CardState {
        id: card,
        generation: 0,
        title: "Scheduled console".into(),
        width_fraction: 0.6,
        existence: Existence::Open,
        exposure: Exposure::Visible,
        residency: Residency::Cold,
        kind: CardKind::Terminal(console.clone()),
    });
    workspace.focused = Some(card);
    let first = store
        .save_workspace(
            &workspace,
            SnapshotBase {
                workspace: 0,
                cards: &Default::default(),
            },
        )
        .unwrap();
    session.terminal = Some(console);
    workspace.cards[0].kind = CardKind::Chat(session.clone());
    workspace.automation_archives.insert(card, agent.id);
    store
        .save_workspace(
            &workspace,
            SnapshotBase {
                workspace: first.generation,
                cards: &first.card_generations,
            },
        )
        .unwrap();
    let restored = store.load_workspace(workspace.id, None).unwrap().unwrap();
    assert_eq!(restored.cards[0].id, card);
    assert_eq!(restored.cards[0].kind, CardKind::Chat(session.clone()));
    let before = fs::read_to_string(fixture.root.path().join("prompts")).unwrap();
    for _ in 0..2 {
        let cagents::automations::Response::ArchivedConversation(archive) = client
            .automation(
                cagents::agent_execution::Authority::default(),
                cagents::automations::Request::ArchivedConversation { agent: agent.id },
            )
            .unwrap()
        else {
            panic!("saved view")
        };
        assert!(
            cagents::automations::archive::ArchivedPaneReadiness::qualify(
                agent.id, &session, &archive
            )
            .unwrap()
            .matches(agent.id, &session)
        );
    }
    assert!(client.list_chats().unwrap().is_empty());
    assert_eq!(
        before,
        fs::read_to_string(fixture.root.path().join("prompts")).unwrap()
    );
    assert_eq!(before.lines().count(), 1);
}

#[test]
fn delayed_native_confirmation_archives_both_cold_source_aliases_without_a_provider_restart() {
    use cagents::{
        agent_execution::HostedSession,
        automations::{Request, Response, archive::inspect_completed_automation_pane},
        chat::NativeContinuity,
        model::{
            BootId, CardId, CardKind, CardState, Existence, Exposure, Residency, WorkspaceState,
        },
        persistence::{SnapshotBase, StateStore},
    };
    const ROLE: &str = "CAGENTS_DELAYED_ARCHIVE_TEST_ROLE";
    const TEST: &str =
        "delayed_native_confirmation_archives_both_cold_source_aliases_without_a_provider_restart";
    let role = std::env::var(ROLE).unwrap_or_default();
    if role.is_empty() {
        // Keep process-wide subreaper state out of the ordinary test runner.
        for mode in ["controller", "controller-failure"] {
            let status = Command::new(std::env::current_exe().unwrap())
                .args(["--exact", TEST, "--nocapture"])
                .env(ROLE, mode)
                .status()
                .unwrap();
            assert!(
                status.success(),
                "isolated archive fixture/{mode} failed: {status}"
            );
        }
        return;
    }
    if matches!(role.as_str(), "controller" | "controller-failure") {
        let expected_failure = role == "controller-failure";
        let controller_root = tempfile::tempdir().unwrap();
        let failure_receipt = controller_root.path().join("confirmed-failure.json");
        struct Family {
            settled: bool,
            cleanup_deadline: Option<Instant>,
        }
        impl Family {
            fn identity(pid: i32) -> std::io::Result<(u32, String)> {
                let stat = fs::read_to_string(format!("/proc/{pid}/stat"))?;
                let fields: Vec<_> = stat
                    .rsplit_once(')')
                    .ok_or_else(|| std::io::Error::other("invalid child stat"))?
                    .1
                    .split_whitespace()
                    .collect();
                Ok((
                    fields
                        .get(1)
                        .ok_or_else(|| std::io::Error::other("missing child parent"))?
                        .parse()
                        .map_err(std::io::Error::other)?,
                    fields
                        .get(19)
                        .ok_or_else(|| std::io::Error::other("missing child start"))?
                        .to_string(),
                ))
            }
            fn kill_direct(pid: i32) -> std::io::Result<()> {
                use std::os::fd::{AsRawFd, FromRawFd};
                let before = match Self::identity(pid) {
                    Ok(value) => value,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
                    Err(error) => return Err(error),
                };
                if before.0 != std::process::id() {
                    return Err(std::io::Error::other("fixture child parent changed"));
                }
                let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
                if raw < 0 {
                    let error = std::io::Error::last_os_error();
                    return if error.raw_os_error() == Some(libc::ESRCH) {
                        Ok(())
                    } else {
                        Err(error)
                    };
                }
                // SAFETY: pidfd_open returned one newly owned descriptor.
                let fd = unsafe { std::os::fd::OwnedFd::from_raw_fd(raw as i32) };
                match Self::identity(pid) {
                    Ok(after) if after == before => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
                    Ok(_) => {
                        return Err(std::io::Error::other(
                            "fixture child identity changed after pinning",
                        ));
                    }
                    Err(error) => return Err(error),
                }
                // SAFETY: the pidfd pins the start/parent-checked direct child.
                let sent = unsafe {
                    libc::syscall(
                        libc::SYS_pidfd_send_signal,
                        fd.as_raw_fd(),
                        libc::SIGKILL,
                        std::ptr::null::<libc::siginfo_t>(),
                        0,
                    )
                };
                if sent < 0 {
                    let error = std::io::Error::last_os_error();
                    if error.raw_os_error() != Some(libc::ESRCH) {
                        return Err(error);
                    }
                }
                Ok(())
            }
            fn finish(&mut self) -> Result<(), String> {
                if self.settled {
                    return Ok(());
                }
                let deadline = *self
                    .cleanup_deadline
                    .get_or_insert_with(|| Instant::now() + Duration::from_secs(5));
                loop {
                    loop {
                        if Instant::now() >= deadline {
                            return Err("fixture family did not settle before its deadline".into());
                        }
                        let mut status = 0;
                        let reaped = unsafe { libc::waitpid(-1, &mut status, libc::WNOHANG) };
                        if reaped > 0 {
                            eprintln!(
                                "archive fixture reaped child {reaped}: wait status {status}"
                            );
                            continue;
                        }
                        if reaped == 0 {
                            break;
                        }
                        let error = std::io::Error::last_os_error();
                        if error.raw_os_error() == Some(libc::ECHILD) {
                            self.settled = true;
                            eprintln!("archive fixture family settled: kernel ECHILD");
                            return Ok(());
                        }
                        if error.kind() != std::io::ErrorKind::Interrupted {
                            return Err(format!("fixture waitpid failed: {error}"));
                        }
                    }
                    if Instant::now() >= deadline {
                        return Err("fixture family did not settle before its deadline".into());
                    }
                    let mut children = std::collections::BTreeSet::new();
                    for task in fs::read_dir("/proc/self/task")
                        .map_err(|error| format!("fixture task census failed: {error}"))?
                    {
                        let task = task.map_err(|error| error.to_string())?;
                        let members = match fs::read_to_string(task.path().join("children")) {
                            Ok(value) => value,
                            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                            Err(error) => {
                                return Err(format!("fixture child census failed: {error}"));
                            }
                        };
                        for member in members.split_whitespace() {
                            children
                                .insert(member.parse::<i32>().map_err(|error| error.to_string())?);
                        }
                    }
                    for child in children {
                        Self::kill_direct(child).map_err(|error| {
                            format!("cannot retire exact fixture child {child}: {error}")
                        })?;
                    }
                    thread::sleep(Duration::from_millis(10));
                }
            }
        }
        impl Drop for Family {
            fn drop(&mut self) {
                if let Err(error) = self.finish() {
                    eprintln!("archive fixture cleanup FAILED: {error}");
                    if !std::thread::panicking() {
                        panic!("{error}");
                    }
                }
            }
        }
        // Every orphaned fixture holder/provider remains a descendant of this
        // dedicated controller, even after it leaves the worker's session.
        assert_eq!(
            unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) },
            0
        );
        let mut family = Family {
            settled: false,
            cleanup_deadline: None,
        };
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args(["--exact", TEST, "--nocapture"])
            .env(ROLE, "worker")
            .env_remove("CAGENTS_ARCHIVE_FAILURE_RECEIPT");
        if expected_failure {
            command.env("CAGENTS_ARCHIVE_FAILURE_RECEIPT", &failure_receipt);
        }
        let mut worker = command.spawn().unwrap();
        let deadline = Instant::now() + Duration::from_secs(90);
        let outcome = loop {
            if let Some(status) = worker.try_wait().unwrap() {
                break Some(status);
            }
            if Instant::now() >= deadline {
                Family::kill_direct(worker.id() as i32).unwrap();
                break None;
            }
            thread::sleep(Duration::from_millis(10));
        };
        eprintln!("archive fixture worker outcome: {outcome:?}");
        family.finish().unwrap();
        if expected_failure {
            let receipt: Value = serde_json::from_slice(
                &fs::read(&failure_receipt)
                    .expect("expected failure must reach actual native confirmation"),
            )
            .unwrap();
            assert_eq!(receipt["point"], "after-native-confirmation");
            assert_eq!(receipt["provider_version"], "0.162.0");
            assert!(uuid::Uuid::parse_str(receipt["thread"].as_str().unwrap()).is_ok());
            assert!(
                outcome.is_some_and(|status| !status.success()),
                "intentional post-confirmation failure was not observed: {outcome:?}"
            );
        } else {
            assert!(
                outcome.is_some_and(|status| status.success()),
                "archive fixture worker failed/timed out: {outcome:?}"
            );
        }
        return;
    }
    assert_eq!(role, "worker", "unknown isolated fixture role");
    let fixture = Fixture::new();
    let root = fixture.root.path();
    let project = root.join("project");
    fixture.cli(&["project", "open", project.to_str().unwrap()]);
    fs::write(
        root.join("automation-codex-fixture"),
        uuid::Uuid::new_v4().to_string(),
    )
    .unwrap();
    fs::create_dir(root.join("home/.codex")).unwrap();
    let provider = root.join("bin/codex");
    fs::write(&provider, include_str!("fixtures/automation-codex.py")).unwrap();
    fs::set_permissions(&provider, fs::Permissions::from_mode(0o700)).unwrap();
    let definition = json!({"name":"Delayed native archive","prompt":"Return the delayed fixture result","agent":{"provider":"Codex","account":null,"target":{"repository":project,"directory":project,"placement":null},"surface":"Chat","model":null,"permission_mode":null},"schedule":{"pattern":{"daily":{"hour":3,"minute":0}},"timezone":"UTC"},"session_mode":"fresh","visibility":"headfull","precheck":null});
    let definition_path = root.join("delayed-native.json");
    fs::write(&definition_path, serde_json::to_vec(&definition).unwrap()).unwrap();
    let created = fixture.cli(&[
        "automation",
        "create",
        "--definition",
        definition_path.to_str().unwrap(),
    ]);
    let automation = created["automation"]["id"].as_str().unwrap();
    let revision = created["automation"]["execution_revision"]
        .as_u64()
        .unwrap()
        .to_string();
    let request = uuid::Uuid::new_v4().to_string();
    let accepted = fixture.cli(&[
        "automation",
        "run-now",
        automation,
        "--execution-revision",
        &revision,
        "--request-id",
        &request,
        "--confirm",
    ]);
    let firing = accepted["firing"]["id"].as_str().unwrap().parse().unwrap();
    let socket = root
        .join("run")
        .join(cagents::instance::RUNTIME_SUBDIRECTORY)
        .join(cagents::terminal_ipc::SOCKET_FILE_NAME);
    let mut client = cagents::control::ControlClient::connect(&socket).unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    let (owner, initial) = loop {
        let Response::Detail(detail) = client
            .automation(Default::default(), Request::ShowFiring { id: firing })
            .unwrap()
        else {
            panic!("typed firing")
        };
        if let Some(agent) = detail.agent
            && let Some(HostedSession::Chat(source)) = agent.session.clone()
            && root.join("codex-thread-requested.json").exists()
        {
            break (agent, source);
        }
        assert!(
            Instant::now() < deadline,
            "provider was not bound before its delayed Session frame: {}",
            fs::read_to_string(root.join("daemon.log")).unwrap()
        );
        thread::sleep(Duration::from_millis(10));
    };
    assert!(!owner.closed);
    assert_eq!(initial.native_continuity, NativeContinuity::ReconnectOnly);
    assert!(initial.launch.session_id.is_none());
    assert!(!root.join("codex-thread-confirmed.json").exists());
    assert!(
        !root.join("codex-turn-requested.json").exists(),
        "no prompt before readiness"
    );
    let process_before = fs::read(root.join("codex-process.json")).unwrap();
    let process: Value = serde_json::from_slice(&process_before).unwrap();
    let thread = process["thread"].as_str().unwrap();
    let card = CardId::from_uuid(owner.id.as_uuid());
    let pane_database = root.join("delayed-pane.sqlite3");
    let registry = root
        .join("data")
        .join(cagents::instance::STORAGE_NAME)
        .join("workspace-model.sqlite3");
    let store = StateStore::open(&pane_database, BootId::new("delayed-native-fixture")).unwrap();
    let mut workspace = WorkspaceState::default();
    workspace.cards.push(CardState {
        id: card,
        generation: 0,
        title: "Delayed native result".into(),
        width_fraction: 0.6,
        existence: Existence::Open,
        exposure: Exposure::Visible,
        residency: Residency::Cold,
        kind: CardKind::Chat(initial.clone()),
    });
    workspace.focused = Some(card);
    let first = store
        .save_workspace(
            &workspace,
            SnapshotBase {
                workspace: 0,
                cards: &Default::default(),
            },
        )
        .unwrap();
    assert!(
        inspect_completed_automation_pane(
            &pane_database,
            &registry,
            card,
            &workspace.cards[0].kind
        )
        .unwrap()
        .is_none(),
        "a pending live actor is not an archive"
    );
    // This file releases the actual app-server thread/start response. The
    // production adapter, native peer proof and Holder perform promotion.
    fs::write(root.join("release-codex-thread"), thread).unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    while !root.join("codex-turn-requested.json").exists()
        || !root.join("codex-thread-confirmed.json").exists()
    {
        assert!(
            Instant::now() < deadline,
            "confirmed provider never received its one prompt"
        );
        thread::sleep(Duration::from_millis(10));
    }
    let confirmed: Value =
        serde_json::from_slice(&fs::read(root.join("codex-thread-confirmed.json")).unwrap())
            .unwrap();
    let prompted: Value =
        serde_json::from_slice(&fs::read(root.join("codex-turn-requested.json")).unwrap()).unwrap();
    assert_eq!(confirmed["thread"], process["thread"]);
    assert_eq!(prompted["thread"], process["thread"]);
    // Read the actual live attachment a window receives after the delayed
    // Session frame; do not manufacture its learned native marker for the test.
    fn exchange_fixture(
        stream: &mut std::os::unix::net::UnixStream,
        id: u64,
        proof: Option<cagents::model::HostSessionProof>,
        request: cagents::terminal_ipc::ClientRequest,
    ) -> cagents::terminal_ipc::ServerResponse {
        use cagents::terminal_ipc::{
            ClientEnvelope, PROTOCOL_VERSION, ServerEnvelope, ServerMessage, read_frame,
            write_frame,
        };
        stream
            .set_write_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        write_frame(
            stream,
            &ClientEnvelope {
                version: PROTOCOL_VERSION,
                request_id: id,
                proof,
                request,
            },
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(
                !remaining.is_zero(),
                "fixture live attachment response timed out"
            );
            stream.set_read_timeout(Some(remaining)).unwrap();
            let response: ServerEnvelope = read_frame(stream)
                .unwrap()
                .expect("fixture daemon disconnected");
            if response.request_id != Some(id) {
                continue;
            }
            match response.message {
                ServerMessage::Response { response } => return response,
                ServerMessage::Error(error) => panic!("fixture live attachment refused: {error:?}"),
                _ => {}
            }
        }
    }
    use cagents::terminal_ipc::{ClientRequest, ServerResponse};
    let mut window = std::os::unix::net::UnixStream::connect(&socket).unwrap();
    let ServerResponse::Connected { proof, .. } = exchange_fixture(
        &mut window,
        0,
        None,
        ClientRequest::Connect {
            expected_host: None,
            resume: Vec::new(),
        },
    ) else {
        panic!("fixture window connect")
    };
    let ServerResponse::ChatAttached {
        session: promoted,
        snapshot: live,
        ..
    } = exchange_fixture(
        &mut window,
        1,
        Some(proof),
        ClientRequest::ChatAttach {
            session: initial.clone(),
        },
    )
    else {
        panic!("fixture actual promoted attachment")
    };
    assert_eq!(
        live.native_continuity,
        NativeContinuity::Durable {
            provider_version: "0.162.0".into()
        }
    );
    assert_eq!(promoted.native_continuity, live.native_continuity);
    assert_eq!(promoted.launch.session_id.as_deref(), Some(thread));
    drop(window);
    if let Some(path) = std::env::var_os("CAGENTS_ARCHIVE_FAILURE_RECEIPT") {
        use std::os::unix::fs::OpenOptionsExt;
        let receipt = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(path)
            .unwrap();
        serde_json::to_writer(receipt,&json!({"point":"after-native-confirmation","provider_version":"0.162.0","thread":thread,"agent":owner.id.to_string()})).unwrap();
        panic!("intentional delayed archive fixture failure after actual native confirmation");
    }
    fs::write(root.join("release-codex-result"), thread).unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    let archived = loop {
        let Response::Detail(detail) = client
            .automation(Default::default(), Request::ShowFiring { id: firing })
            .unwrap()
        else {
            panic!("typed completed firing")
        };
        if detail.agent.as_ref().is_some_and(|agent| agent.closed)
            && let Ok(Response::ArchivedConversation(archive)) = client.automation(
                Default::default(),
                Request::ArchivedConversation { agent: owner.id },
            )
        {
            break archive;
        }
        assert!(
            Instant::now() < deadline,
            "confirmed completed provider was not retired"
        );
        thread::sleep(Duration::from_millis(10));
    };
    assert_eq!(
        archived.snapshot.native_continuity,
        NativeContinuity::Durable {
            provider_version: "0.162.0".into()
        }
    );
    assert_eq!(archived.snapshot.session_id.as_deref(), Some(thread));
    assert!(archived.agent.closed && archived.snapshot.exited && !archived.snapshot.ready);
    assert!(archived.snapshot.items.iter().any(|item|matches!(&item.content,cagents::chat::ItemContent::Text{text,..} if text.contains("DELAYED_NATIVE_ARCHIVE_RESULT"))));
    assert!(
        inspect_completed_automation_pane(
            &pane_database,
            &registry,
            card,
            &workspace.cards[0].kind
        )
        .unwrap()
        .is_some(),
        "cold pre-confirmation descriptor without saved archive map must qualify privately"
    );
    // Persist the identity a window learns from that same real private
    // snapshot. Neither alias may force a new executable attachment on reopen.
    assert_eq!(promoted.launch.session_id, archived.snapshot.session_id);
    assert_eq!(
        promoted.native_continuity,
        archived.snapshot.native_continuity
    );
    workspace.cards[0].kind = CardKind::Chat(promoted.clone());
    workspace.automation_archives.insert(card, owner.id);
    store
        .save_workspace(
            &workspace,
            SnapshotBase {
                workspace: first.generation,
                cards: &first.card_generations,
            },
        )
        .unwrap();
    let restored = store.load_workspace(workspace.id, None).unwrap().unwrap();
    assert_eq!(restored.cards[0].kind, CardKind::Chat(promoted.clone()));
    assert!(
        inspect_completed_automation_pane(&pane_database, &registry, card, &restored.cards[0].kind)
            .unwrap()
            .is_some()
    );
    for source in [&initial, &promoted] {
        assert!(
            cagents::automations::archive::ArchivedPaneReadiness::qualify(
                owner.id, source, &archived
            )
            .unwrap()
            .matches(owner.id, source)
        );
    }
    assert!(
        client.list_chats().unwrap().is_empty(),
        "cold qualification has no live provider actor"
    );
    let pid = process["pid"].as_u64().unwrap();
    let start = process["start_ticks"].as_str().unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let still_same_peer = fs::read_to_string(format!("/proc/{pid}/stat"))
            .ok()
            .is_some_and(|stat| {
                stat.rsplit_once(')')
                    .and_then(|(_, tail)| tail.split_whitespace().nth(19))
                    == Some(start)
            });
        if !still_same_peer {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "fresh archive must retire the exact native peer process"
        );
        thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        fs::read(root.join("codex-process.json")).unwrap(),
        process_before,
        "cold qualification cannot spawn a replacement peer"
    );
    let after_prompt: Value =
        serde_json::from_slice(&fs::read(root.join("codex-turn-requested.json")).unwrap()).unwrap();
    assert_eq!(
        after_prompt, prompted,
        "cold qualification cannot replay a prompt"
    );
}
