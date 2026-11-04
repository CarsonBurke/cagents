#![cfg(feature = "host")]

//! Shipped CLI -> daemon -> retained cache with a real issued child caller.
//! Canonical panes are published through the existing public UI model API,
//! using only daemon-returned live descriptors. This is a synthetic UI
//! publisher, not native GUI or commercial-provider qualification.

#[path = "support/hosted_agent_cleanup.rs"]
mod hosted_agent_cleanup;
#[path = "support/private_cli_capture.rs"]
mod private_cli_capture;

use cagents::{
    agent_execution::HostedSession,
    control::ControlClient,
    model::{CardId, CardKind, ChannelId},
    workspace_model::{
        ModelEdit,
        views::{Pane, Scope},
    },
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs,
    os::unix::fs::{PermissionsExt, symlink},
    path::Path,
    process::{Child, Command, Output, Stdio},
    thread,
    time::{Duration, Instant},
};

struct Fixture {
    root: tempfile::TempDir,
    daemon: Child,
    agents: hosted_agent_cleanup::Agents,
    parent: Option<String>,
}

impl Fixture {
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
            include_bytes!("fixtures/peer-inspection-claude.py"),
        )
        .unwrap();
        fs::set_permissions(provider, fs::Permissions::from_mode(0o700)).unwrap();
        // The provider invokes this actual matched CLI; no protocol peer or
        // authority value is substituted by the test process.
        symlink(
            env!("CARGO_BIN_EXE_cagents"),
            root.path().join("bin/cagents"),
        )
        .unwrap();
        let log = fs::File::create(root.path().join("daemon.log")).unwrap();
        let mut command = Command::new(env!("CARGO_BIN_EXE_cagentsd"));
        command.env_clear();
        for (name, leaf) in [
            ("HOME", "home"),
            ("XDG_RUNTIME_DIR", "run"),
            ("XDG_DATA_HOME", "data"),
            ("XDG_CONFIG_HOME", "config"),
            ("XDG_CACHE_HOME", "cache"),
            ("TMPDIR", "tmp"),
        ] {
            command.env(name, root.path().join(leaf));
        }
        let daemon = command
            .env(
                "PATH",
                format!("{}:/usr/bin:/bin", root.path().join("bin").display()),
            )
            .current_dir(root.path())
            .stdin(Stdio::null())
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .spawn()
            .unwrap();
        let fixture = Self {
            root,
            daemon,
            agents: Default::default(),
            parent: None,
        };
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let output = fixture.output_until(&["daemon", "status"], deadline);
            if output.status.success()
                && serde_json::from_slice::<Value>(&output.stdout).is_ok_and(|v| {
                    v["data"]["running"] == true
                        && v["data"]["daemon_version"]
                            .as_str()
                            .is_some_and(|s| !s.is_empty())
                })
            {
                break;
            }
            assert!(Instant::now() < deadline, "private daemon readiness");
            thread::sleep(Duration::from_millis(20));
        }
        fixture
    }

    fn output_until(&self, arguments: &[&str], deadline: Instant) -> Output {
        let spec = private_cli_capture::private_cli(self.root.path())
            .args(["--device", "local", "--no-start"])
            .args(arguments.iter().copied());
        let output = private_cli_capture::capture(spec, deadline)
            .unwrap_or_else(|failure| panic!("private CLI {arguments:?}: {failure:?}"));
        self.agents.observe(&output);
        output
    }

    fn cli(&self, arguments: &[&str]) -> Value {
        let output = self.output_until(arguments, Instant::now() + Duration::from_secs(25));
        assert!(output.status.success(), "{arguments:?}: {output:?}");
        let value: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(value["ok"], true, "{value}");
        value["data"].clone()
    }

    fn spawn(&self, channel: &str, name: &str, prompt: Value) -> Value {
        // A recreated public name is a new logical request, not a retry of
        // the retired target's durable (origin, idempotency-key) pair.
        let key = uuid::Uuid::new_v4().to_string();
        self.cli(&[
            "agent",
            "spawn",
            "--provider",
            "claude",
            "--surface",
            "chat",
            "--project",
            self.root.path().join("project").to_str().unwrap(),
            "--channel",
            channel,
            "--name",
            name,
            "--idempotency-key",
            &key,
            "--wait",
            "--timeout",
            "15",
            &prompt.to_string(),
        ])
    }

    fn parent(&self, prompt: Value) -> Value {
        let value = self.cli(&[
            "agent",
            "send",
            self.parent.as_deref().unwrap(),
            &prompt.to_string(),
            "--wait",
            "--timeout",
            "20",
        ]);
        let report: Value =
            serde_json::from_str(value["run"]["result"]["text"].as_str().unwrap()).unwrap();
        assert!(report.get("fixture_error").is_none(), "{report}");
        report
    }

    fn client(&self) -> ControlClient {
        let status = self.cli(&["daemon", "status"]);
        ControlClient::connect(Path::new(status["socket"].as_str().unwrap())).unwrap()
    }

    fn publish(&self, channel: &str, ids: &[&str]) {
        let channel: ChannelId = channel.parse().unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut client = self.client();
        loop {
            let model = client.read_workspace_model().unwrap();
            let chats = client.list_chats().unwrap();
            let mut panes = Vec::new();
            for id in ids {
                let agent = model
                    .agents
                    .iter()
                    .find(|agent| agent.id.to_string() == *id)
                    .unwrap();
                assert!(!agent.closed && agent.placement == Scope::Channel(channel));
                let Some(HostedSession::Chat(source)) = &agent.session else {
                    continue;
                };
                let Some(source) = chats.iter().find(|chat| chat.id == source.id) else {
                    continue;
                };
                panes.push(Pane {
                    id: CardId::from_uuid(agent.id.as_uuid()),
                    title: "Peer contract".into(),
                    user_title: None,
                    pending_terminal_start: false,
                    width_fraction: 1.0,
                    kind: CardKind::Chat(source.clone()),
                    scope: None,
                });
            }
            if panes.len() == ids.len() {
                // Same public publication route used by the existing console
                // contract and the UI. No Registry, DB write or fabricated proof.
                client
                    .edit_workspace_model(ModelEdit::SeedView {
                        scope: Scope::Channel(channel),
                        panes,
                        closed: vec![],
                    })
                    .unwrap();
                return;
            }
            assert!(
                Instant::now() < deadline,
                "live canonical sources were not published"
            );
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn prompts(&self) -> usize {
        fs::read_to_string(self.root.path().join("target-prompts"))
            .unwrap()
            .lines()
            .count()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let panicking = thread::panicking();
        // User does not take over a child on cleanup. Its real owning parent
        // performs the child's public Close using the parent's issued context.
        let child = self
            .parent
            .as_ref()
            .map(|parent| {
                hosted_agent_cleanup::cli(
                    self.root.path(),
                    &[
                        "agent",
                        "send",
                        parent,
                        "{\"action\":\"cleanup\"}",
                        "--wait",
                        "--timeout",
                        "20",
                    ],
                    Instant::now() + Duration::from_secs(25),
                )
                .and_then(|value| {
                    let report: Value = serde_json::from_str(
                        value["run"]["result"]["text"]
                            .as_str()
                            .ok_or("parent cleanup omitted its result")?,
                    )
                    .map_err(|e| e.to_string())?;
                    if report["child_closed"] == true {
                        Ok(())
                    } else {
                        Err("owning parent did not confirm child Close".into())
                    }
                })
            })
            .transpose();
        let mut failures = Vec::new();
        if let Err(error) = child {
            failures.push(error);
        }
        // A child failure must not skip the User's legal cleanup of its own
        // accepted roots. Preserve every failure even if later cleanup works.
        match self.agents.close(self.root.path()) {
            Ok(()) => {
                if let Err(error) = hosted_agent_cleanup::stop(self.root.path()) {
                    failures.push(error);
                }
            }
            Err(error) => failures.push(error),
        }
        let deadline = Instant::now() + Duration::from_secs(3);
        while self.daemon.try_wait().ok().flatten().is_none() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        let stopped = self.daemon.try_wait().ok().flatten().is_some();
        if !stopped {
            failures.push("private daemon did not stop after public cleanup".into());
            let _ = self.daemon.kill();
        }
        let _ = self.daemon.wait();
        if !failures.is_empty() {
            self.root.disable_cleanup(true);
            hosted_agent_cleanup::diagnostics(self.root.path());
            if !panicking {
                panic!("private peer cleanup: {}", failures.join("; "));
            }
        }
    }
}

fn assert_page_report(report: &Value, tag: &str) {
    let text = format!("{tag}{}", "PUBLIC_PEER_λ🦀\n".repeat(12000));
    assert_eq!(
        report["assistant_sha256"],
        format!("{:x}", Sha256::digest(text.as_bytes()))
    );
    assert_eq!(report["assistant_bytes"], text.len());
    assert_eq!(report["tool_output"], "PUBLIC_TOOL_RESULT_λ🦀");
    assert!(report["pages"].as_u64().unwrap() > 1);
    assert!(report["largest_page"].as_u64().unwrap() <= 128 * 1024);
    assert!(report["most_parts"].as_u64().unwrap() <= 16);
    assert_eq!(report["target_owner"], "User");
    assert!(report["caller_owner"]["Session"].is_string());
    assert_eq!(report["target_relation"], "Peer");
    assert_eq!(report["excluded_fields_absent"], true);
    assert_eq!(report["at_end"], true);
    assert!(report["next_cursor"].is_null());
}

#[test]
fn issued_child_cli_reads_user_peer_but_refuses_control_other_channel_and_stale_cursor() {
    let mut fixture = Fixture::new();
    let project = fixture.root.path().join("project");
    let channel = fixture.cli(&[
        "channel",
        "spawn",
        "--repository",
        project.to_str().unwrap(),
    ]);
    let a = channel["channel"]["id"].as_str().unwrap().to_owned();
    let channel = fixture.cli(&[
        "channel",
        "spawn",
        "--repository",
        project.to_str().unwrap(),
    ]);
    let b = channel["channel"]["id"].as_str().unwrap().to_owned();
    let target = fixture.spawn(
        &a,
        "peer-target",
        json!({"role":"target","tag":"ORIGINAL|"}),
    );
    assert_eq!(target["agent"]["origin"], "User");
    let target = target["agent"]["id"].as_str().unwrap().to_owned();
    let parent = fixture.spawn(
        &a,
        "peer-parent",
        json!({
            "role":"parent", "project":project, "channel":a, "target":target,
        }),
    );
    fixture.parent = Some(parent["agent"]["id"].as_str().unwrap().to_owned());
    let child: Value =
        serde_json::from_str(parent["run"]["result"]["text"].as_str().unwrap()).unwrap();
    assert!(
        child["child_origin"]["Child"]["parent"].is_string(),
        "{child}"
    );
    let child_session = child["child_session"].as_str().unwrap();
    let child = child["child_agent"].as_str().unwrap();
    fixture.publish(&a, &[&target, fixture.parent.as_deref().unwrap(), child]);
    let before = fixture.cli(&["agent", "status", &target, "--full"]);
    let prompts = fixture.prompts();
    let read = fixture.parent(json!({"action":"read"}));
    assert_eq!(read["caller_session"], child_session);
    assert_page_report(&read, "ORIGINAL|");
    assert_eq!(read["owner_status_hint"], true);
    assert_eq!(read["owner_close_refused"], true);
    assert_eq!(fixture.cli(&["agent", "status", &target, "--full"]), before);
    assert_eq!(
        fixture.prompts(),
        prompts,
        "peer reads must not send another prompt"
    );

    fixture.cli(&["channel", "move", &target, "--to", &b]);
    let outside = fixture.parent(json!({"action":"outside"}));
    assert_eq!(outside["fresh_refused"], "protocol");
    assert_eq!(outside["continuation_refused"], "protocol");
    fixture.cli(&["channel", "move", &target, "--to", &a]);
    let returned = fixture.parent(json!({"action":"returned"}));
    assert_eq!(returned["old_cursor_refused"], "protocol");
    assert_page_report(&returned["fresh"], "ORIGINAL|");
    assert_eq!(fixture.prompts(), prompts);

    // Reusing the public name must not revive the old session/cursor.
    fixture.cli(&["agent", "close", &target]);
    let replacement = fixture.spawn(
        &a,
        "peer-target",
        json!({"role":"target","tag":"REPLACEMENT|"}),
    );
    let replacement = replacement["agent"]["id"].as_str().unwrap();
    assert_ne!(replacement, target);
    fixture.publish(&a, &[replacement]);
    let replaced = fixture.parent(json!({"action":"replacement", "target":replacement}));
    assert_eq!(replaced["old_session_refused"], "protocol");
    assert_eq!(replaced["new_session_old_cursor_refused"], "protocol");
    assert_page_report(&replaced["fresh"], "REPLACEMENT|");
    assert_eq!(fixture.prompts(), prompts + 1);
    assert_eq!(
        fixture.parent(json!({"action":"cleanup"}))["child_closed"],
        true
    );
}
