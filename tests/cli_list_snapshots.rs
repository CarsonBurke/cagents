//! Real CLI output against a deterministic, CLI-lane binary protocol fixture.
use cagents::{agent::Provider, agent_execution as execution, automations, model::*, terminal_ipc::*, workspace_model::*};
use serde_json::json;
use std::{fs, os::unix::{fs::PermissionsExt, net::{UnixListener, UnixStream}}, process::{Command, Stdio}, sync::{Arc, atomic::{AtomicBool, Ordering}}, thread, time::Duration};
const ID: &str = "11111111-1111-4111-8111-111111111111";
const RUN: &str = "22222222-2222-4222-8222-222222222222";

struct Fixture { root: tempfile::TempDir, socket: std::path::PathBuf, stop: Arc<AtomicBool>, worker: Option<thread::JoinHandle<()>> }
impl Fixture {
    fn new(populated: bool) -> Self {
        let root = tempfile::tempdir().unwrap();
        for path in ["home", "run", "data", "config", "cache"] { fs::create_dir(root.path().join(path)).unwrap(); }
        fs::set_permissions(root.path().join("run"), fs::Permissions::from_mode(0o700)).unwrap();
        let runtime = root.path().join("run").join(cagents::instance::RUNTIME_SUBDIRECTORY);
        fs::create_dir(&runtime).unwrap(); fs::set_permissions(&runtime, fs::Permissions::from_mode(0o700)).unwrap();
        let socket = runtime.join(SOCKET_FILE_NAME);
        let listener = UnixListener::bind(&socket).unwrap(); listener.set_nonblocking(true).unwrap();
        let stop = Arc::new(AtomicBool::new(false)); let end = Arc::clone(&stop);
        let worker = thread::spawn(move || {
            let proof = HostSessionProof { host_session_id: HostSessionId::new("list-fixture"), boot_id: BootId::new("boot"), host_incarnation: HostIncarnation::new("incarnation") };
            while !end.load(Ordering::Relaxed) {
                let (mut stream, _) = match listener.accept() { Ok(client) => client, Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => { thread::sleep(Duration::from_millis(2)); continue; }, Err(error) => panic!("{error}") };
                stream.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
                while let Ok(Some(request)) = read_frame::<ClientEnvelope>(&mut stream) {
                    let response = if matches!(request.request, ClientRequest::Connect { .. }) {
                        ServerResponse::Connected { proof: proof.clone(), resumed: vec![] }
                    } else { respond(request.request, populated) };
                    write_frame(&mut stream, &ServerEnvelope { version: PROTOCOL_VERSION, request_id: Some(request.request_id), proof: proof.clone(), message: ServerMessage::Response { response } }).unwrap();
                }
            }
        });
        Self { root, socket, stop, worker: Some(worker) }
    }
    fn output(&self, args: &[&str], json_output: bool) -> std::process::Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_cagents"));
        command.env_clear()
            .env("HOME", self.root.path().join("home")).env("XDG_RUNTIME_DIR", self.root.path().join("run"))
            .env("XDG_DATA_HOME", self.root.path().join("data")).env("XDG_CONFIG_HOME", self.root.path().join("config"))
            .env("XDG_CACHE_HOME", self.root.path().join("cache")).env("PATH", "/usr/bin:/bin")
            .current_dir(self.root.path()).stdin(Stdio::null()).arg("--no-start").args(args);
        if json_output { command.arg("--json"); }
        let result = command.output().unwrap();
        assert!(result.status.success(), "{args:?}: {result:?}");
        result
    }
    fn json(&self, args: &[&str]) -> serde_json::Value {
        serde_json::from_slice(&self.output(args, true).stdout).unwrap()
    }
    fn human(&self, args: &[&str]) -> String {
        let result = self.output(args, false);
        let output = String::from_utf8(result.stdout).unwrap();
        for debug in ["Some(", "None", "at_unix_ms", " { "] { assert!(!output.contains(debug), "{args:?}: {output}"); }
        output.trim_end().into()
    }
}
impl Drop for Fixture { fn drop(&mut self) { self.stop.store(true, Ordering::Relaxed); let _ = UnixStream::connect(&self.socket); if let Some(worker) = self.worker.take() { let _ = worker.join(); } } }

fn respond(request: ClientRequest, populated: bool) -> ServerResponse {
    let terminal_id: TerminalId = ID.parse().unwrap();
    let recipe = LaunchRecipe { program: "bash".into(), arguments: vec![], working_directory: "/work/repo".into(), environment: Default::default(), unset_environment: Default::default() };
    match request {
        ClientRequest::List => {
            let mut terminal = TerminalDescriptor::relaunchable(terminal_id, recipe, None);
            terminal.title = "Fixture shell".into(); terminal.completed = false; terminal.visibility = TerminalVisibility::Hidden;
            ServerResponse::Listed { terminals: if populated { vec![terminal] } else { vec![] } }
        }
        ClientRequest::ReadWorkspaceModel => {
            let mut model = WorkspaceModel::default();
            if populated {
                model.projects.push(ModelProject { repository: "/work/repo/.git".into(), name: "Fixture project".into(), kind: ProjectKind::Git, ordinal: 1 });
                model.channels.push(ModelChannel { id: ID.parse().unwrap(), repository: "/work/repo/.git".into(), title: "Fixture channel".into(), title_source: Default::default(), ordinal: 1 });
            }
            ServerResponse::WorkspaceModel { model }
        }
        ClientRequest::Agent { operation: execution::Operation::List, .. } => {
            let agent = execution::Agent { id: ID.parse().unwrap(), name: Some("reviewer".into()), origin: execution::Origin::User,
                placement: execution::Placement::Channel(ID.parse().unwrap()), spec: execution::Spec { provider: Provider::Claude, account: None,
                    target: execution::Target { repository: "/work/repo/.git".into(), directory: "/work/repo".into(), placement: None }, surface: execution::Surface::Chat,
                    model: None, permission_mode: None, reasoning_effort: None }, session: None, console: None, created_ms: 0, closed: false };
            ServerResponse::Agent(execution::Reply::Listed(if populated { vec![execution::Summary { agent, run: RUN.parse().unwrap(), state: execution::State::Failed, reason: Some("exit 3".into()) }] } else { vec![] }))
        }
        ClientRequest::Agent { operation: execution::Operation::Directory(_), .. } => {
            let entry = execution::directory::Entry { id: terminal_id, lineage: execution::Lineage(terminal_id), surface: execution::directory::Surface::Chat,
                provider: Some(Provider::Claude), title: Some("Fixture session".into()), state: cagents::agent::ActivityState::Idle,
                directory: Some("/work/repo".into()), worktree: Some("/work/repo".into()), project: None, project_name: None, delegation: None,
                relation: execution::directory::Relation::Peer, channel: None, view_scope: None, channel_repository: None };
            ServerResponse::Agent(execution::Reply::Directory(execution::directory::Directory { caller: None, scope: execution::directory::Scope::All, sessions: if populated { vec![entry] } else { vec![] } }))
        }
        ClientRequest::Automation { operation: automations::Request::List { .. }, .. } => ServerResponse::Automation(automations::Response::Listed(if populated { vec![automations::Summary {
            id: ID.parse().unwrap(), revision: 1, name: "Nightly".into(), provider: Provider::Claude, directory: "/work/repo".into(), enabled: true, next: None, active_firing: None, last_state: None, blocked: None,
        }] } else { vec![] })),
        ClientRequest::Automation { operation: automations::Request::History { .. }, .. } => ServerResponse::Automation(automations::Response::History(automations::HistoryPage { firings: if populated { vec![automations::Firing {
            id: ID.parse().unwrap(), automation: ID.parse().unwrap(), name: "Nightly".into(), revision: 1, trigger: automations::Trigger::Manual { request: ID.parse().unwrap() },
            state: automations::FiringState::Skipped, release_pending: false, cancel_requested: false, created_ms: cagents::chat::now_ms().unwrap(), started_ms: None,
            finished_ms: None, agent: None, run: None, skip: Some(automations::SkipReason::Capacity), diagnostic: None,
        }] } else { vec![] }, next: None })),
        ClientRequest::DeviceOperation { operation: cagents::device_service::Operation::AccountsRead } => ServerResponse::DeviceOperation { reply: cagents::device_service::Reply::Accounts(json!({"installed":{"Claude":populated},"registry":{"revision":1,"account":[]}})) },
        other => panic!("Unexpected list fixture request: {other:?}"),
    }
}

fn cells(output: &str) -> Vec<Vec<&str>> { output.lines().map(|line| line.split("  ").filter(|cell| !cell.is_empty()).map(str::trim).collect()).collect() }
#[test]
fn every_cli_list_has_a_populated_and_empty_human_snapshot() {
    let populated = Fixture::new(true); let empty = Fixture::new(false);
    for (args, empty_line, expected) in [
        (vec!["agent", "list"], "No open agents", vec![vec!["ID", "PROVIDER", "STATE", "RUN", "DIRECTORY", "NAME"], vec!["11111111", "claude", "failed (exit 3)", "22222222", "/work/repo", "reviewer"]]),
        (vec!["automation", "list"], "No automations", vec![vec!["ID", "STATE", "NAME", "PROVIDER", "NEXT"], vec![ID, "enabled", "Nightly", "claude", "unscheduled"]]),
        (vec!["channel", "list"], "No channels", vec![vec!["ID", "PROJECT", "TITLE"], vec![ID, "/work/repo", "Fixture channel"]]),
        (vec!["terminal", "list", "--all"], "No terminals", vec![vec!["ID", "STATE", "TITLE", "PROGRAM", "DIRECTORY"], vec![ID, "running", "Fixture shell", "bash", "/work/repo"]]),
        (vec!["project", "list"], "No open projects", vec![vec!["PATH", "NAME"], vec!["/work/repo", "Fixture project"]]),
        (vec!["sessions"], "No live sessions", vec![vec!["ID", "PROVIDER", "STATE", "DIRECTORY", "TITLE"], vec![ID, "claude", "idle", "/work/repo", "Fixture session"]]),
        (vec!["account", "list"], "No configured accounts", vec![vec!["ID", "LABEL", "DEFAULT", "STATUS"], vec!["claude/system", "System account", "selected", "available"]]),
    ] {
        assert_eq!(empty.human(&args), empty_line, "{args:?}");
        assert_eq!(cells(&populated.human(&args)), expected, "{args:?}");
    }
    assert_eq!(empty.human(&["automation", "history"]), "No automation runs");
    let history = populated.human(&["automation", "history"]);
    assert_eq!(&cells(&history)[0], &["ID", "NAME", "STATE", "CREATED", "DETAIL"]);
    assert!(history.contains("Nightly") && history.contains("skipped") && history.contains("capacity"), "{history}");
}

#[test]
fn actual_cli_json_normalizes_typed_values_and_keeps_terminal_lists_lean() {
    let fixture = Fixture::new(true);
    let agents = fixture.json(&["agent", "list"]);
    let agent = &agents["data"]["agents"][0];
    assert_eq!(agent["agent"]["spec"]["provider"], "claude");
    assert_eq!(agent["agent"]["spec"]["surface"], "chat");
    assert_eq!(agent["state"], "failed");
    assert_eq!(agent["reason"], "exit 3");
    let sessions = fixture.json(&["sessions"]);
    assert_eq!(sessions["data"]["scope"], "all");
    assert_eq!(sessions["data"]["sessions"][0]["relation"], "peer");
    let terminals = fixture.json(&["terminal", "list", "--all"]);
    let terminal = &terminals["data"]["terminals"][0];
    assert_eq!(terminal["state"], "running");
    for forbidden in ["environment", "resume_hint", "continuity", "launch"] {
        assert!(!terminal.to_string().contains(forbidden));
    }
    let show = fixture.json(&["terminal", "show", ID]);
    assert!(show["data"]["terminal"]["session"]["launch"]["environment"].is_object());
    assert!(show["data"]["terminal"]["session"].get("resume_hint").is_some());
}
