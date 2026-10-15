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
    isolated_with_environment(&[])
}

fn isolated_with_environment(environment: &[(&str, &str)]) -> bool {
    const CHILD: &str = "CAGENTS_PAIRED_UPGRADE_CONTRACT";
    let exact = thread::current().name().unwrap().to_owned();
    if std::env::var(CHILD).as_deref() == Ok(exact.as_str()) {
        return false;
    }
    let root = tempfile::Builder::new()
        .prefix("cagents-paired-upgrade-")
        .tempdir_in("/var/tmp")
        .unwrap()
        .keep();
    for name in ["home", "run", "data", "config", "cache", "state"] {
        fs::create_dir(root.join(name)).unwrap();
        fs::set_permissions(root.join(name), fs::Permissions::from_mode(0o700)).unwrap();
    }
    eprintln!(
        "paired upgrade owning evidence retained at {}",
        root.display()
    );
    let mut command = CommandSpec::new(std::env::current_exe().unwrap())
        .args(["--exact", &exact, "--nocapture", "--test-threads=1"])
        .env(CHILD, &exact)
        .env("HOME", root.join("home"))
        .env("XDG_RUNTIME_DIR", root.join("run"))
        .env("XDG_DATA_HOME", root.join("data"))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_CACHE_HOME", root.join("cache"))
        .env("XDG_STATE_HOME", root.join("state"))
        .env("PATH", "/usr/bin:/bin")
        .env_remove("CODEX_HOME")
        .env_remove("CLAUDE_CONFIG_DIR")
        .env_remove("PI_CODING_AGENT_DIR")
        .env_remove("OMP_PROFILE")
        .env_remove("PI_PROFILE")
        .env_remove("PI_CONFIG_DIR")
        .timeout(Duration::from_secs(60))
        .output_limits(128 * 1024, 128 * 1024);
    for name in crate::account::credential_overrides(Provider::Codex) {
        command = command.env_remove(*name);
    }
    for (name, value) in environment {
        command = command.env(*name, *value);
    }
    let output = SystemProcessExecutor
        .execute(&command, &CancellationToken::new())
        .unwrap();
    fs::write(root.join("stdout.log"), &output.stdout).unwrap();
    fs::write(root.join("stderr.log"), &output.stderr).unwrap();
    let output = output.require_success(&command.program).unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.lines().any(|line| line == "running 1 test")
            && stdout
                .lines()
                .any(|line| line == format!("test {exact} ... ok"))
            && stdout
                .lines()
                .any(|line| line.starts_with("test result: ok. 1 passed; 0 failed;")),
        "owning child did not execute its exact contract; evidence {}",
        root.display()
    );
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
                assert!(
                    Instant::now() < deadline,
                    "paired Codex peer exceeded its deadline"
                );
                match listener.accept() {
                    Ok((stream, _)) => {
                        stream
                            .set_read_timeout(Some(Duration::from_secs(1)))
                            .unwrap();
                        stream
                            .set_write_timeout(Some(Duration::from_secs(1)))
                            .unwrap();
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
        Self {
            stop,
            complete,
            thread,
        }
    }

    fn finish(self) -> Vec<String> {
        self.stop.store(true, Ordering::Release);
        let methods = self
            .complete
            .recv_timeout(Duration::from_secs(3))
            .expect("peer did not stop");
        self.thread.join().unwrap();
        methods
    }
}

fn executable(root: &Path) -> PathBuf {
    let source = root.join("codex.c");
    let executable = root.join("codex");
    // A real foreground native process accepts canonical provider argv and
    // waits on its PTY. The private Rust peer owns the exact shared thread.
    fs::write(&source, "#include <unistd.h>\n#include <stdio.h>\n#include <stdlib.h>\n#include <string.h>\nint main(int argc, char **argv) { if (argc > 1 && strcmp(argv[1], \"resume\") == 0) { if (getenv(\"CODEX_API_KEY\")) return 3; puts(\"effective-recipe-ready\"); fflush(stdout); } char byte; while (read(0, &byte, 1) > 0) {} return 0; }\n").unwrap();
    let command = CommandSpec::new("cc")
        .args([source.to_str().unwrap(), "-o", executable.to_str().unwrap()])
        .timeout(Duration::from_secs(10))
        .output_limits(16 * 1024, 16 * 1024);
    SystemProcessExecutor
        .execute(&command, &CancellationToken::new())
        .unwrap()
        .require_success(&command.program)
        .unwrap();
    executable
}

fn run_pair(mode: &str) {
    let root = tempfile::Builder::new()
        .prefix("paired-source-")
        .tempdir()
        .unwrap()
        .keep();
    let cwd = root.canonicalize().unwrap();
    let executable = executable(&cwd);
    let home = cwd.join("codex-home");
    fs::create_dir(&home).unwrap();
    let socket = cwd.join("codex.sock");
    let peer = Peer::start(&socket, home.clone(), cwd.clone());
    // Select the complete fixture runtime configuration before publishing the
    // terminal. Foreground observation includes these account/runtime fields;
    // an omitted inherited HOME or PATH would itself be a genuine recipe drift.
    let mut environment: BTreeMap<String, String> = [
        "HOME",
        "PATH",
        "XDG_CONFIG_HOME",
        "XDG_DATA_HOME",
        "XDG_STATE_HOME",
        "XDG_CACHE_HOME",
    ]
    .into_iter()
    .map(|name| (name.to_owned(), std::env::var(name).unwrap()))
    .collect();
    if let Some(path) =
        crate::agent_execution::bundled::matched_path(environment.get("PATH").map(String::as_str))
    {
        environment.insert("PATH".into(), path);
    }
    environment.insert("CODEX_HOME".into(), home.display().to_string());
    let fresh = LaunchRecipe {
        program: executable.to_string_lossy().into_owned(),
        arguments: vec!["--remote".into(), format!("unix://{}", socket.display())],
        working_directory: cwd.clone(),
        environment,
        unset_environment: Default::default(),
    };
    let exact = crate::model::agent_recovery::bind_session(&fresh, ID).unwrap();
    let issued = match mode {
        "exact" => exact.clone(),
        "fresh" | "fresh-chat" => fresh.clone(),
        "shell" => LaunchRecipe {
            program: "/bin/bash".into(),
            arguments: vec![
                "-c".into(),
                format!(
                    "exec '{}' --remote 'unix://{}'",
                    executable.display(),
                    socket.display()
                ),
            ],
            ..fresh.clone()
        },
        _ => unreachable!(),
    };
    let original = host("original");
    let id = TerminalId::new();
    let terminal = TerminalSession {
        id,
        launch: issued.clone(),
        transcript_path: None,
        resume_hint: None,
        owned_codex_listener: None,
        recovery_origin: None,
        predecessor: None,
        continuity: TerminalContinuity::RelaunchAvailable {
            recipe: issued,
            last_exit: None,
        },
    };
    spawn_terminal(&original, 1, terminal, TerminalSpawnOptions::default()).unwrap();
    let terminal = original.lock().unwrap().terminals[&id].handle.clone();
    let deadline = Instant::now() + Duration::from_secs(5);
    while terminal.observe_agent_process().is_none() {
        assert!(
            Instant::now() < deadline,
            "Codex source did not become foreground"
        );
        thread::sleep(Duration::from_millis(10));
    }
    let proof = original.lock().unwrap().proof().clone();
    let request = Session {
        id,
        launch: Launch {
            provider: Provider::Codex,
            recipe: exact,
            session_id: Some(ID.into()),
        },
        proof: Some(proof.clone()),
        terminal: None,
        owned_codex_listener: None,
        native_continuity: NativeContinuity::ReconnectOnly,
    };
    let preview = super::super::chat::process(
        &original,
        1,
        ClientRequest::ChatPreview {
            session: request.clone(),
            source: proof.clone(),
        },
    )
    .unwrap();
    let ServerResponse::ChatPreviewed { preview } = preview else {
        panic!("missing exact preview")
    };
    let attached = super::super::chat::process(
        &original,
        1,
        ClientRequest::ChatStartSelected {
            session: request,
            source: proof,
            token: preview.token,
        },
    )
    .unwrap();
    let ServerResponse::ChatAttached {
        session: initial, ..
    } = attached
    else {
        panic!("missing paired attachment")
    };
    assert_eq!(
        initial.terminal.as_ref().unwrap().launch,
        original.lock().unwrap().terminals[&id].session.launch
    );
    assert_eq!(
        initial.terminal.as_ref().unwrap().continuity.live_proof(),
        Some(original.lock().unwrap().proof().clone())
    );
    let actor = original.lock().unwrap().chats[&id].handle.clone();
    let deadline = Instant::now() + Duration::from_secs(5);
    while actor.confirmed_session_id().as_deref() != Some(ID) {
        assert!(
            Instant::now() < deadline,
            "paired actor did not confirm its thread"
        );
        thread::sleep(Duration::from_millis(10));
    }
    // Observe the descriptor through the public attachment after the real
    // confirmation, rather than inventing a learned identity in the fixture.
    let deadline = Instant::now() + Duration::from_secs(5);
    let saved = loop {
        let attached = super::super::chat::process(
            &original,
            1,
            ClientRequest::ChatAttach {
                session: initial.clone(),
            },
        )
        .unwrap();
        let ServerResponse::ChatAttached { session, snapshot } = attached else {
            panic!("missing confirmed pair")
        };
        if session.launch.session_id.as_deref() == Some(ID)
            && snapshot.ready
            && !snapshot.history_loading
        {
            break session;
        }
        assert!(
            Instant::now() < deadline,
            "public pair attachment did not become ready"
        );
        thread::sleep(Duration::from_millis(10));
    };
    assert_eq!(saved.launch.session_id.as_deref(), Some(ID));
    let store_path = cwd.join("checkpoint");
    let capability = original
        .lock()
        .unwrap()
        .shutdown_preflight(Instant::now())
        .unwrap()
        .capability;
    let prepared = prepare_upgrade_with_policy(&original, capability, false, None, || {
        Store::open(&store_path)
    });
    assert_eq!(prepared.unwrap(), (1, 1));
    let prepared = Store::open(&store_path).unwrap().load().unwrap().unwrap();
    assert_eq!(prepared.chats[0].source, saved);
    let mut checkpoint_source = prepared.terminals[0].source.clone();
    checkpoint_source.continuity = saved.terminal.as_ref().unwrap().continuity.clone();
    assert_eq!(checkpoint_source, *saved.terminal.as_ref().unwrap());
    let expected_execution = saved.launch.terminal_recipe().unwrap();
    assert_eq!(prepared.terminals[0].recipe, expected_execution);
    if mode != "exact" {
        assert_ne!(prepared.terminals[0].source.launch, expected_execution);
    }
    finish_upgrade(&original).unwrap();
    assert!(actor.is_completed());
    assert!(terminal.is_completed());
    let mut source = saved;
    if mode == "fresh-chat" {
        // Compatibility input, not a claim that today's selected conversion
        // emits this historical Chat recipe. Its source/pair/process are real;
        // encode the old issued fresh Chat recipe separately from execution.
        let store = Store::open(&store_path).unwrap();
        let mut checkpoint = store.load().unwrap().unwrap();
        checkpoint.chats[0].source.launch.recipe = fresh;
        source = checkpoint.chats[0].source.clone();
        assert_ne!(source.launch.recipe, checkpoint.chats[0].launch.recipe);
        store.save(&checkpoint).unwrap();
    }
    if mode == "shell" {
        // Older source daemons wrote the confirmed execution hint into the
        // terminal checkpoint while the chat retained its issued shell tuple.
        // Exercise that exact old encoding without changing the saved pane.
        let store = Store::open(&store_path).unwrap();
        let mut checkpoint = store.load().unwrap().unwrap();
        checkpoint.terminals[0].source.resume_hint =
            crate::model::agent_recovery::hint_from_launch(&expected_execution);
        assert_ne!(checkpoint.terminals[0].source.resume_hint,
            checkpoint.chats[0].source.terminal.as_ref().unwrap().resume_hint);
        store.save(&checkpoint).unwrap();
        for mutation in 0..5 {
            let mut changed = checkpoint.clone();
            match mutation {
                0 => changed.terminals[0].source.launch.arguments.push("different".into()),
                1 => changed.terminals[0].source.launch.working_directory = "/different".into(),
                2 => {
                    changed.terminals[0].source.launch.environment.insert("CODEX_HOME".into(), "/different".into());
                }
                3 => changed.terminals[0].source.resume_hint = Some("unqualified observation".into()),
                _ => {
                    let mut wrong = expected_execution.clone();
                    wrong.arguments.extend(["--model".into(), "another".into()]);
                    changed.terminals[0].source.resume_hint =
                        crate::model::agent_recovery::hint_from_launch(&wrong);
                }
            }
            assert!(changed.validate().is_err(), "old encoding cannot change pair identity or execution");
        }
    }
    let proc_root = cwd.join("empty-proc");
    fs::create_dir(&proc_root).unwrap();
    for generation in 1..=2 {
        let replacement = host(&format!("replacement-{generation}"));
        restore_upgrade_from(
            &replacement,
            Store::open(&store_path).unwrap(),
            &proc_root,
            None,
        ).unwrap();
        let resolved = super::super::chat::resolve_upgrade(&replacement, &source).unwrap();
        let attached = super::super::chat::process(
            &replacement, 1, ClientRequest::ChatAttach { session: source.clone() },
        ).unwrap();
        let ServerResponse::ChatAttached { session, .. } = attached else {
            panic!("upgraded pair did not attach through the public dispatcher")
        };
        assert_eq!(session, resolved);
        assert!(crate::chat::is_upgrade_recovery(&source, &resolved));
        assert_eq!(resolved.launch.session_id.as_deref(), Some(ID));
        let mut paired = resolved.terminal.as_ref().unwrap().clone();
        paired.continuity = source.terminal.as_ref().unwrap().continuity.clone();
        assert_eq!(&paired, source.terminal.as_ref().unwrap());
        let current_terminal = replacement.lock().unwrap().terminals[&id].handle.clone();
        let current_actor = replacement.lock().unwrap().chats[&id].handle.clone();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(process) = current_terminal.observe_agent_process() {
                assert_eq!(crate::model::agent_recovery::resume_recipe(&process.recovery).as_ref(), Some(&expected_execution));
                break;
            }
            assert!(Instant::now() < deadline, "restored CLI execution was not observed");
            thread::sleep(Duration::from_millis(10));
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        let snapshot = loop {
            let snapshot = current_actor.snapshot(None).unwrap();
            if snapshot.ready && !snapshot.history_loading {
                break snapshot;
            }
            assert!(Instant::now() < deadline, "restored paired history did not load");
            thread::sleep(Duration::from_millis(10));
        };
        assert!(snapshot.items.iter().any(|item| matches!(&item.content,
            crate::chat::ItemContent::Text { text, .. } if text == "existing paired answer")));
        if generation == 1 {
            let capability = replacement.lock().unwrap().shutdown_preflight(Instant::now()).unwrap().capability;
            prepare_upgrade_with_policy(&replacement, capability, false, None, || Store::open(&store_path)).unwrap();
            finish_upgrade(&replacement).unwrap();
            assert!(current_terminal.is_completed());
            assert!(current_actor.is_completed());
            source = resolved;
        } else {
            super::super::chat::close_for_terminal(&replacement, id).unwrap();
            current_terminal.shutdown().unwrap();
        }
    }
    let methods = peer.finish();
    assert!(methods.iter().any(|method| method == "thread/resume"));
    assert!(!methods.iter().any(|method| method == "turn/start"));
}

#[test]
fn legacy_paired_fresh_and_shell_recipes_preserve_issued_identity_across_two_restarts() {
    if isolated() {
        return;
    }
    run_pair("fresh");
    run_pair("shell");
}

#[test]
fn legacy_paired_exact_recipe_survives_checkpoint_restore_and_strict_client_identity() {
    if isolated() {
        return;
    }
    run_pair("exact");
}

#[test]
fn legacy_paired_fresh_chat_checkpoint_preserves_both_issued_recipes() {
    if isolated() {
        return;
    }
    run_pair("fresh-chat");
}

#[test]
fn paired_effective_execution_survives_actual_holder_adoption_without_restarting_root() {
    if isolated_with_environment(&[("CODEX_API_KEY", "synthetic-inherited-fixture-key")]) {
        return;
    }
    let root = tempfile::Builder::new().prefix("paired-holder-").tempdir().unwrap().keep();
    let executable = executable(&root);
    let home = root.join("codex-home");
    fs::create_dir(&home).unwrap();
    let paths = crate::daemon::RuntimePaths::discover().unwrap();
    let boot = crate::daemon::read_boot_id().unwrap();
    let make_proof = |generation| HostSessionProof {
        host_session_id: HostSessionId::new(format!("paired-holder-{generation}")),
        boot_id: boot.clone(),
        host_incarnation: HostIncarnation::new(format!("paired-holder-{generation}")),
    };
    let continuity = |proof: &HostSessionProof| TerminalContinuity::DetachedSameBoot {
        host_session_id: proof.host_session_id.clone(),
        boot_id: proof.boot_id.clone(),
        host_incarnation: proof.host_incarnation.clone(),
    };
    let mut proof = make_proof(0);
    let mut lease = Some(crate::daemon::HostLease::acquire(&paths, &proof).unwrap());
    let issued = TerminalSession {
        id: TerminalId::new(),
        launch: LaunchRecipe {
            program: "/bin/sh".into(),
            arguments: vec!["-c".into(), "printf wrong-issued-recipe; exit 4".into()],
            working_directory: root.clone(),
            environment: Default::default(),
            unset_environment: Default::default(),
        },
        transcript_path: None,
        resume_hint: None,
        owned_codex_listener: None,
        recovery_origin: None,
        predecessor: None,
        continuity: continuity(&proof),
    };
    let effective = LaunchRecipe {
        program: executable.to_string_lossy().into_owned(),
        arguments: vec!["resume".into(), ID.into()],
        working_directory: root.clone(),
        environment: [("CODEX_HOME".into(), home.to_string_lossy().into_owned())].into(),
        unset_environment: ["CODEX_API_KEY".into()].into(),
    };
    let recovery = crate::terminal::holder::Recovery {
        source: issued.clone(),
        previous_sources: Vec::new(),
        relaunches: Vec::new(),
        options: Default::default(),
        execution_environment: None,
        execution_recipe: Some(effective.clone()),
    };
    let mut handle = TerminalHandle::spawn(issued.clone(), TerminalConfig {
        execution_recipe: Some(effective.clone()),
        holder_recovery: Some(recovery),
        initially_visible: false,
        ..Default::default()
    }, |_| {}).unwrap();
    let original = handle.holder_identity().unwrap().clone();
    let socket = crate::terminal::holder::socket(&crate::terminal::holder::directory().unwrap(), issued.id);
    for generation in 0..=2 {
        assert_eq!(handle.holder_identity(), Some(&original));
        handle.reveal().unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let snapshot = handle.snapshot();
            handle.acknowledge_redraw(snapshot.generation);
            if snapshot.lines.iter().any(|row| row.cells.iter().map(|cell| cell.text.as_str()).collect::<String>().contains("effective-recipe-ready")) {
                break;
            }
            assert!(Instant::now() < deadline, "exact execution/unset environment did not become ready");
            thread::sleep(Duration::from_millis(10));
        }
        assert!(original.root.is_current());
        if generation == 2 {
            handle.shutdown().unwrap();
            assert!(crate::terminal::holder::discover(&crate::terminal::holder::directory().unwrap(), &boot).unwrap().is_empty());
            break;
        }
        handle.carry(socket.clone()).unwrap();
        drop(handle);
        drop(lease.take());
        proof = make_proof(generation + 1);
        lease = Some(crate::daemon::HostLease::acquire(&paths, &proof).unwrap());
        let (mut recovery, _) = crate::terminal::inspect_holder(&socket, &original, &proof).unwrap();
        assert_eq!(recovery.execution_recipe.as_ref(), Some(&effective));
        assert_eq!(recovery.source.launch, issued.launch);
        assert_eq!(recovery.source.resume_hint, issued.resume_hint);
        recovery.previous_sources.push(recovery.source.clone());
        recovery.source.continuity = continuity(&proof);
        let mut changed = recovery.clone();
        changed.execution_recipe = None;
        assert!(TerminalHandle::spawn(changed.source.clone(), TerminalConfig {
            execution_recipe: Some(effective.clone()),
            holder_recovery: Some(changed),
            adoption: Some(socket.clone()),
            initially_visible: false,
            ..Default::default()
        }, |_| {}).is_err(), "an older/mismatched context must not be adopted");
        let (retained, _) = crate::terminal::inspect_holder(&socket, &original, &proof).unwrap();
        assert_eq!(retained.execution_recipe.as_ref(), Some(&effective));
        assert!(original.root.is_current());
        handle = TerminalHandle::spawn(recovery.source.clone(), TerminalConfig {
            execution_recipe: recovery.execution_recipe.clone(),
            holder_recovery: Some(recovery),
            adoption: Some(socket.clone()),
            initially_visible: false,
            ..Default::default()
        }, |_| {}).unwrap();
    }
}
