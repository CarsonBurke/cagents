//! Reopening a saved Codex thread from Resume conversation. The picker issues
//! the thread with a bare `unix://` remote, naming the account's shared daemon.
use super::*;
use crate::host::agent_execution::tests::{fixture, isolated};
use crate::model::LaunchRecipe;
use serde_json::{Value, json};
use std::{
    os::unix::{fs::PermissionsExt, net::UnixListener},
    path::{Path, PathBuf},
    sync::mpsc,
};

/// The saved thread, distinct in each isolated test process: a test that
/// finds live owners by thread must not find another test's stand-ins.
fn thread_id() -> &'static str {
    static ID: std::sync::LazyLock<String> =
        std::sync::LazyLock::new(|| uuid::Uuid::new_v4().to_string());
    &ID
}

/// The saved thread as Resume conversation hands it to the daemon.
fn reopened(root: &Path) -> Session {
    Session {
        id: TerminalId::new(),
        native_continuity: chat::NativeContinuity::ReconnectOnly,
        owned_codex_listener: None,
        proof: None,
        terminal: None,
        launch: chat::Launch {
            provider: Provider::Codex,
            recipe: LaunchRecipe {
                program: root.join("bin/codex").to_str().unwrap().into(),
                arguments: ["resume", thread_id(), "--remote", "unix://"]
                    .map(String::from)
                    .into(),
                working_directory: root.join("project"),
                environment: BTreeMap::new(),
                unset_environment: Default::default(),
            },
            session_id: Some(thread_id().into()),
        },
    }
}

/// A Codex stand-in that records its arguments and stays running.
fn install_codex(root: &Path) {
    let program = root.join("bin/codex");
    std::fs::write(
        &program,
        "#!/bin/sh\nset -eu\nprintf '%s\\n' \"$@\" > \"$HOME/../codex-arguments\"\nexec /bin/sleep 60\n",
    )
    .unwrap();
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
}

/// Serve the saved thread to the native adapter, reporting each method it
/// calls. Connections that only probe whether anything listens are passed over.
fn serve(listener: UnixListener, home: PathBuf, methods: mpsc::Sender<Value>) {
    listener.set_nonblocking(true).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut websocket = loop {
        match listener.accept() {
            Ok((stream, _)) => {
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(10)))
                    .unwrap();
                if let Ok(websocket) = tungstenite::accept(stream) {
                    break websocket;
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(Instant::now() < deadline, "the adapter never connected");
                thread::sleep(Duration::from_millis(10));
            }
            Err(error) => panic!("{error}"),
        }
    };
    loop {
        let message = match websocket.read() {
            Ok(message) if message.is_close() => break,
            Ok(message) => message,
            Err(_) => break,
        };
        let frame: Value = serde_json::from_str(message.to_text().unwrap()).unwrap();
        let Some(id) = frame.get("id").cloned() else {
            continue;
        };
        let thread = json!({"id":thread_id(),"canAcceptDirectInput":true,"status":{"type":"idle"}});
        let result = match frame["method"].as_str().unwrap() {
            "initialize" => json!({"userAgent":"codex/0.166.0","codexHome":home}),
            "thread/resume" => json!({"thread":thread,"model":"fixture-model"}),
            "model/list" | "skills/list" => json!({"data":[]}),
            "thread/queue/list" | "thread/list" | "thread/turns/list" => {
                json!({"data":[],"nextCursor":null})
            }
            "thread/read" => json!({"thread":{"id":thread_id(),"turns":[]}}),
            method => panic!("unexpected Codex request {method}"),
        };
        let _ = methods.send(frame);
        if websocket
            .send(tungstenite::Message::Text(
                json!({"id":id,"result":result}).to_string().into(),
            ))
            .is_err()
        {
            break;
        }
    }
}

/// Wait until the adapter resumes the saved thread on whichever server it joined.
fn resumed(methods: &mpsc::Receiver<Value>) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let frame = methods
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .expect("the saved thread was never resumed");
        if frame["method"] == "thread/resume" {
            assert_eq!(frame["params"]["threadId"], thread_id());
            return;
        }
    }
}

fn started(response: ServerResponse) -> Session {
    match response {
        ServerResponse::ChatAttached { session, .. } => session,
        other => panic!("the reopened thread did not start: {other:?}"),
    }
}

/// With no shared daemon serving the account, nothing owns the saved thread,
/// so it starts on a listener of its own rather than failing to join one.
#[test]
fn a_reopened_codex_thread_nothing_serves_starts_on_its_own_listener() {
    const NAME: &str = "host::chat::unserved_resume_tests::a_reopened_codex_thread_nothing_serves_starts_on_its_own_listener";
    if isolated(NAME).is_some() {
        return;
    }
    let (host, _, path) = fixture();
    let root = path.parent().unwrap().to_owned();
    install_codex(&root);
    // The daemon's own layout, whose listener root does not exist yet. A
    // short path keeps its sockets within an address wherever TMPDIR is.
    let runtime = tempfile::tempdir_in("/tmp").unwrap();
    let layout = crate::host::chat::CodexListenerRoots::in_runtime_directory(runtime.path());
    assert_ne!(
        layout,
        crate::host::chat::CodexListenerRoots::at(std::env::temp_dir())
    );
    host.lock().unwrap().codex_listeners = layout;
    let requested = reopened(&root);
    let socket = runtime
        .path()
        .join("codex")
        .join(format!("cagents-codex-{}", requested.id))
        .join("control.sock");
    let (methods, received) = mpsc::channel();
    let server = {
        let (root, socket) = (root.clone(), socket.clone());
        thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(10);
            let arguments = loop {
                if let Ok(arguments) = std::fs::read_to_string(root.join("codex-arguments")) {
                    break arguments;
                }
                assert!(Instant::now() < deadline, "no Codex server was launched");
                thread::sleep(Duration::from_millis(10));
            };
            assert_eq!(
                arguments.lines().collect::<Vec<_>>(),
                [
                    "app-server",
                    "--listen",
                    &format!("unix://{}", socket.display())
                ]
            );
            serve(
                UnixListener::bind(&socket).unwrap(),
                root.join("home/.codex"),
                methods,
            );
        })
    };
    host.lock().unwrap().register_client(1);
    let session = started(
        start(
            &host,
            1,
            requested.clone(),
            None,
            false,
            None,
            Default::default(),
        )
        .unwrap(),
    );
    resumed(&received);
    assert_eq!(session.id, requested.id);
    assert_eq!(session.launch.session_id.as_deref(), Some(thread_id()));
    assert!(session.owns_codex_listener());
    assert_eq!(
        session.owned_codex_listener.as_ref().unwrap().socket,
        socket
    );
    assert_eq!(
        session.launch.recipe.arguments,
        [
            "resume".into(),
            thread_id().into(),
            "--remote".into(),
            format!("unix://{}", socket.display())
        ]
    );
    // Storage accepts the daemon's resolution as the one change it made.
    assert!(requested.resolved_to_owned_codex_listener(&session));

    // A second start of the same thread is refused rather than becoming
    // another writer on its own listener.
    let refusal = start(
        &host,
        1,
        reopened(&root),
        None,
        false,
        None,
        Default::default(),
    )
    .unwrap_err()
    .message;
    assert!(refusal.contains("already owns work"), "{refusal}");

    crate::host::finish_chat_shutdown(&host).unwrap();
    server.join().unwrap();
}

/// A shared daemon serving the account may own the saved thread, so the
/// reopened thread joins it and no other server is launched.
#[test]
fn a_reopened_codex_thread_joins_the_shared_daemon_serving_it() {
    const NAME: &str = "host::chat::unserved_resume_tests::a_reopened_codex_thread_joins_the_shared_daemon_serving_it";
    if isolated(NAME).is_some() {
        return;
    }
    let (host, _, path) = fixture();
    let root = path.parent().unwrap().to_owned();
    install_codex(&root);
    let home = root.join("home/.codex");
    let control = home.join("app-server-control");
    std::fs::create_dir_all(&control).unwrap();
    let listener = UnixListener::bind(control.join("app-server-control.sock")).unwrap();
    let (methods, received) = mpsc::channel();
    let server = thread::spawn(move || serve(listener, home, methods));
    host.lock().unwrap().register_client(1);
    let requested = reopened(&root);
    let session = started(
        start(
            &host,
            1,
            requested.clone(),
            None,
            false,
            None,
            Default::default(),
        )
        .unwrap(),
    );
    resumed(&received);
    assert!(!session.owns_codex_listener());
    assert_eq!(session.launch.recipe, requested.launch.recipe);
    assert!(!root.join("codex-arguments").exists());
    crate::host::finish_chat_shutdown(&host).unwrap();
    server.join().unwrap();
}

/// A process outside this daemon with the thread loaded, such as a Codex CLI
/// or the desktop app, keeps its rollout open. With the shared daemon stopped,
/// the reopened thread is refused rather than started beside that writer.
#[test]
fn a_reopened_codex_thread_another_process_has_open_is_refused() {
    const NAME: &str = "host::chat::unserved_resume_tests::a_reopened_codex_thread_another_process_has_open_is_refused";
    if isolated(NAME).is_some() {
        return;
    }
    let (host, _, path) = fixture();
    let root = path.parent().unwrap().to_owned();
    install_codex(&root);
    let home = root.join("home/.codex");
    // A shared daemon that stopped without removing its endpoint.
    let control = home.join("app-server-control");
    std::fs::create_dir_all(&control).unwrap();
    drop(UnixListener::bind(control.join("app-server-control.sock")).unwrap());
    let sessions = home.join("sessions/2026/11/08");
    std::fs::create_dir_all(&sessions).unwrap();
    let rollout = sessions.join(format!("rollout-2026-11-08T12-00-00-{}.jsonl", thread_id()));
    std::fs::write(&rollout, b"").unwrap();
    let mut cli = std::process::Command::new("/bin/sh")
        .args(["-c", "exec /bin/sleep 30 3>>\"$0\""])
        .arg(&rollout)
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while super::codex_owners::writer(None, thread_id())
        .unwrap()
        .is_none()
    {
        assert!(
            Instant::now() < deadline,
            "the stand-in never opened its rollout"
        );
        thread::sleep(Duration::from_millis(10));
    }
    host.lock().unwrap().register_client(1);
    let refusal = start(
        &host,
        1,
        reopened(&root),
        None,
        false,
        None,
        Default::default(),
    )
    .unwrap_err()
    .message;
    cli.kill().unwrap();
    cli.wait().unwrap();
    assert!(
        refusal.contains(&format!(
            "pid {}). Close it there, then resume it here.",
            cli.id()
        )),
        "{refusal}"
    );
    assert!(
        !root.join("codex-arguments").exists(),
        "no server was launched"
    );
    assert!(host.lock().unwrap().chat_spawning.is_empty());
}

/// Private app-servers do not publish the shared control socket. A server
/// retaining its writer lock must still prevent a second private server,
/// including when it has no rollout descriptor open at this instant.
#[test]
fn a_reopened_codex_thread_served_on_another_private_listener_is_refused() {
    const NAME: &str = "host::chat::unserved_resume_tests::a_reopened_codex_thread_served_on_another_private_listener_is_refused";
    if isolated(NAME).is_some() {
        return;
    }
    let (host, _, path) = fixture();
    let root = path.parent().unwrap().to_owned();
    install_codex(&root);
    let locks = root.join("home/.codex/thread-writer-locks");
    std::fs::create_dir_all(&locks).unwrap();
    let private = tempfile::tempdir_in("/tmp").unwrap();
    let socket = private.path().join("private.sock");
    let ready = private.path().join("ready");
    let mut server = std::process::Command::new("/usr/bin/python3")
        .args([
            "-c",
            "import fcntl, pathlib, socket, sys, time\n\
             lock = open(sys.argv[1], 'w')\n\
             fcntl.flock(lock, fcntl.LOCK_EX)\n\
             listener = socket.socket(socket.AF_UNIX)\n\
             listener.bind(sys.argv[2])\n\
             listener.listen()\n\
             pathlib.Path(sys.argv[3]).touch()\n\
             time.sleep(30)",
        ])
        .arg(locks.join(format!("{}.lock", thread_id())))
        .arg(&socket)
        .arg(&ready)
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !ready.exists() {
        assert!(
            Instant::now() < deadline,
            "the private server never became ready"
        );
        thread::sleep(Duration::from_millis(10));
    }
    let writer = super::codex_owners::writer(
        Some(&locks.join(format!("{}.lock", thread_id()))),
        thread_id(),
    )
    .unwrap()
    .expect("the private server holds the real kernel writer lock");
    assert_eq!(writer.pid, server.id() as libc::pid_t);
    host.lock().unwrap().register_client(1);
    let result = start(
        &host,
        1,
        reopened(&root),
        None,
        false,
        None,
        Default::default(),
    );
    // Clean up the fixture before checking the result, including a mutant
    // that incorrectly tries to launch the installed Codex stand-in.
    server.kill().unwrap();
    server.wait().unwrap();
    assert!(
        !root.join("codex-arguments").exists(),
        "no second private server was launched"
    );
    let refusal = result.unwrap_err();
    assert!(
        refusal.message.contains(&format!("pid {})", writer.pid)),
        "{refusal:?}"
    );
    assert!(
        !refusal.retryable,
        "a second server cannot help while the writer owns it"
    );
    assert!(host.lock().unwrap().chat_spawning.is_empty());
}
