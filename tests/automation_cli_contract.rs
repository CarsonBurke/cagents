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
            .env("PATH", "/usr/bin:/bin")
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
        for name in ["home", "run", "data", "config", "cache", "tmp", "project"] {
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
