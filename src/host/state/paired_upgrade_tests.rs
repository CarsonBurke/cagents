//! Real legacy Codex pairs, including the terminal descriptor issued at conversion.
use super::*;
use crate::{
    agent::Provider,
    chat::{Launch, NativeContinuity, Session},
    model::{BootId, HostIncarnation, HostSessionId, LaunchRecipe},
    repository::{CancellationToken, CommandSpec, ProcessExecutor, SystemProcessExecutor},
    terminal_ipc::{ClientRequest, ServerResponse},
    upgrade::handoff::Store,
};
use std::{
    os::unix::{fs::PermissionsExt, net::UnixListener},
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
};

const ID: &str = "12345678-1234-4234-8234-123456789abc";

fn isolated() -> bool {
    const CHILD: &str = "CAGENTS_PAIRED_UPGRADE_CONTRACT";
    let exact = thread::current().name().unwrap().to_owned();
    if std::env::var(CHILD).as_deref() == Ok(exact.as_str()) {
        return false;
    }
    let root = tempfile::Builder::new().prefix("cagents-paired-upgrade-")
        .tempdir_in("/var/tmp").unwrap().keep();
    for name in ["home", "run", "data", "config", "cache"] {
        fs::create_dir(root.join(name)).unwrap();
        fs::set_permissions(root.join(name), fs::Permissions::from_mode(0o700)).unwrap();
    }
    eprintln!("paired upgrade owning evidence retained at {}", root.display());
    let command = CommandSpec::new(std::env::current_exe().unwrap())
        .args(["--exact", &exact, "--nocapture", "--test-threads=1"])
        .env(CHILD, &exact).env("HOME", root.join("home"))
        .env("XDG_RUNTIME_DIR", root.join("run"))
        .env("XDG_DATA_HOME", root.join("data"))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_CACHE_HOME", root.join("cache"))
        .timeout(Duration::from_secs(60)).output_limits(128 * 1024, 128 * 1024);
    let output = SystemProcessExecutor.execute(&command, &CancellationToken::new()).unwrap();
    fs::write(root.join("stdout.log"), &output.stdout).unwrap();
    fs::write(root.join("stderr.log"), &output.stderr).unwrap();
    let output = output.require_success(&command.program).unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.lines().any(|line| line == "running 1 test")
        && stdout.lines().any(|line| line == format!("test {exact} ... ok"))
        && stdout.lines().any(|line| line.starts_with("test result: ok. 1 passed; 0 failed;")),
        "owning child did not execute its exact contract; evidence {}", root.display());
    true
}

fn host(incarnation: &str) -> SharedHost {
    Arc::new(HostLock::new(HostState::new(HostSessionProof {
        host_session_id: HostSessionId::new("paired-upgrade"),
        boot_id: BootId::new("fixture-boot"),
        host_incarnation: HostIncarnation::new(incarnation),
    })))
}

struct Peer {
    stop: Arc<AtomicBool>,
    complete: std::sync::mpsc::Receiver<Vec<String>>,
    thread: thread::JoinHandle<()>,
}

impl Peer {
    fn start(socket: &Path, home: PathBuf, cwd: PathBuf) -> Self {
        let listener = UnixListener::bind(socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = stop.clone();
        let (finished, complete) = std::sync::mpsc::channel();
        let thread = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(50);
            let mut clients = Vec::new();
            let mut methods = Vec::new();
            while !stopping.load(Ordering::Acquire) {
                assert!(Instant::now() < deadline, "paired Codex peer exceeded its deadline");
                match listener.accept() {
                    Ok((stream, _)) => {
                        stream.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
                        stream.set_write_timeout(Some(Duration::from_secs(1))).unwrap();
                        let client = tungstenite::accept(stream).unwrap();
                        client.get_ref().set_nonblocking(true).unwrap();
                        clients.push(client);
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                    Err(error) => panic!("paired Codex accept: {error}"),
                }
                clients.retain_mut(|client| {
                    let text = match client.read() {
                        Ok(tungstenite::Message::Text(text)) => text,
                        Ok(_) => return true,
                        Err(tungstenite::Error::Io(error)) if error.kind() == std::io::ErrorKind::WouldBlock => return true,
                        Err(_) => return false,
                    };
                    let frame: serde_json::Value = serde_json::from_str(&text).unwrap();
                    let method = frame["method"].as_str().unwrap();
                    methods.push(method.to_owned());
                    if frame.get("id").is_none() {
                        assert_eq!(method, "initialized");
                        return true;
                    }
                    let result = match method {
                        "initialize" => serde_json::json!({"userAgent":"cagents/0.1.0","codexHome":home}),
                        "thread/loaded/list" => serde_json::json!({"data":[ID],"nextCursor":null}),
                        "thread/read" => {
                            assert_eq!(frame["params"]["threadId"], ID);
                            serde_json::json!({"thread":{"id":ID,"name":"Existing paired thread","cwd":cwd,"status":{"type":"idle"}}})
                        }
                        "thread/resume" => {
                            assert_eq!(frame["params"]["threadId"], ID);
                            serde_json::json!({"thread":{"id":ID,"canAcceptDirectInput":true,"status":{"type":"idle"}},
                                "model":"fixture-luna","initialTurnsPage":{"data":[{"id":"saved-turn","status":"completed","items":[
                                    {"type":"userMessage","id":"old-prompt","content":[{"type":"text","text":"existing paired prompt"}]},
                                    {"type":"agentMessage","id":"old-answer","text":"existing paired answer","phase":"final_answer"}]}],"nextCursor":null}})
                        }
                        "model/list" | "skills/list" | "thread/list" | "thread/queue/list" => serde_json::json!({"data":[],"nextCursor":null}),
                        _ => panic!("unrequested provider work: {method}"),
                    };
                    client.send(tungstenite::Message::Text(serde_json::json!({"id":frame["id"],"result":result}).to_string().into())).is_ok()
                });
                thread::sleep(Duration::from_millis(2));
            }
            drop(clients);
            finished.send(methods).unwrap();
        });
        Self { stop, complete, thread }
    }

    fn finish(self) -> Vec<String> {
        self.stop.store(true, Ordering::Release);
        let methods = self.complete.recv_timeout(Duration::from_secs(3)).expect("peer did not stop");
        self.thread.join().unwrap();
        methods
    }
}

fn executable(root: &Path) -> PathBuf {
    let source = root.join("codex.c");
    let executable = root.join("codex");
    // A real foreground native process accepts canonical provider argv and
    // waits on its PTY. The private Rust peer owns the exact shared thread.
    fs::write(&source, "#include <unistd.h>\nint main(void) { char byte; while (read(0, &byte, 1) > 0) {} return 0; }\n").unwrap();
    let command = CommandSpec::new("cc")
        .args([source.to_str().unwrap(), "-o", executable.to_str().unwrap()])
        .timeout(Duration::from_secs(10)).output_limits(16 * 1024, 16 * 1024);
    SystemProcessExecutor.execute(&command, &CancellationToken::new()).unwrap()
        .require_success(&command.program).unwrap();
    executable
}

fn run_pair(mode: &str) {
    let root = tempfile::Builder::new().prefix("paired-source-").tempdir().unwrap().keep();
    let cwd = root.canonicalize().unwrap();
    let executable = executable(&cwd);
    let home = cwd.join("codex-home");
    fs::create_dir(&home).unwrap();
    let socket = cwd.join("codex.sock");
    let peer = Peer::start(&socket, home.clone(), cwd.clone());
    let fresh = LaunchRecipe {
        program: executable.to_string_lossy().into_owned(),
        arguments: vec!["--remote".into(), format!("unix://{}", socket.display())],
        working_directory: cwd.clone(),
        environment: BTreeMap::from([("CODEX_HOME".into(), home.display().to_string())]),
        unset_environment: Default::default(),
    };
    let exact = crate::model::agent_recovery::bind_session(&fresh, ID).unwrap();
    let issued = match mode {
        "exact" => exact.clone(),
        "fresh" => fresh.clone(),
        "shell" => LaunchRecipe {
            program: "/bin/bash".into(),
            arguments: vec!["-c".into(), format!("exec '{}' --remote 'unix://{}'", executable.display(), socket.display())],
            ..fresh.clone()
        },
        _ => unreachable!(),
    };
    let original = host("original");
    let id = TerminalId::new();
    let terminal = TerminalSession {
        id, launch: issued.clone(), transcript_path: None, resume_hint: None,
        owned_codex_listener: None, recovery_origin: None, predecessor: None,
        continuity: TerminalContinuity::RelaunchAvailable { recipe: issued, last_exit: None },
    };
    spawn_terminal(&original, 1, terminal, TerminalSpawnOptions::default()).unwrap();
    let terminal = original.lock().unwrap().terminals[&id].handle.clone();
    let deadline = Instant::now() + Duration::from_secs(5);
    while terminal.observe_agent_process().is_none() {
        assert!(Instant::now() < deadline, "Codex source did not become foreground");
        thread::sleep(Duration::from_millis(10));
    }
    let proof = original.lock().unwrap().proof().clone();
    let request = Session { id, launch: Launch { provider: Provider::Codex, recipe: exact, session_id: Some(ID.into()) },
        proof: Some(proof.clone()), terminal: None, owned_codex_listener: None, native_continuity: NativeContinuity::ReconnectOnly };
    let preview = super::super::chat::process(&original, 1, ClientRequest::ChatPreview { session: request.clone(), source: proof.clone() }).unwrap();
    let ServerResponse::ChatPreviewed { preview } = preview else { panic!("missing exact preview") };
    let attached = super::super::chat::process(&original, 1, ClientRequest::ChatStartSelected { session: request, source: proof, token: preview.token }).unwrap();
    let ServerResponse::ChatAttached { session: saved, .. } = attached else { panic!("missing paired attachment") };
    assert_eq!(saved.terminal.as_ref().unwrap().launch, original.lock().unwrap().terminals[&id].session.launch);
    let actor = original.lock().unwrap().chats[&id].handle.clone();
    let deadline = Instant::now() + Duration::from_secs(5);
    while actor.confirmed_session_id().as_deref() != Some(ID) {
        assert!(Instant::now() < deadline, "paired actor did not confirm its thread");
        thread::sleep(Duration::from_millis(10));
    }
    let store_path = cwd.join("checkpoint");
    let capability = original.lock().unwrap().shutdown_preflight(Instant::now()).unwrap().capability;
    let prepared = prepare_upgrade_with_policy(&original, capability, false, None, || Store::open(&store_path));
    if mode != "exact" {
        let failure = prepared.unwrap_err();
        assert!(failure.message.contains("Paired Codex terminal needs a different restart recipe"), "{}", failure.message);
        assert!(!original.lock().unwrap().shutting_down);
        assert!(!actor.is_completed());
        assert!(!terminal.is_completed());
        assert!(Store::open(&store_path).unwrap().load().unwrap().is_none());
        super::super::chat::close_for_terminal(&original, id).unwrap();
        terminal.shutdown().unwrap();
    } else {
        assert_eq!(prepared.unwrap(), (1, 1));
        let prepared = Store::open(&store_path).unwrap().load().unwrap().unwrap();
        assert_eq!(prepared.chats[0].source, saved);
        assert_eq!(prepared.terminals[0].recipe, saved.terminal.as_ref().unwrap().launch);
        finish_upgrade(&original).unwrap();
        assert!(actor.is_completed());
        assert!(terminal.is_completed());
        let replacement = host("replacement");
        let proc_root = cwd.join("empty-proc");
        fs::create_dir(&proc_root).unwrap();
        restore_upgrade_from(&replacement, Store::open(&store_path).unwrap(), &proc_root, None).unwrap();
        let resolved = super::super::chat::resolve_upgrade(&replacement, &saved).unwrap();
        assert!(crate::chat::is_upgrade_recovery(&saved, &resolved));
        assert_eq!(resolved.launch.session_id.as_deref(), Some(ID));
        let current_terminal = replacement.lock().unwrap().terminals[&id].handle.clone();
        let current_actor = replacement.lock().unwrap().chats[&id].handle.clone();
        let snapshot = current_actor.snapshot(None).unwrap();
        assert!(snapshot.items.iter().any(|item| matches!(&item.content,
            crate::chat::ItemContent::Text { text, .. } if text == "existing paired answer")));
        super::super::chat::close_for_terminal(&replacement, id).unwrap();
        current_terminal.shutdown().unwrap();
    }
    let methods = peer.finish();
    assert!(methods.iter().any(|method| method == "thread/resume"));
    assert!(!methods.iter().any(|method| method == "turn/start"));
}

#[test]
fn legacy_paired_fresh_and_shell_recipes_refuse_before_source_stop() {
    if isolated() { return; }
    run_pair("fresh");
    run_pair("shell");
}

#[test]
fn legacy_paired_exact_recipe_survives_checkpoint_restore_and_strict_client_identity() {
    if isolated() { return; }
    run_pair("exact");
}
