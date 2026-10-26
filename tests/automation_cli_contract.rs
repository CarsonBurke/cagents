#![cfg(feature = "host")]

#[path = "support/hosted_agent_cleanup.rs"]
mod hosted_agent_cleanup;

use serde_json::{Value, json};
use std::{
    fs,
    os::unix::{fs::PermissionsExt, process::CommandExt},
    path::Path,
    process::{Child, Command, Output, Stdio},
    thread,
    time::{Duration, Instant},
};

struct Fixture {
    root: tempfile::TempDir,
    daemon: Child,
    agents: hosted_agent_cleanup::Agents,
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
        Self::start_with_soft_limit(root, None)
    }

    fn start_with_soft_limit(root: &Path, soft_limit: Option<libc::rlim_t>) -> Child {
        let log = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(root.join("daemon.log"))
            .unwrap();
        let mut command = Self::command(root, env!("CARGO_BIN_EXE_cagentsd"));
        command.stdout(log.try_clone().unwrap()).stderr(log);
        if let Some(soft_limit) = soft_limit {
            // Set only the fixture daemon's incoming limit. The test process
            // and the other contracts running beside it retain their limits.
            unsafe {
                command.pre_exec(move || {
                    let mut limit = std::mem::zeroed();
                    if libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) != 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    limit.rlim_cur = soft_limit;
                    if libc::setrlimit(libc::RLIMIT_NOFILE, &limit) != 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
        }
        command.spawn().unwrap()
    }

    fn new() -> Self {
        Self::with_soft_limit(None)
    }

    fn with_soft_limit(soft_limit: Option<libc::rlim_t>) -> Self {
        let root = tempfile::tempdir().unwrap();
        for name in [
            "home", "run", "data", "config", "cache", "tmp", "project", "bin",
        ] {
            fs::create_dir(root.path().join(name)).unwrap();
        }
        fs::set_permissions(root.path().join("run"), fs::Permissions::from_mode(0o700)).unwrap();
        let daemon = Self::start_with_soft_limit(root.path(), soft_limit);
        let mut fixture = Self {
            root,
            daemon,
            agents: Default::default(),
        };
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
        let output = child.wait_with_output().unwrap();
        self.agents.observe(&output);
        output
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
        let panicking = thread::panicking();
        if panicking {
            self.root.disable_cleanup(true);
            hosted_agent_cleanup::diagnostics(self.root.path());
        }
        // Fresh firings retire their execution through the scheduler. Cancel
        // any still-active private firing only after contract assertions; the
        // intentional stop/restart boundaries in the tests remain unchanged.
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
fn precheck_shell_keeps_incoming_limits_while_daemon_retains_raised_limit() {
    let mut incoming: libc::rlimit = unsafe { std::mem::zeroed() };
    assert_eq!(
        unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut incoming) },
        0
    );
    assert!(
        incoming.rlim_max > 64,
        "fixture needs room below the hard limit"
    );
    let soft_limit = incoming.rlim_cur.min(1024).min(incoming.rlim_max - 1);
    let fixture = Fixture::with_soft_limit(Some(soft_limit));
    let project = fixture.root.path().join("project");
    fixture.cli(&["project", "open", project.to_str().unwrap()]);
    let definition = json!({
        "name": "Descriptor limit fixture", "prompt": "must never launch a provider",
        "agent": { "provider": "Omp", "account": null,
            "target": {"repository": project, "directory": project, "channel": null},
            "surface": "Chat", "model": null, "permission_mode": null },
        "schedule": {"pattern": {"daily": {"hour": 3, "minute": 0}}, "timezone": "UTC"},
        "session_mode": "fresh",
        "precheck": {"command": "ulimit -Sn > soft-limit; ulimit -Hn > hard-limit; exit 1", "timeout_seconds": 2}
    });
    let path = fixture.root.path().join("definition.json");
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
    let accepted = fixture.cli(&[
        "automation",
        "run-now",
        id,
        "--execution-revision",
        &revision,
        "--request-id",
        &uuid::Uuid::new_v4().to_string(),
        "--confirm",
    ]);
    let firing = accepted["firing"]["id"].as_str().unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let detail = fixture.cli(&["automation", "run", firing]);
        if detail["run"]["firing"]["state"] == "skipped" {
            assert!(detail["run"]["run"].is_null());
            break;
        }
        assert!(
            Instant::now() < deadline,
            "precheck did not settle: {detail}"
        );
        thread::sleep(Duration::from_millis(50));
    }
    let limit_text = |limit: libc::rlim_t| {
        if limit == libc::RLIM_INFINITY {
            "unlimited".to_owned()
        } else {
            limit.to_string()
        }
    };
    assert_eq!(
        fs::read_to_string(project.join("soft-limit"))
            .unwrap()
            .trim(),
        limit_text(soft_limit)
    );
    assert_eq!(
        fs::read_to_string(project.join("hard-limit"))
            .unwrap()
            .trim(),
        limit_text(incoming.rlim_max)
    );
    let daemon_limits =
        fs::read_to_string(format!("/proc/{}/limits", fixture.daemon.id())).unwrap();
    let mut fields = daemon_limits
        .lines()
        .find_map(|line| line.strip_prefix("Max open files"))
        .unwrap()
        .split_whitespace();
    assert_eq!(fields.next().unwrap(), limit_text(incoming.rlim_max));
    assert_eq!(fields.next().unwrap(), limit_text(incoming.rlim_max));
    let mut unchanged: libc::rlimit = unsafe { std::mem::zeroed() };
    assert_eq!(
        unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut unchanged) },
        0
    );
    assert_eq!(unchanged.rlim_cur, incoming.rlim_cur);
    assert_eq!(unchanged.rlim_max, incoming.rlim_max);
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
fn automation_cli_lifecycle_audit_pages_durable_receipts_without_running_a_provider() {
    let mut fixture = Fixture::new();
    let project = fixture.root.path().join("project");
    // This case owns lifecycle receipts, not discovery of a plain folder.
    // Bind its project explicitly even when TMPDIR is inside another checkout.
    let initialized = Fixture::command(fixture.root.path(), "git")
        .args([
            "init",
            "--quiet",
            "--template=",
            "--initial-branch=lifecycle-fixture",
        ])
        .arg(&project)
        .output()
        .unwrap();
    assert!(initialized.status.success(), "{initialized:?}");
    let discovered = Fixture::command(fixture.root.path(), "git")
        .arg("-C")
        .arg(&project)
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .unwrap();
    assert!(discovered.status.success(), "{discovered:?}");
    let discovered = String::from_utf8(discovered.stdout).unwrap();
    assert_eq!(
        Path::new(discovered.trim_end()).canonicalize().unwrap(),
        project.canonicalize().unwrap(),
        "the audit fixture must not discover an enclosing repository"
    );
    fixture.cli(&["project", "open", project.to_str().unwrap()]);
    let provider = fixture.root.path().join("bin/omp");
    fs::write(
        &provider,
        "#!/bin/sh\nprintf 'unexpected provider\\n' >> \"$HOME/../unexpected-provider\"\nexit 1\n",
    )
    .unwrap();
    fs::set_permissions(&provider, fs::Permissions::from_mode(0o700)).unwrap();
    let definition = json!({
        "name": "Lifecycle only", "prompt": "PRIVATE_LIFECYCLE_PROMPT",
        "agent": {"provider": "Omp", "account": null,
            "target": {"repository": project, "directory": project, "channel": null},
            "surface": "Chat", "model": null, "permission_mode": null},
        "schedule": {"pattern": {"daily": {"hour": 3, "minute": 0}}, "timezone": "UTC"},
        "session_mode": "fresh", "precheck": null
    });
    let path = fixture.root.path().join("audit-definition.json");
    fs::write(&path, serde_json::to_vec(&definition).unwrap()).unwrap();
    let created = fixture.cli(&[
        "automation",
        "create",
        "--definition",
        path.to_str().unwrap(),
    ]);
    let id = created["automation"]["id"].as_str().unwrap().to_owned();
    fixture.cli(&["automation", "disable", &id, "--revision", "1"]);
    assert!(
        !fixture
            .output(&["automation", "disable", &id, "--revision", "1"])
            .status
            .success()
    );
    let first = fixture.cli(&["automation", "audit", &id, "--limit", "1"]);
    assert_eq!(first["audit"]["events"][0]["cause"], "disabled");
    assert_eq!(first["audit"]["events"][0]["actor"], "user_management");
    assert_eq!(first["audit"]["events"][0]["revision"], 2);
    let cursor = first["audit"]["next"].as_u64().unwrap().to_string();
    let second = fixture.cli(&[
        "automation",
        "audit",
        &id,
        "--limit",
        "1",
        "--before",
        &cursor,
    ]);
    assert_eq!(second["audit"]["events"][0]["cause"], "created");
    assert!(second["audit"]["next"].is_null());
    assert_eq!(second["audit"]["earlier_events_unavailable"], false);
    let audit = fixture.cli(&["automation", "audit", &id]);
    assert!(!audit.to_string().contains("PRIVATE_LIFECYCLE_PROMPT"));
    fixture.restart();
    assert_eq!(fixture.cli(&["automation", "audit", &id]), audit);
    fixture.cli(&["automation", "remove", &id, "--revision", "2"]);
    let removed = fixture.cli(&["automation", "audit", &id]);
    assert_eq!(removed["audit"]["events"].as_array().unwrap().len(), 3);
    assert_eq!(removed["audit"]["events"][0]["cause"], "removed");
    assert_eq!(removed["audit"]["events"][0]["revision"], 3);
    assert_eq!(
        fixture.cli(&["automation", "history", &id])["history"]["firings"],
        json!([])
    );
    assert!(!fixture.root.path().join("unexpected-provider").exists());
}
