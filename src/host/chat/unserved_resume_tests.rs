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

/// Real kernel writer ownership, without a rollout FD or provider argv that
/// main's process census could otherwise identify as this thread's execution.
struct ThreadWriter(std::fs::File);

impl ThreadWriter {
    fn hold(home: &Path) -> Self {
        use std::os::fd::AsRawFd;
        let locks = home.join("thread-writer-locks");
        std::fs::create_dir_all(&locks).unwrap();
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(locks.join(format!("{}.lock", thread_id())))
            .unwrap();
        assert_eq!(unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) }, 0);
        assert_eq!(
            codex_owners::writer(
                Some(&locks.join(format!("{}.lock", thread_id()))),
                thread_id()
            )
            .unwrap()
            .unwrap()
            .pid,
            std::process::id() as libc::pid_t,
        );
        Self(file)
    }
}

impl Drop for ThreadWriter {
    fn drop(&mut self) {
        use std::os::fd::AsRawFd;
        assert_eq!(unsafe { libc::flock(self.0.as_raw_fd(), libc::LOCK_UN) }, 0);
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
    let home = root.join("account");
    let _writer = ThreadWriter::hold(&home);
    let control = home.join("app-server-control");
    std::fs::create_dir_all(&control).unwrap();
    let listener = UnixListener::bind(control.join("app-server-control.sock")).unwrap();
    let (methods, received) = mpsc::channel();
    let server = thread::spawn(move || serve(listener, home, methods));
    host.lock().unwrap().register_client(1);
    let mut requested = reopened(&root);
    requested.launch.recipe.environment.insert(
        "CODEX_HOME".into(),
        root.join("account").to_str().unwrap().into(),
    );
    let ServerResponse::ChatResumePrepared { session: prepared } = process(
        &host,
        1,
        ClientRequest::ChatPrepareResume {
            source: requested.clone(),
        },
    )
    .unwrap() else {
        panic!("expected join-only preparation")
    };
    assert_eq!(
        prepared, requested,
        "preparation must not allocate or rewrite a bare source"
    );
    assert!(host.lock().unwrap().chat_spawning.is_empty());
    assert!(host.lock().unwrap().chats.is_empty());
    assert!(!root.join("codex-arguments").exists());
    let mut conflicting = requested.clone();
    conflicting.launch.recipe.arguments[1] = "another-thread".into();
    let refused = process(
        &host,
        1,
        ClientRequest::ChatPrepareResume {
            source: conflicting,
        },
    )
    .unwrap_err();
    assert_eq!(refused.code, ProtocolErrorCode::InvalidRequest);
    assert!(!refused.retryable, "a changed recipe is structural");
    let session = started(
        process(
            &host,
            1,
            ClientRequest::ChatStart {
                session: prepared,
                source: None,
                terminate_source: false,
                execution_environment: Default::default(),
            },
        )
        .unwrap(),
    );
    resumed(&received);
    assert!(
        !root.join("codex-arguments").exists(),
        "a live shared daemon must be joined without launching a private server"
    );
    assert!(!session.owns_codex_listener());
    assert_eq!(session.launch.recipe, requested.launch.recipe);
    crate::host::finish_chat_shutdown(&host).unwrap();
    server.join().unwrap();
}

#[test]
fn a_prepared_codex_resume_rechecks_the_shared_service_before_starting() {
    const NAME: &str = "host::chat::unserved_resume_tests::a_prepared_codex_resume_rechecks_the_shared_service_before_starting";
    if isolated(NAME).is_some() {
        return;
    }
    let (host, _, path) = fixture();
    let root = path.parent().unwrap().to_owned();
    install_codex(&root);
    let home = root.join("home/.codex");
    let writer = ThreadWriter::hold(&home);
    let control = home.join("app-server-control");
    std::fs::create_dir_all(&control).unwrap();
    let shared = control.join("app-server-control.sock");
    let listener = UnixListener::bind(&shared).unwrap();
    host.lock().unwrap().register_client(1);
    let requested = reopened(&root);
    let ServerResponse::ChatResumePrepared { session: prepared } = process(
        &host,
        1,
        ClientRequest::ChatPrepareResume {
            source: requested.clone(),
        },
    )
    .unwrap() else {
        panic!("expected join-only preparation")
    };
    assert_eq!(prepared, requested);
    drop(listener);
    let refusal = process(
        &host,
        1,
        ClientRequest::ChatStart {
            session: prepared,
            source: None,
            terminate_source: false,
            execution_environment: Default::default(),
        },
    )
    .unwrap_err();
    assert!(refusal.retryable, "{refusal:?}");
    assert_eq!(
        refusal.message,
        format!(
            "Cannot restart Codex: {}",
            running_codex_process(std::process::id())
        )
    );
    let refuse_prepare = || {
        process(
            &host,
            1,
            ClientRequest::ChatPrepareResume {
                source: requested.clone(),
            },
        )
        .unwrap_err()
    };
    let prepare = refuse_prepare();
    assert_eq!(prepare.code, ProtocolErrorCode::TerminalUnavailable);
    assert!(
        prepare.retryable,
        "a live writer can end before Retry: {prepare:?}"
    );
    assert_eq!(prepare.message, running_codex_process(std::process::id()));
    assert_eq!(
        refuse_prepare(),
        prepare,
        "Retry while held repeats the same refusal"
    );
    assert!(
        !root.join("codex-arguments").exists(),
        "no private replacement was launched"
    );
    assert!(host.lock().unwrap().chat_spawning.is_empty());
    assert!(host.lock().unwrap().chats.is_empty());
    drop(writer);
    assert!(
        matches!(
            process(
                &host,
                1,
                ClientRequest::ChatPrepareResume { source: requested }
            ),
            Ok(ServerResponse::ChatResumePrepared { .. })
        ),
        "the same source prepares once its writer exits"
    );
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
    .unwrap_err();
    cli.kill().unwrap();
    cli.wait().unwrap();
    assert!(refusal.retryable, "{refusal:?}");
    assert!(
        refusal.message == format!("Cannot restart Codex: {}", running_codex_process(cli.id())),
        "{refusal:?}"
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
    let requested = reopened(&root);
    // A stopped entry allocated for this pane must remain available to the
    // execution proof until every writer check has passed.
    host.lock().unwrap().codex_listeners = CodexListenerRoots::at(private.path().into());
    let allocation = private
        .path()
        .join(format!("cagents-codex-{}", requested.id));
    std::fs::create_dir(&allocation).unwrap();
    std::fs::set_permissions(&allocation, std::fs::Permissions::from_mode(0o700)).unwrap();
    let allocated_socket = allocation.join("control.sock");
    drop(UnixListener::bind(&allocated_socket).unwrap());
    let before = std::fs::symlink_metadata(&allocated_socket).unwrap();
    // A writer-gate counterfactual must reach the explicit no-launch/inode
    // assertion, rather than time out connecting to its mistaken new server.
    let (cancel_fallback, fallback_cancelled) = mpsc::channel();
    let unexpected_server = {
        let (socket, home, record) = (
            allocated_socket.clone(),
            root.join("home/.codex"),
            root.join("codex-arguments"),
        );
        thread::spawn(move || {
            loop {
                match fallback_cancelled.recv_timeout(Duration::from_millis(10)) {
                    Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                }
                if record.exists() {
                    let (methods, _received) = mpsc::channel();
                    serve(UnixListener::bind(socket).unwrap(), home, methods);
                    break;
                }
            }
        })
    };
    let result = start(
        &host,
        1,
        requested.clone(),
        None,
        false,
        None,
        Default::default(),
    );
    let repeated = start(&host, 1, requested, None, false, None, Default::default());
    // Clean up the fixture before checking the result, including a mutant
    // that incorrectly tries to launch the installed Codex stand-in.
    if result.is_ok() {
        crate::host::finish_chat_shutdown(&host).unwrap();
    }
    let _ = cancel_fallback.send(());
    unexpected_server.join().unwrap();
    server.kill().unwrap();
    server.wait().unwrap();
    assert!(
        !root.join("codex-arguments").exists(),
        "no second private server was launched"
    );
    use std::os::unix::fs::MetadataExt;
    let after = std::fs::symlink_metadata(&allocated_socket)
        .expect("a live writer must prevent retirement of the pane's stopped endpoint");
    assert_eq!((before.dev(), before.ino()), (after.dev(), after.ino()));
    let refusal = result.unwrap_err();
    let repeated = repeated.unwrap_err();
    assert_eq!(refusal.code, ProtocolErrorCode::TerminalUnavailable);
    assert_eq!(
        repeated, refusal,
        "Retry while held must repeat the refusal"
    );
    assert!(
        refusal.message
            == format!(
                "Cannot restart Codex: {}",
                running_codex_process(writer.pid)
            ),
        "{refusal:?}"
    );
    assert!(
        refusal.retryable,
        "the same card may retry after its writer closes"
    );
    assert!(host.lock().unwrap().chat_spawning.is_empty());
}

#[test]
fn a_reopened_codex_thread_with_uninspectable_writer_evidence_refuses_retryably() {
    const NAME: &str = "host::chat::unserved_resume_tests::a_reopened_codex_thread_with_uninspectable_writer_evidence_refuses_retryably";
    if isolated(NAME).is_some() {
        return;
    }
    let (host, _, path) = fixture();
    let root = path.parent().unwrap().to_owned();
    install_codex(&root);
    let locks = root.join("home/.codex/thread-writer-locks");
    std::fs::create_dir_all(&locks).unwrap();
    let lock = locks.join(format!("{}.lock", thread_id()));
    // A loop reliably makes the real lock lookup fail, including under root.
    // It must not be interpreted as evidence that the thread has no writer.
    std::os::unix::fs::symlink(&lock, &lock).unwrap();
    host.lock().unwrap().register_client(1);
    let prepared = process(
        &host,
        1,
        ClientRequest::ChatPrepareResume {
            source: reopened(&root),
        },
    )
    .unwrap_err();
    assert_eq!(prepared.code, ProtocolErrorCode::TerminalUnavailable);
    assert!(prepared.retryable, "{prepared:?}");
    assert!(
        prepared
            .message
            .contains("Cannot inspect Codex thread writer lock"),
        "{prepared:?}"
    );
    let refusal = start(
        &host,
        1,
        reopened(&root),
        None,
        false,
        None,
        Default::default(),
    )
    .unwrap_err();
    assert_eq!(refusal.code, ProtocolErrorCode::TerminalUnavailable);
    assert!(
        refusal
            .message
            .contains("Cannot inspect Codex thread writer lock"),
        "{refusal:?}"
    );
    assert!(
        refusal.retryable,
        "inspection may be repaired before retrying"
    );
    assert!(
        !root.join("codex-arguments").exists(),
        "an inspection failure must never launch a server"
    );
    assert!(host.lock().unwrap().chat_spawning.is_empty());
}
