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
        let fixture = Self { root, daemon };
        fixture.ready();
        fixture
    }

    fn ready(&self) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let output = self.output(&["daemon", "status"]);
            if output.status.success()
                && serde_json::from_slice::<Value>(&output.stdout)
                    .is_ok_and(|v| v["data"]["running"] == true)
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

    fn output(&self, arguments: &[&str]) -> Output {
        let mut child = Self::command(self.root.path(), env!("CARGO_BIN_EXE_cagents"))
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
        "session_mode": "fresh", "precheck": null
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
    let snapshot = client.read_notifications().unwrap();
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
        history["history"]["names"][firing.as_str()].is_string(),
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
