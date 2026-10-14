#![cfg(feature = "host")]

use cagents::{
    control::ControlClient,
    model::{BrowserId, BrowserRestoreFidelity, BrowserSession, CardId, CardKind},
    workspace_model::{
        ModelEdit, WorkspaceModel,
        views::{Pane, Scope},
    },
};
use serde_json::Value;
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
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
        for name in [
            "home", "run", "data", "config", "cache", "tmp", "project", "other",
        ] {
            fs::create_dir(root.path().join(name)).unwrap();
        }
        fs::set_permissions(root.path().join("run"), fs::Permissions::from_mode(0o700)).unwrap();
        let daemon = Self::start(root.path());
        let mut fixture = Self { root, daemon };
        fixture.ready();
        fixture
    }

    fn ready(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            assert!(
                self.daemon.try_wait().unwrap().is_none(),
                "fixture daemon exited: {}",
                fs::read_to_string(self.root.path().join("daemon.log")).unwrap()
            );
            let output = self.output(&["daemon", "status"]);
            if output.status.success()
                && serde_json::from_slice::<Value>(&output.stdout).is_ok_and(|status| {
                    status["data"]["running"] == true
                        && status["data"]["daemon_version"].is_string()
                })
            {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "the fixture daemon never answered"
            );
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn output(&self, arguments: &[&str]) -> Output {
        Self::command(self.root.path(), env!("CARGO_BIN_EXE_cagents"))
            .arg("--json")
            .args(arguments)
            .output()
            .unwrap()
    }

    fn cli(&self, arguments: &[&str]) -> Value {
        let output = self.output(arguments);
        let response: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert!(output.status.success(), "{arguments:?}: {output:?}");
        assert_eq!(response["ok"], true, "{response}");
        response["data"].clone()
    }

    fn refused(&self, arguments: &[&str]) -> Value {
        let output = self.output(arguments);
        assert!(!output.status.success(), "{arguments:?} succeeded");
        let response: Value = serde_json::from_slice(&output.stderr).unwrap();
        assert_eq!(response["ok"], false, "{response}");
        response["error"].clone()
    }

    fn client(&self) -> ControlClient {
        let status = self.cli(&["daemon", "status"]);
        ControlClient::connect(Path::new(status["socket"].as_str().unwrap())).unwrap()
    }

    fn model(&self) -> WorkspaceModel {
        self.client().read_workspace_model().unwrap()
    }

    fn channel(&self, project: &Path) -> String {
        let data = self.cli(&[
            "channel",
            "spawn",
            "--repository",
            project.to_str().unwrap(),
        ]);
        data["channel"]["id"].as_str().unwrap().to_owned()
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

fn pane(project: PathBuf) -> Pane {
    Pane {
        id: CardId::new(),
        title: "Docs".into(),
        user_title: Some("Reference".into()),
        pending_terminal_start: false,
        width_fraction: 0.4,
        scope: Some(project),
        kind: CardKind::Browser(BrowserSession {
            id: BrowserId::new(),
            current_url: "https://example.com/docs".parse().unwrap(),
            history: vec![],
            history_index: 0,
            scroll_x: 0.,
            scroll_y: 0.,
            fidelity: BrowserRestoreFidelity::UrlOnly,
        }),
    }
}

fn panes(model: &WorkspaceModel, channel: &str) -> Vec<Pane> {
    let scope = Scope::Channel(channel.parse().unwrap());
    model
        .views
        .iter()
        .find(|view| view.scope == scope)
        .map(|view| view.panes.clone())
        .unwrap_or_default()
}

#[test]
fn a_pane_moves_between_channels_of_its_project_intact_and_durably() {
    let mut fixture = Fixture::new();
    let project = fixture.root.path().join("project");
    let other = fixture.root.path().join("other");
    let source = fixture.channel(&project);
    let destination = fixture.channel(&project);
    let foreign = fixture.channel(&other);
    let repository = fixture
        .model()
        .channel(source.parse().unwrap())
        .unwrap()
        .repository
        .clone();
    let (moving, staying) = (pane(repository.clone()), pane(repository));
    fixture
        .client()
        .edit_workspace_model(ModelEdit::SeedView {
            scope: Scope::Channel(source.parse().unwrap()),
            panes: vec![moving.clone(), staying.clone()],
            closed: vec![],
        })
        .unwrap();
    let CardKind::Browser(session) = &moving.kind else {
        unreachable!()
    };
    let session = session.id.to_string();

    // Named by the session it hosts, as `cagents sessions` reports it.
    let moved = fixture.cli(&["channel", "move", &session, "--to", &destination]);
    assert_eq!(moved["moved"], true);
    assert_eq!(moved["pane"], moving.id.to_string());
    assert_eq!(moved["from"]["Channel"], source);
    assert_eq!(moved["to"]["Channel"], destination);
    let model = fixture.model();
    assert_eq!(panes(&model, &source), vec![staying.clone()]);
    assert_eq!(
        panes(&model, &destination),
        vec![moving.clone()],
        "the descriptor moves unchanged"
    );

    let retried = fixture.cli(&["channel", "move", &session, "--to", &destination]);
    assert_eq!(retried["moved"], false, "a retry changes nothing");

    // Placed before an anchor in the destination, named by its pane id.
    let anchor = staying.id.to_string();
    fixture.cli(&["channel", "move", &anchor, "--to", &destination]);
    let reordered = fixture.cli(&[
        "channel",
        "move",
        &moving.id.to_string(),
        "--to",
        &destination,
        "--before",
        &anchor,
    ]);
    assert_eq!(reordered["moved"], true);
    assert_eq!(
        panes(&fixture.model(), &destination),
        vec![moving.clone(), staying.clone()]
    );

    let error = fixture.refused(&["channel", "move", &session, "--to", &foreign]);
    assert!(
        error["message"]
            .as_str()
            .unwrap()
            .contains("only within one project"),
        "{error}"
    );
    let unknown = uuid::Uuid::new_v4().to_string();
    fixture.refused(&["channel", "move", &unknown, "--to", &destination]);
    fixture.refused(&["channel", "move", &session, "--to", &unknown]);

    fixture.restart();
    let model = fixture.model();
    assert!(panes(&model, &source).is_empty());
    assert!(panes(&model, &foreign).is_empty());
    assert_eq!(panes(&model, &destination), vec![moving, staying]);
}
