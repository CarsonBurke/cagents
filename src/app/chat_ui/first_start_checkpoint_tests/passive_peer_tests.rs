//! UI/private-wire proposal at 8da9055. No HOST, native window or provider.
//! The oracle is the production retirement consumer's emitted ChatClose.
use super::*;
use crate::{
    terminal_ipc::{
        ClientEnvelope, ClientRequest, FrameCodec, PROTOCOL_VERSION, ServerEnvelope, ServerMessage,
        ServerResponse,
    },
    workspace_model::{
        ModelEdit,
        views::{Change, Pane, Scope, View},
    },
};
use cosmic::iced::futures::StreamExt;
use std::{
    net::Shutdown,
    os::unix::{
        fs::PermissionsExt,
        net::{UnixListener, UnixStream},
    },
    sync::mpsc,
    time::Instant,
};

const CHILD: &str = "FW16_TEST_1757_PASSIVE_PEER";
const WAIT: Duration = Duration::from_secs(5);
const FAMILY_BUDGET: Duration = Duration::from_secs(40);
const FIRST: &str = "one accepted first prompt";

struct Child(std::process::Child);
impl Drop for Child {
    fn drop(&mut self) {
        if !matches!(self.0.try_wait(), Ok(Some(_))) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

// Inherit all authority/session environment unchanged. Only owned XDG roots
// and this fixture marker differ. Never use env_clear, set_var or a User role.
fn isolated(initialized: bool, late_identity: bool) {
    let name = std::thread::current().name().unwrap().to_owned();
    if std::env::var(CHILD).as_deref() == Ok(name.as_str()) {
        run(initialized, late_identity);
        return;
    }
    let directory = tempfile::Builder::new().prefix("p1757-").tempdir().unwrap();
    let stdout = directory.path().join("stdout");
    let stderr = directory.path().join("stderr");
    let mut command = std::process::Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", &name, "--nocapture", "--test-threads=1"])
        .env(CHILD, &name)
        .current_dir(directory.path())
        .stdin(std::process::Stdio::null())
        .stdout(std::fs::File::create(&stdout).unwrap())
        .stderr(std::fs::File::create(&stderr).unwrap());
    for (variable, leaf) in [
        ("XDG_CONFIG_HOME", "config"),
        ("XDG_DATA_HOME", "data"),
        ("XDG_STATE_HOME", "state"),
        ("XDG_CACHE_HOME", "cache"),
        ("XDG_RUNTIME_DIR", "run"),
    ] {
        let path = directory.path().join(leaf);
        std::fs::create_dir(&path).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        command.env(variable, path);
    }
    // Files prevent pipe backpressure. This deadline owns and reaps only the
    // exact self-spawned child, including its private wire/reader workers.
    let deadline = Instant::now() + FAMILY_BUDGET;
    let mut child = Child(command.spawn().unwrap());
    let status = loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "owned passive-peer test child timed out"
        );
        std::thread::sleep(Duration::from_millis(5));
    };
    let stdout = std::fs::read_to_string(stdout).unwrap();
    let stderr = std::fs::read_to_string(stderr).unwrap();
    assert!(status.success(), "{name}: {status}\n{stdout}\n{stderr}");
    assert!(stdout.contains("1 passed"));
}

// Socket ownership plus joined fixture workers bounds failure cleanup too.
struct WirePeer {
    stopping: Arc<std::sync::atomic::AtomicBool>,
    stop: Option<mpsc::Sender<()>>,
    sockets: Arc<Mutex<Vec<UnixStream>>>,
    worker: Option<std::thread::JoinHandle<()>>,
}
impl WirePeer {
    fn finish(mut self) {
        self.stopping
            .store(true, std::sync::atomic::Ordering::Release);
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        for socket in self.sockets.lock().unwrap().iter() {
            let _ = socket.shutdown(Shutdown::Both);
        }
        self.worker.take().unwrap().join().unwrap();
    }
}
impl Drop for WirePeer {
    fn drop(&mut self) {
        self.stopping
            .store(true, std::sync::atomic::Ordering::Release);
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        for socket in self.sockets.lock().unwrap().iter() {
            let _ = socket.shutdown(Shutdown::Both);
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn proof() -> HostSessionProof {
    HostSessionProof {
        host_session_id: crate::model::HostSessionId::new("passive-peer-host"),
        boot_id: crate::model::BootId::new("passive-peer-boot"),
        host_incarnation: crate::model::HostIncarnation::new("passive-peer-incarnation"),
    }
}

fn reply(stream: &mut UnixStream, id: Option<u64>, message: ServerMessage) {
    crate::terminal_ipc::write_frame(
        stream,
        &ServerEnvelope {
            version: PROTOCOL_VERSION,
            request_id: id,
            proof: proof(),
            message,
        },
    )
    .unwrap();
}
fn respond(stream: &mut UnixStream, id: u64, response: ServerResponse) {
    reply(stream, Some(id), ServerMessage::Response { response });
}
fn request(stream: &mut UnixStream) -> ClientEnvelope {
    let request: ClientEnvelope = crate::terminal_ipc::read_frame(stream).unwrap().unwrap();
    assert_eq!(request.version, PROTOCOL_VERSION);
    assert_eq!(request.proof, Some(proof()));
    request
}
fn handshake(stream: &mut UnixStream) -> Option<HostSessionProof> {
    stream.set_read_timeout(Some(WAIT)).unwrap();
    stream.set_write_timeout(Some(WAIT)).unwrap();
    let request: ClientEnvelope = crate::terminal_ipc::read_frame(stream).unwrap().unwrap();
    assert_eq!(request.version, PROTOCOL_VERSION);
    assert_eq!(request.proof, None);
    let ClientRequest::Connect {
        expected_host,
        resume,
    } = request.request
    else {
        panic!("the private endpoint requires a Connect handshake");
    };
    assert!(resume.is_empty());
    assert!(expected_host.is_none() || expected_host == Some(proof()));
    respond(
        stream,
        request.request_id,
        ServerResponse::Connected {
            proof: proof(),
            resumed: vec![],
        },
    );
    expected_host
}

#[derive(Default)]
struct Observed {
    connects: Vec<Option<HostSessionProof>>,
    lists: usize,
    chat_lists: usize,
    attaches: Vec<chat::Session>,
    closes: Vec<TerminalId>,
}

// This is a protocol endpoint, not HOST emulation. Its live inventory is
// populated only after the creator's actual Prompt Accepted + User snapshot.
fn cleanup_peer(
    listener: UnixListener,
    live: Arc<Mutex<Option<(chat::Session, Snapshot)>>>,
    observed: Arc<Mutex<Observed>>,
) -> WirePeer {
    listener.set_nonblocking(true).unwrap();
    let sockets = Arc::new(Mutex::new(Vec::new()));
    let owned = sockets.clone();
    let stopping = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stopping_worker = stopping.clone();
    let (stop, stopped) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        macro_rules! send {
            (@message $stream:expr, $id:expr, $message:expr) => {{
                let result = crate::terminal_ipc::write_frame(
                    $stream,
                    &ServerEnvelope {
                        version: PROTOCOL_VERSION,
                        request_id: $id,
                        proof: proof(),
                        message: $message,
                    },
                );
                match result {
                    Ok(()) => {}
                    Err(crate::terminal_ipc::FrameError::Io(error))
                        if stopping_worker.load(std::sync::atomic::Ordering::Acquire)
                            && matches!(
                                error.kind(),
                                std::io::ErrorKind::ConnectionReset
                                    | std::io::ErrorKind::BrokenPipe
                                    | std::io::ErrorKind::NotConnected
                            ) =>
                    {
                        return;
                    }
                    Err(error) => panic!("private passive response failed: {error}"),
                }
            }};
            ($stream:expr, $id:expr, $response:expr) => {
                send!(@message $stream, Some($id), ServerMessage::Response { response: $response })
            };
        }
        for _ in 0..8_000 {
            if stopped.try_recv().is_ok() {
                return;
            }
            let (mut stream, _) = match listener.accept() {
                Ok(connected) => connected,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    match stopped.recv_timeout(Duration::from_millis(5)) {
                        Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => return,
                        Err(mpsc::RecvTimeoutError::Timeout) => continue,
                    }
                }
                Err(error) => panic!("private accept failed: {error}"),
            };
            owned.lock().unwrap().push(stream.try_clone().unwrap());
            let expected = handshake(&mut stream);
            observed.lock().unwrap().connects.push(expected.clone());
            for _ in 0..8 {
                // Protocol verification uses a handshake-only connection.
                // EOF there is normal; attachment requests below still pass
                // through the production client and exact proof checks.
                let request: ClientEnvelope = match crate::terminal_ipc::read_frame(&mut stream) {
                    Ok(Some(request)) => request,
                    Ok(None) => break,
                    Err(crate::terminal_ipc::FrameError::Io(error))
                        if stopping_worker.load(std::sync::atomic::Ordering::Acquire)
                            && matches!(
                                error.kind(),
                                std::io::ErrorKind::ConnectionReset
                                    | std::io::ErrorKind::BrokenPipe
                                    | std::io::ErrorKind::NotConnected
                            ) =>
                    {
                        return;
                    }
                    Err(error) => panic!("private passive request failed: {error}"),
                };
                assert_eq!(request.version, PROTOCOL_VERSION);
                assert_eq!(request.proof, Some(proof()));
                let response = match request.request {
                    ClientRequest::List => {
                        observed.lock().unwrap().lists += 1;
                        ServerResponse::Listed { terminals: vec![] }
                    }
                    ClientRequest::ChatList => {
                        observed.lock().unwrap().chat_lists += 1;
                        let session = live
                            .lock()
                            .unwrap()
                            .as_ref()
                            .expect("creator already accepted B")
                            .0
                            .clone();
                        ServerResponse::ChatListed {
                            sessions: vec![session],
                        }
                    }
                    ClientRequest::ChatAttach { session } => {
                        let (canonical, snapshot) = live.lock().unwrap().clone().unwrap();
                        assert_eq!(
                            session, canonical,
                            "cleanup attaches the live inventory's exact descriptor"
                        );
                        if expected.is_some() {
                            assert_eq!(expected, canonical.proof);
                        }
                        observed.lock().unwrap().attaches.push(session.clone());
                        ServerResponse::ChatAttached { session, snapshot }
                    }
                    ClientRequest::ChatVisibility { chat_id, visible } => {
                        let (session, snapshot) = live.lock().unwrap().clone().unwrap();
                        assert_eq!(chat_id, session.id);
                        send!(&mut stream, request.request_id, ServerResponse::Accepted);
                        if visible {
                            // Match HOST: becoming visible announces a
                            // presentation, which the actual client reads.
                            send!(@message &mut stream, None, ServerMessage::ChatChanged { chat_id });
                        } else if snapshot.generation > 1 {
                            send!(@message &mut stream, None, ServerMessage::ChatObserved {
                                // No acknowledgement has advanced this
                                // subscriber's observation epoch from zero.
                                chat_id, epoch: 0,
                            });
                        }
                        continue;
                    }
                    ClientRequest::ChatRead { chat_id, .. } => {
                        let (session, snapshot) = live.lock().unwrap().clone().unwrap();
                        assert_eq!(chat_id, session.id);
                        ServerResponse::ChatSnapshot {
                            chat_id,
                            snapshot: Some(snapshot),
                        }
                    }
                    ClientRequest::ChatDetach { chat_id } => {
                        assert_eq!(chat_id, live.lock().unwrap().as_ref().unwrap().0.id);
                        send!(&mut stream, request.request_id, ServerResponse::Detached);
                        break;
                    }
                    ClientRequest::ChatClose { chat_id } => {
                        let (session, snapshot) = live.lock().unwrap().clone().unwrap();
                        assert_eq!(chat_id, session.id);
                        assert_eq!(observed.lock().unwrap().attaches.last(), Some(&session));
                        observed.lock().unwrap().closes.push(chat_id);
                        send!(
                            &mut stream,
                            request.request_id,
                            ServerResponse::ChatClosed { chat_id, snapshot }
                        );
                        break;
                    }
                    other => panic!("unexpected cleanup request: {other:?}"),
                };
                send!(&mut stream, request.request_id, response);
            }
        }
        panic!("private accept loop exhausted its 40-second/count bound");
    });
    WirePeer {
        stopping,
        stop: Some(stop),
        sockets,
        worker: Some(worker),
    }
}

fn creator_peer(
    actor: TerminalId,
    live: Arc<Mutex<Option<(chat::Session, Snapshot)>>>,
    late_identity: bool,
) -> (UnixStream, WirePeer) {
    let (client, mut server) = UnixStream::pair().unwrap();
    let sockets = Arc::new(Mutex::new(vec![server.try_clone().unwrap()]));
    let stopping = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stopping_worker = stopping.clone();
    let worker = std::thread::spawn(move || {
        assert_eq!(handshake(&mut server), None);
        server
            .set_read_timeout(Some(Duration::from_secs(15)))
            .unwrap();
        // Only intentional shutdown of this owned socket can make a concurrent
        // write's expected I/O error benign. Protocol/assertion failures remain
        // fatal, and normal running writes must succeed.
        macro_rules! send {
            ($id:expr, $message:expr) => {{
                let result = crate::terminal_ipc::write_frame(
                    &mut server,
                    &ServerEnvelope {
                        version: PROTOCOL_VERSION,
                        request_id: $id,
                        proof: proof(),
                        message: $message,
                    },
                );
                match result {
                    Ok(()) => {}
                    Err(crate::terminal_ipc::FrameError::Io(error))
                        if stopping_worker.load(std::sync::atomic::Ordering::Acquire)
                            && matches!(
                                error.kind(),
                                std::io::ErrorKind::BrokenPipe
                                    | std::io::ErrorKind::ConnectionReset
                                    | std::io::ErrorKind::NotConnected
                            ) =>
                    {
                        return;
                    }
                    Err(error) => panic!("creator response failed: {error}"),
                }
            }};
        }
        let start = request(&mut server);
        let ClientRequest::ChatStart { mut session, .. } = start.request else {
            panic!("real ChatStart required");
        };
        assert_eq!(session.id, actor);
        assert!(session.is_unstarted());
        session.proof = Some(proof());
        session.launch.session_id =
            (!late_identity).then(|| "17570000-0000-4000-8000-000000000002".into());
        if late_identity {
            session.native_continuity = chat::NativeContinuity::RetainedClaudeActor { protocol: 2 };
        }
        let initial = Snapshot {
            generation: 1,
            ready: true,
            session_id: session.launch.session_id.clone(),
            native_continuity: session.native_continuity.clone(),
            selected_model: session
                .launch
                .draft_model
                .as_ref()
                .map(|choice| choice.model.clone()),
            reasoning: session
                .launch
                .draft_model
                .as_ref()
                .and_then(|choice| choice.reasoning.clone()),
            ..Snapshot::default()
        };
        send!(
            Some(start.request_id),
            ServerMessage::Response {
                response: ServerResponse::ChatAttached {
                    session: session.clone(),
                    snapshot: initial.clone()
                },
            }
        );
        let mut prompt = false;
        let mut codec = FrameCodec::new(crate::terminal_ipc::MAX_FRAME_BYTES);
        for _ in 0..64 {
            let request: ClientEnvelope = match codec.read(&mut server) {
                Ok(Some(request)) => request,
                Ok(None) => return,
                Err(crate::terminal_ipc::FrameError::Io(error))
                    if stopping_worker.load(std::sync::atomic::Ordering::Acquire)
                        && matches!(
                            error.kind(),
                            std::io::ErrorKind::ConnectionReset
                                | std::io::ErrorKind::BrokenPipe
                                | std::io::ErrorKind::NotConnected
                        ) =>
                {
                    return;
                }
                other => panic!("creator private wire failed: {other:?}"),
            };
            assert_eq!(request.version, PROTOCOL_VERSION);
            assert_eq!(request.proof, Some(proof()));
            match request.request {
                ClientRequest::ChatVisibility { chat_id, .. } => {
                    assert_eq!(chat_id, actor);
                    send!(
                        Some(request.request_id),
                        ServerMessage::Response {
                            response: ServerResponse::Accepted
                        }
                    );
                }
                ClientRequest::ChatCommand {
                    chat_id,
                    command:
                        Command::Prompt {
                            text, attachments, ..
                        },
                } => {
                    assert_eq!(chat_id, actor);
                    assert_eq!(text, FIRST);
                    assert!(attachments.is_empty());
                    assert!(!prompt, "one original first prompt");
                    prompt = true;
                    session.launch.session_id = Some("17570000-0000-4000-8000-000000000002".into());
                    // Normal HOST serial processing: acknowledge this command
                    // before any later read, then announce the new transcript.
                    send!(
                        Some(request.request_id),
                        ServerMessage::Response {
                            response: ServerResponse::Accepted
                        }
                    );
                    send!(None, ServerMessage::ChatChanged { chat_id: actor });
                }
                ClientRequest::ChatRead { chat_id, .. } => {
                    assert_eq!(chat_id, actor);
                    let snapshot = if prompt {
                        Snapshot {
                            generation: 2,
                            busy: true,
                            session_id: session.launch.session_id.clone(),
                            items: vec![Arc::new(chat::Item {
                                id: "passive-peer-first-user".into(),
                                role: chat::Role::User,
                                content: chat::ItemContent::Text {
                                    text: FIRST.into(),
                                    thinking: false,
                                    phase: Default::default(),
                                },
                                started_at: None,
                                at: None,
                            })],
                            ..initial.clone()
                        }
                    } else {
                        initial.clone()
                    };
                    // HOST inventory already reflects accepted work before a UI
                    // can receive its later User snapshot. Publish this fixture
                    // inventory first, so observing User proves it is available.
                    if prompt {
                        *live.lock().unwrap() = Some((session.clone(), snapshot.clone()));
                    }
                    send!(
                        Some(request.request_id),
                        ServerMessage::Response {
                            response: ServerResponse::ChatSnapshot {
                                chat_id,
                                snapshot: Some(snapshot)
                            },
                        }
                    );
                }
                ClientRequest::ChatDetach { chat_id } => {
                    assert_eq!(chat_id, actor);
                    send!(
                        Some(request.request_id),
                        ServerMessage::Response {
                            response: ServerResponse::Detached
                        }
                    );
                    return;
                }
                other => panic!("unexpected creator request: {other:?}"),
            }
        }
        panic!("creator private wire exceeded its 64-frame bound");
    });
    (
        client,
        WirePeer {
            stopping,
            stop: None,
            sockets,
            worker: Some(worker),
        },
    )
}

// Join the small executor; task count + deadline bound drains independently of
// the outer owned-child deadline. None never starts a daemon or provider.
fn outputs(task: Task<Message>, stop_at_command: bool) -> Vec<Message> {
    std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let Some(mut stream) = cosmic::iced::runtime::task::into_stream(task) else {
                    return vec![];
                };
                let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
                let mut messages = Vec::new();
                for _ in 0..64 {
                    let Some(action) = tokio::time::timeout_at(deadline, stream.next())
                        .await
                        .expect("bounded UI task drain")
                    else {
                        return messages;
                    };
                    if let cosmic::iced::runtime::Action::Output(cosmic::Action::App(message)) =
                        action
                    {
                        let done = stop_at_command
                            && matches!(&message, Message::ChatUi(Action::CommandFinished(..)));
                        messages.push(message);
                        if done {
                            return messages;
                        }
                    }
                }
                panic!("UI task drain exceeded 64 actions");
            })
    })
    .join()
    .unwrap()
}

fn next_wake(app: &Cagents) -> (Target, Wake) {
    let receiver = app.chat_ui.receiver.clone();
    std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async move {
                tokio::time::timeout(WAIT, receiver.recv())
                    .await
                    .unwrap()
                    .unwrap()
            })
    })
    .join()
    .unwrap()
}

fn run(initialized: bool, late_identity: bool) {
    chat::attachment_assets::with_test_image(|first| {
        let second = chat::attachment_assets::test_image("passive peer second image.png");
        run_with_peer_draft(initialized, late_identity, vec![first, second]);
    });
}

fn reopen_peer_writer(
    peer: &mut Cagents,
    database: &std::path::Path,
    card: CardId,
    expected: &chat::Session,
    draft: &str,
    images: &[Attachment],
) {
    let writer = peer.session.as_ref().unwrap();
    writer.flush(WAIT).unwrap();
    assert_eq!(writer.last_error(), None);
    assert_eq!(stored(peer, card), *expected);
    let mut writer = peer.session.take().unwrap();
    writer.shutdown();
    drop(writer);
    let (writer, restored) = crate::session::SessionRuntime::open(
        database,
        crate::model::BootId::new("passive-peer-save"),
    )
    .unwrap();
    assert!(writer.restore_warnings().is_empty());
    assert_eq!(writer.last_error(), None);
    let restored = restored.expect("the passive window has a saved workspace");
    assert_eq!(restored.id, peer.durable.id);
    assert_eq!(restored.cards.len(), 1);
    assert_eq!(restored.cards[0].id, card);
    assert!(restored.cards[0].existence.is_open());
    assert_eq!(restored.cards[0].kind, CardKind::Chat(expected.clone()));
    assert_eq!(
        restored.chat_drafts.get(&card).map(String::as_str),
        Some(draft)
    );
    assert_eq!(
        restored
            .chat_draft_attachments
            .get(&card)
            .map(Vec::as_slice),
        Some(images)
    );
    assert!(restored.pending_chat_admissions.is_empty());
    peer.session = Some(writer);
}

fn run_with_peer_draft(initialized: bool, late_identity: bool, images: Vec<Attachment>) {
    let runtime_root = PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR").unwrap());
    let socket = crate::terminal::ipc::daemon_socket_path().unwrap();
    assert!(
        socket.starts_with(&runtime_root),
        "refuse any endpoint outside this owned child's runtime namespace"
    );
    assert!(crate::device::selected().is_none());
    assert_eq!(
        socket,
        runtime_root
            .join(crate::instance::RUNTIME_SUBDIRECTORY)
            .join("cagentsd.sock")
    );
    std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
    let listener = UnixListener::bind(&socket).unwrap();
    let live = Arc::new(Mutex::new(None));
    let observed = Arc::new(Mutex::new(Observed::default()));
    let cleanup = cleanup_peer(listener, live.clone(), observed.clone());

    let directory = tempfile::tempdir().unwrap();
    // Synthetic launch only: no process environment or account authority is
    // rewritten. Skills may inspect only these owned paths and stop at .git.
    std::fs::create_dir(directory.path().join(".git")).unwrap();
    let home = directory.path().join("home");
    std::fs::create_dir(&home).unwrap();
    let source = chat::Session {
        id: TerminalId::new(),
        proof: None,
        terminal: None,
        native_continuity: Default::default(),
        owned_codex_listener: None,
        launch: chat::Launch {
            draft_model: late_identity.then(|| chat::DraftModel {
                model: "sonnet".into(),
                reasoning: Some("high".into()),
            }),
            provider: Provider::Claude,
            session_id: None,
            recipe: LaunchRecipe {
                program: "claude".into(),
                arguments: vec![],
                working_directory: directory.path().to_owned(),
                unset_environment: Default::default(),
                environment: [
                    ("HOME".into(), home.to_string_lossy().into_owned()),
                    (
                        "CLAUDE_CONFIG_DIR".into(),
                        home.join(".claude").to_string_lossy().into_owned(),
                    ),
                ]
                .into(),
            },
        },
    };
    let mut creator = crate::app::tests::context_inspector_test_app(false);
    let (writer, _) = crate::session::SessionRuntime::open(
        directory.path().join("creator.sqlite3"),
        crate::model::BootId::new("passive-peer-save"),
    )
    .unwrap();
    creator.session = Some(writer);
    creator.workspace.resize_viewport(1280.0);
    drop(creator.insert_chat_card(source.clone(), None, None));
    let card = creator.workspace.focused().unwrap();
    let target = runtime(&creator, card).target;
    let channel = crate::model::ChannelId::new();
    Arc::make_mut(&mut creator.durable)
        .channels
        .push(crate::model::ProjectChannel {
            id: channel,
            project_id: ProjectId::new(),
            title: "private first start".into(),
            title_source: Default::default(),
            ordinal: 1,
            shared: true,
            cards: vec![card],
            focused: Some(card),
        });
    Arc::make_mut(&mut creator.durable).active_channel = Some(channel);
    let scope = Scope::Channel(channel);
    let pane_a = Pane::from_card(creator.card_state(card).unwrap(), None).unwrap();
    let mut view = View::empty(channel);
    view.revision = 1;
    view.panes.push(pane_a.clone());
    creator.workspace_model.identity = uuid::Uuid::new_v4();
    if late_identity {
        let repository = directory.path().join(".git");
        creator
            .workspace_model
            .projects
            .push(crate::workspace_model::ModelProject {
                repository: repository.clone(),
                name: "Private enrichment race".into(),
                kind: Default::default(),
                ordinal: 1,
            });
        creator
            .workspace_model
            .channels
            .push(crate::workspace_model::ModelChannel {
                id: channel,
                repository,
                title: "private first start".into(),
                title_source: crate::model::ChannelTitleSource::Manual,
                ordinal: 1,
            });
    }
    creator.workspace_model.views = vec![view.clone()];
    let (publish, published) = async_channel::unbounded();
    creator.workspace_model_tx = publish;
    creator.publish_view_changes(); // actual SeedView A establishes its baseline
    assert!(matches!(
        published.try_recv().unwrap(),
        ModelEdit::SeedView { .. }
    ));
    creator.acknowledge_view_edit(scope.clone(), None);
    drop(creator.reconcile_shared_views());
    assert!(published.try_recv().is_err());

    // A passive peer already shows A in the same canonical pane. Its scope is
    // offscreen, avoiding window/lifecycle tasks while retaining real projection.
    let mut peer = crate::app::tests::context_inspector_test_app(false);
    let peer_database = directory.path().join("peer.sqlite3");
    let (writer, _) = crate::session::SessionRuntime::open(
        &peer_database,
        crate::model::BootId::new("passive-peer-save"),
    )
    .unwrap();
    peer.session = Some(writer);
    peer.durable = creator.durable.clone();
    Arc::make_mut(&mut peer.durable).active_channel = None;
    peer.workspace_model = creator.workspace_model.clone();
    drop(peer.open_chat(card, source.clone(), chat::Intent::Start));
    peer.publish_view_changes();
    peer.acknowledge_view_edit(scope.clone(), None);
    drop(peer.reconcile_shared_views());
    assert!(peer.chat_ui.retiring.is_empty());
    let peer_draft = format!(
        "Unsent peer text stays here  \n{} {}\n  Unicode λ and trailing spaces  ",
        images[0].reference.as_ref().unwrap().token(),
        images[1].reference.as_ref().unwrap().token(),
    );
    if let Some(CardRuntime::Chat(runtime)) = peer.cards.get_mut(&card) {
        runtime.replace_draft(text_editor::Content::with_text(&peer_draft));
        runtime.replace_attachments(images.clone());
    }
    peer.persist();
    // A must be on disk before B appears. Otherwise an initial coalesced
    // snapshot would hide the ordinary identity-save refusal on this peer.
    reopen_peer_writer(
        &mut peer,
        &peer_database,
        card,
        &source,
        &peer_draft,
        &images,
    );

    drop(creator.handle_chat_ui(Action::Composer(
        target,
        text_editor::Action::Edit(text_editor::Edit::Paste(Arc::new(FIRST.into()))),
    )));
    creator.persist();
    creator.session.as_ref().unwrap().flush(WAIT).unwrap();
    let save = creator.handle_chat_ui(Action::Submit(target, PromptMode::Send));
    let (saved_target, attempt, result) = checkpoint(save);
    assert!(result.is_ok(), "real A-to-B writer checkpoint");
    // Drop only discovery/start work. The helper below drives the same actual
    // native-client handshake on a private socket, never ensure_daemon_ready.
    drop(creator.handle_chat_ui(Action::DraftStartCheckpointed(
        saved_target,
        attempt,
        result,
    )));
    let candidate = creator.chat_ui.attempt(card).unwrap().session.clone();
    let start_target = creator.chat_ui.attempt(card).unwrap().target;
    assert_ne!(candidate.id, source.id);
    assert!(candidate.is_unstarted());
    let (operation, change) = std::iter::from_fn(|| published.try_recv().ok())
        .find_map(|edit| match edit {
            ModelEdit::ChangeView {
                scope: edit_scope,
                operation,
                change: change @ Change::Update { .. },
                ..
            } if edit_scope == scope
                && matches!(&change, Change::Update { before, pane }
                if before.id == card && before.kind == CardKind::Chat(source.clone())
                    && pane.kind == CardKind::Chat(candidate.clone())) =>
            {
                Some((operation, change))
            }
            _ => None,
        })
        .expect("checkpoint reducer must really publish unstarted B replacing A");
    assert!(view.apply(&change).unwrap());
    view.revision += 1;
    creator.acknowledge_view_edit(scope.clone(), Some(operation));
    creator.workspace_model.views = vec![view.clone()];
    peer.workspace_model.views = vec![view.clone()];
    // In the regression, capture the actual peer projection's task now and do
    // not poll it until B has accepted the creator's original prompt.
    let mut delayed = (!initialized).then(|| peer.reconcile_shared_views());
    assert_eq!(
        peer.card_state(card).unwrap().kind,
        if initialized {
            CardKind::Chat(source.clone())
        } else {
            CardKind::Chat(candidate.clone())
        }
    );

    let (client, creator_wire) = creator_peer(candidate.id, live.clone(), late_identity);
    let wake_sender = creator.chat_ui.sender.clone();
    let handle = Arc::new(
        ClientHandle::fixture_start_on(client, candidate.clone(), move |wake| {
            let _ = wake_sender.try_send((start_target, wake));
        })
        .unwrap(),
    );
    let send = creator.handle_chat_ui(Action::Ready(
        start_target,
        Ready(Arc::new(Mutex::new(Some(Ok(handle.clone()))))),
    ));
    if let CardRuntime::Chat(runtime) = creator.cards.get_mut(&card).unwrap() {
        runtime.show(true);
    }
    let late_peer = late_identity.then(|| {
        // The retained actor is authenticated before Claude names its native
        // conversation. Attach the peer now, before polling the original Send.
        drop(delayed.take());
        let canonical = handle.session();
        assert!(canonical.launch.session_id.is_none());
        assert_eq!(
            canonical.native_continuity,
            chat::NativeContinuity::RetainedClaudeActor { protocol: 2 }
        );
        *live.lock().unwrap() = Some((canonical.clone(), (*handle.snapshot()).clone()));
        let edit = std::iter::from_fn(|| published.try_recv().ok())
            .find(|edit| {
                matches!(edit, ModelEdit::ChangeView {
                change: Change::Update { before, pane }, ..
            } if before.kind == CardKind::Chat(candidate.clone())
                && pane.kind == CardKind::Chat(canonical.clone()))
            })
            .expect("Ready publishes the actual authenticated pre-ID actor");
        let ModelEdit::ChangeView {
            operation,
            expected_revision,
            change,
            ..
        } = edit
        else {
            unreachable!()
        };
        assert_eq!(expected_revision, view.revision);
        assert!(view.apply(&change).unwrap());
        view.revision += 1;
        creator.acknowledge_view_edit(scope.clone(), Some(operation));
        creator.workspace_model.views = vec![view.clone()];
        peer.workspace_model.views = vec![view.clone()];
        let (sender, edits) = async_channel::unbounded();
        peer.workspace_model_tx = sender;
        let attach = peer.reconcile_shared_views();
        let peer_target = peer.chat_ui.attempt(card).unwrap().target;
        for message in outputs(attach, false) {
            assert!(matches!(&message, Message::ChatUi(Action::Ready(..))));
            drop(peer.update(message));
        }
        assert_eq!(runtime(&peer, card).session, canonical);
        assert_eq!(runtime(&peer, card).composer.text(), peer_draft);
        assert_eq!(runtime(&peer, card).attachments, images);
        assert!(runtime(&peer, card).queued.is_none());
        peer.session.as_ref().unwrap().flush(WAIT).unwrap();
        assert_eq!(peer.session.as_ref().unwrap().last_error(), None);
        assert_eq!(stored(&peer, card), canonical);
        assert!(
            edits.try_recv().is_err(),
            "passive Ready adopts the canonical source"
        );
        (edits, peer_target, canonical)
    });
    let completed = outputs(send, true);
    assert_eq!(
        completed
            .iter()
            .filter(|message| matches!(message,
        Message::ChatUi(Action::CommandFinished(target, _, Ok(()))) if *target == start_target))
            .count(),
        1
    );
    for message in completed {
        drop(creator.update(message));
    }
    // Drain actual wake FIFO, rather than fabricating Changed or accepted state.
    let wakes = creator.chat_ui.receiver.clone();
    let wakes = std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async move {
                tokio::time::timeout(WAIT, wakes.recv())
                    .await
                    .unwrap()
                    .unwrap()
            })
    })
    .join()
    .unwrap();
    drop(creator.handle_chat_ui(Action::Wake(wakes.0, wakes.1)));
    // Visibility may publish an earlier Changed. Consume bounded real FIFO until
    // the reader has installed the User item and the reducer displays it.
    for _ in 0..64 {
        if handle.has_observed_user_item()
            && runtime(&creator, card)
                .snapshot
                .items
                .iter()
                .any(|item| item.role == chat::Role::User)
        {
            break;
        }
        let receiver = creator.chat_ui.receiver.clone();
        let wake = std::thread::spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(async move {
                    tokio::time::timeout(WAIT, receiver.recv())
                        .await
                        .unwrap()
                        .unwrap()
                })
        })
        .join()
        .unwrap();
        assert_eq!(wake.0, start_target);
        drop(creator.handle_chat_ui(Action::Wake(wake.0, wake.1)));
    }
    assert!(handle.has_observed_user_item());
    assert!(runtime(&creator, card).snapshot.busy);
    assert_eq!(
        runtime(&creator, card)
            .snapshot
            .items
            .iter()
            .filter(|item| item.role == chat::Role::User)
            .count(),
        1
    );
    let (accepted, _) = live
        .lock()
        .unwrap()
        .clone()
        .expect("real accepted publication is in inventory");
    assert_eq!(runtime(&creator, card).session.id, accepted.id);
    assert_eq!(runtime(&creator, card).session.proof, accepted.proof);
    assert_eq!(
        runtime(&creator, card).session.launch.session_id,
        accepted.launch.session_id
    );

    if let Some((peer_edits, peer_target, pre_id)) = late_peer {
        // Both reducer publications below come from actual reader metadata.
        // Hold the peer's watcher so its revision still names pre-ID B while
        // the creator's independently queued enrichment wins at the registry.
        let source_edit = |edits: &async_channel::Receiver<ModelEdit>| {
            let mut sources = std::iter::from_fn(|| edits.try_recv().ok()).filter(|edit| {
                matches!(edit, ModelEdit::ChangeView {
                    change: Change::Update { before, pane }, ..
                } if pane.id == card && before.kind != pane.kind)
            });
            let edit = sources
                .next()
                .expect("reader enrichment publishes a source edit");
            assert!(
                sources.next().is_none(),
                "one provider-ID enrichment per reader"
            );
            edit
        };
        let creator_edit = source_edit(&published);
        let ModelEdit::ChangeView {
            expected_revision: creator_revision,
            change: creator_change @ Change::Update { .. },
            ..
        } = &creator_edit
        else {
            unreachable!()
        };
        let Change::Update { before, pane } = creator_change else {
            unreachable!()
        };
        assert_eq!(before.kind, CardKind::Chat(pre_id.clone()));
        assert_eq!(pane.kind, CardKind::Chat(accepted.clone()));
        let mut only_provider_id = pre_id.clone();
        only_provider_id.launch.session_id = accepted.launch.session_id.clone();
        assert_eq!(
            only_provider_id, accepted,
            "no actor, recipe or capability changed"
        );
        assert_eq!(*creator_revision, view.revision);

        if let Some(CardRuntime::Chat(runtime)) = peer.cards.get_mut(&card) {
            runtime.show(true);
        }
        let mut wake_order = Vec::new();
        for _ in 0..64 {
            if runtime(&peer, card).session.launch.session_id == accepted.launch.session_id {
                break;
            }
            let (target, wake) = next_wake(&peer);
            assert_eq!(target, peer_target);
            wake_order.push(format!("{wake:?}"));
            drop(peer.handle_chat_ui(Action::Wake(target, wake)));
        }
        assert_eq!(runtime(&peer, card).session, accepted);
        // Hidden observation uses the same real connection after the visible
        // read. Exercise its actual Observed callback too; no synthetic wake.
        if let Some(CardRuntime::Chat(runtime)) = peer.cards.get_mut(&card) {
            runtime.show(false);
        }
        let mut observed_wake = false;
        for _ in 0..64 {
            let (target, wake) = next_wake(&peer);
            assert_eq!(target, peer_target);
            let observed = matches!(wake, Wake::Observed);
            wake_order.push(format!("{wake:?}"));
            drop(peer.handle_chat_ui(Action::Wake(target, wake)));
            if observed {
                observed_wake = true;
                break;
            }
        }
        assert!(observed_wake);
        let peer_edit = source_edit(&peer_edits);
        let ModelEdit::ChangeView {
            scope: peer_scope,
            operation,
            expected_revision: peer_revision,
            change: peer_change @ Change::Update { .. },
        } = &peer_edit
        else {
            unreachable!()
        };
        let Change::Update { before, pane } = peer_change else {
            unreachable!()
        };
        assert_eq!(peer_scope, &scope);
        assert_eq!(*peer_revision, *creator_revision);
        assert_eq!(before.kind, CardKind::Chat(pre_id.clone()));
        assert_eq!(pane.kind, CardKind::Chat(accepted.clone()));
        assert_eq!(before.pending_terminal_start, pane.pending_terminal_start);
        assert_eq!(before.scope, pane.scope);
        let source_conflicts = crate::app::workspace_model_sync::source_update_conflicts_for_test;
        assert!(!source_conflicts(
            &peer.workspace_model,
            &scope,
            peer_change
        ));

        // Accept the first window's actual source update at its original
        // revision. The second request then hits the same revision refusal
        // and exact source predicate used by the production publisher.
        assert!(view.apply(creator_change).unwrap());
        view.revision += 1;
        let mut canonical_model = peer.workspace_model.clone();
        canonical_model.sequence += 1;
        canonical_model.views = vec![view.clone()];
        assert_ne!(*peer_revision, view.revision);
        assert!(source_conflicts(&canonical_model, &scope, peer_change));
        assert_eq!(view.panes[0].kind, pane.kind);
        assert_eq!(
            view.panes[0].pending_terminal_start,
            pane.pending_terminal_start
        );
        assert_eq!(view.panes[0].scope, pane.scope);
        eprintln!(
            "LATE_PROVIDER_ID_RACE initialized={initialized} wakes={wake_order:?} creator={creator_edit:?} peer={peer_edit:?} canonical={:?}",
            view.panes[0],
        );
        drop(peer.apply_workspace_model_update(
            crate::app::workspace_model_sync::ModelUpdate::ViewConflicted {
                scope: scope.clone(),
                operation: *operation,
                change: peer_change.clone(),
                model: canonical_model,
                error: "daemon rejected the request (ViewConflict): The workspace view changed; rebase this pane mutation on the current revision".into(),
            },
        ));
        assert_eq!(runtime(&peer, card).session, accepted);
        assert_eq!(runtime(&peer, card).composer.text(), peer_draft);
        assert_eq!(runtime(&peer, card).attachments, images);
        assert!(runtime(&peer, card).queued.is_none() && runtime(&peer, card).pending.is_none());
        assert!(peer.chat_ui.retiring.is_empty() && peer.chat_ui.abandoned.is_empty());
        let false_refusal = peer
            .notices
            .recent()
            .any(|entry| entry.notice.message.contains("session update was refused"));
        peer.persist();
        reopen_peer_writer(
            &mut peer,
            &peer_database,
            card,
            &accepted,
            &peer_draft,
            &images,
        );
        let after_settlement =
            std::iter::from_fn(|| peer_edits.try_recv().ok()).collect::<Vec<_>>();
        assert!(
            after_settlement.iter().all(|edit| match edit {
                ModelEdit::ChangeView {
                    change: Change::Update { before, pane },
                    ..
                } => {
                    before.kind == pane.kind
                        && before.pending_terminal_start == pane.pending_terminal_start
                        && before.scope == pane.scope
                }
                ModelEdit::ChangeView { .. } | ModelEdit::SeedView { .. } => false,
                _ => true,
            }),
            "settling the achieved provider-ID update must not replay source intent: {after_settlement:?}",
        );
        cleanup.finish();
        creator_wire.finish();
        assert!(observed.lock().unwrap().closes.is_empty());
        assert!(
            !false_refusal,
            "the actual duplicate provider-ID publication is already canonical; it must settle without a source-refusal notice",
        );
        return;
    }

    if let Some(delayed) = delayed {
        for message in outputs(delayed, false) {
            assert!(
                matches!(
                    &message,
                    Message::ChatUi(Action::Retired(_, Ok(())) | Action::DraftSkills(..))
                ),
                "unexpected peer task output: {message:?}"
            );
            // Apply discovery replies, dropping follow-on work before another
            // connection could run. This first projection remains a draft.
            drop(peer.update(message));
        }
        let writer = peer.session.as_ref().unwrap();
        writer.flush(WAIT).unwrap();
        assert_eq!(writer.last_error(), None);
        assert_eq!(stored(&peer, card), candidate);
        assert_eq!(runtime(&peer, card).composer.text(), peer_draft);
        assert_eq!(runtime(&peer, card).attachments, images);
        assert!(!peer.chat_start_pending(card));
    }
    {
        // Both watcher orders must attach actual initialized B: direct A -> B
        // and A -> unstarted B -> initialized B with the same session id.
        // Keep whichever unstarted source reached disk until exact Ready.
        let saved_source = if initialized { &source } else { &candidate };
        let initialized_pane = std::iter::from_fn(|| published.try_recv().ok())
            .filter_map(|edit| match edit {
                ModelEdit::ChangeView {
                    change: Change::Update { pane, .. },
                    ..
                } if matches!(&pane.kind, CardKind::Chat(session) if session == &accepted) => {
                    Some(pane)
                }
                _ => None,
            })
            .last()
            .expect("creator must really publish initialized B");
        view.panes = vec![initialized_pane];
        view.revision += 1;
        peer.workspace_model.views = vec![view];
        let attach = peer.reconcile_shared_views();
        assert!(
            peer.chat_ui.retiring.is_empty(),
            "initialized peer Restore must not schedule retirement"
        );
        assert!(peer.chat_start_matches(card, &accepted));
        assert!(peer.chat_ui.explicit_starts.is_empty());
        assert!(peer.chat_ui.draft_checkpoints.is_empty());
        let writer = peer.session.as_ref().unwrap();
        writer.flush(WAIT).unwrap();
        assert_eq!(writer.last_error(), None);
        assert_eq!(&stored(&peer, card), saved_source);
        // A repeated watcher and ordinary autosave must not commit B before
        // the held attachment completes, or create another passive attempt.
        let attachment_target = peer.chat_ui.attempt(card).unwrap().target;
        drop(peer.reconcile_shared_views());
        peer.persist();
        peer.session.as_ref().unwrap().flush(WAIT).unwrap();
        assert_eq!(peer.session.as_ref().unwrap().last_error(), None);
        assert_eq!(&stored(&peer, card), saved_source);
        let held = peer
            .session
            .as_ref()
            .unwrap()
            .operation_store()
            .load_workspace(peer.durable.id, None)
            .unwrap()
            .unwrap();
        assert!(held.pending_chat_admissions.is_empty());
        assert_eq!(
            peer.persistent_workspace().pending_chat_admissions,
            held.pending_chat_admissions
        );
        if initialized {
            // A cold window reconstructs from ordinary saved A, not the old
            // map or its weak writer identity. It must prove canonical B anew.
            let cold_database = directory.path().join("peer-cold.sqlite3");
            crate::persistence::StateStore::copy_snapshot(&peer_database, &cold_database).unwrap();
            let (writer, restored) = crate::session::SessionRuntime::open(
                &cold_database,
                crate::model::BootId::new("passive-peer-save"),
            )
            .unwrap();
            assert!(writer.restore_warnings().is_empty());
            let restored = restored.unwrap();
            assert!(restored.pending_chat_admissions.is_empty());
            assert_eq!(restored.cards[0].kind, CardKind::Chat(source.clone()));
            let mut cold = crate::app::tests::context_inspector_test_app(false);
            cold.chat_ui = crate::app::chat_ui::State::with_draft_attachments(
                restored.chat_drafts.clone(),
                restored.chat_draft_attachments.clone(),
            );
            cold.durable = Arc::new(restored);
            cold.workspace = Cagents::rebuild_workspace(&cold.durable);
            cold.workspace_model = peer.workspace_model.clone();
            cold.shared_views = crate::app::shared_views::State::for_workspace(&cold.durable);
            cold.session = Some(writer);
            assert!(cold.shared_views.attached.contains(&card));
            // Match application restoration: a saved shared pane first shows
            // its local bytes behind the ordinary shared-admission barrier.
            cold.await_shared_chat_admission(card, source.clone());
            assert!(cold.chat_awaiting_admission(card));
            assert_eq!(runtime(&cold, card).composer.text(), peer_draft);
            assert_eq!(runtime(&cold, card).attachments, images);
            cold.publish_view_changes();
            cold.acknowledge_view_edit(scope.clone(), None);
            let attach = cold.reconcile_shared_views();
            let attempt = cold.chat_ui.attempt(card).unwrap();
            assert_eq!(attempt.intent, chat::Intent::Restore);
            assert_eq!(attempt.session, accepted);
            assert!(attempt.recovery.is_none() && attempt.upgrade_admission.is_none());
            let pending = &cold.chat_ui.shared_draft_attachments[&card];
            assert_eq!(pending.source, source);
            assert_eq!(pending.canonical, accepted);
            assert!(!cold.chat_awaiting_admission(card));
            assert_eq!(cold.durable.cards.len(), 1, "no retained source copy");
            assert!(cold.durable.local_only_cards.is_empty());
            assert!(cold.chat_ui.explicit_starts.is_empty());
            assert!(cold.chat_ui.draft_checkpoints.is_empty());
            assert!(cold.chat_ui.retiring.is_empty() && cold.chat_ui.abandoned.is_empty());
            assert!(
                cold.persistent_workspace()
                    .pending_chat_admissions
                    .is_empty()
            );
            cold.session.as_ref().unwrap().flush(WAIT).unwrap();
            assert_eq!(cold.session.as_ref().unwrap().last_error(), None);
            assert_eq!(stored(&cold, card), source);
            drop(attach); // reserve only; never poll another connection
        }
        assert_eq!(
            peer.chat_ui.attempt(card).unwrap().target,
            attachment_target
        );
        let replies = outputs(attach, false);
        assert_eq!(
            replies
                .iter()
                .filter(|message| matches!(message, Message::ChatUi(Action::Ready(..))))
                .count(),
            1,
            "the actual framed attachment must return one Ready"
        );
        for message in replies {
            let Message::ChatUi(Action::Ready(target, ready)) = &message else {
                panic!("only the shared attachment's Ready is expected");
            };
            assert_eq!(*target, attachment_target);
            {
                let reply = ready.0.lock().unwrap();
                let handle = reply.as_ref().unwrap().as_ref().unwrap();
                assert_eq!(handle.session(), accepted);
                assert_eq!(handle.proof(), accepted.proof);
                assert!(handle.has_observed_user_item());
            }
            drop(peer.update(message));
        }
        let shown = runtime(&peer, card);
        assert_eq!(shown.session, accepted);
        assert!(shown.handle().is_some());
        assert_eq!(shown.composer.text(), peer_draft);
        assert_eq!(shown.attachments, images);
        assert!(shown.queued.is_none() && shown.pending.is_none());
        assert!(shown.followups.is_empty());
        assert!(!peer.chat_start_pending(card));
        assert!(peer.chat_ui.retiring.is_empty() && peer.chat_ui.abandoned.is_empty());
        assert!(peer.chat_ui.explicit_starts.is_empty());
        assert!(peer.chat_ui.draft_checkpoints.is_empty());
        peer.persist();
        reopen_peer_writer(
            &mut peer,
            &peer_database,
            card,
            &accepted,
            &peer_draft,
            &images,
        );
        let seen = observed.lock().unwrap();
        assert_eq!(seen.attaches, vec![accepted.clone()]);
        assert_eq!(
            seen.connects
                .iter()
                .filter(|expected| expected.is_some())
                .collect::<Vec<_>>(),
            vec![&Some(proof())],
            "the shared Restore pins the complete canonical daemon proof"
        );
    }
    cleanup.finish();
    creator_wire.finish();
    let seen = observed.lock().unwrap();
    // This is the sole expected failure at 8da: real cleanup attaches the
    // enriched live B, then sends ChatClose(B), despite accepted creator work.
    assert!(
        seen.closes.is_empty(),
        "passive Restore of published unstarted B must never emit ChatClose for creator-accepted B: lists={}, chat_lists={}, attaches={:?}, closes={:?}",
        seen.lists,
        seen.chat_lists,
        seen.attaches,
        seen.closes
    );
}

/// Isolate draft projection from the writer. An unstarted replacement can
/// carry the editor locally; an initialized replacement must wait for storage.
/// Neither path may acquire provider or retirement authority.
fn peer_replacement_without_writer(initialized: bool) {
    chat::attachment_assets::with_test_image(|first| {
        let second = chat::attachment_assets::test_image("second peer image.png");
        let images = vec![first, second];
        let draft = format!(
            "Keep this peer draft  \n{} {}\n  Unicode λ and trailing spaces  ",
            images[0].reference.as_ref().unwrap().token(),
            images[1].reference.as_ref().unwrap().token(),
        );
        let (mut app, _directory, card, target) = fixture_for_provider(Provider::Claude);
        // The fixture's source is valid and saved. Isolate the subsequent
        // in-memory projection from unrelated identity-save quarantine.
        drop(app.session.take());
        let source = runtime(&app, card).session.clone();
        assert!(source.is_unstarted());
        if let Some(CardRuntime::Chat(runtime)) = app.cards.get_mut(&card) {
            runtime.replace_draft(text_editor::Content::with_text(&draft));
            runtime.replace_attachments(images.clone());
        }
        let channel = crate::model::ChannelId::new();
        Arc::make_mut(&mut app.durable)
            .channels
            .push(crate::model::ProjectChannel {
                id: channel,
                project_id: crate::model::ProjectId::new(),
                title: "Peer composer projection".into(),
                title_source: Default::default(),
                ordinal: 1,
                shared: true,
                cards: vec![card],
                focused: Some(card),
            });
        let scope = Scope::Channel(channel);
        let mut view = View::empty(channel);
        view.revision = 1;
        view.panes
            .push(Pane::from_card(app.card_state(card).unwrap(), None).unwrap());
        app.workspace_model.identity = uuid::Uuid::new_v4();
        app.workspace_model.views = vec![view.clone()];
        // Establish the actual local baseline before the peer changes its
        // existing pane; this is neither an alias nor a recovery admission.
        app.publish_view_changes();
        app.acknowledge_view_edit(scope.clone(), None);
        drop(app.reconcile_shared_views());
        assert_eq!(runtime(&app, card).target, target);
        assert_eq!(runtime(&app, card).composer.text(), draft);
        assert_eq!(runtime(&app, card).attachments, images);
        assert!(app.chat_ui.retiring.is_empty());

        let mut canonical = source.clone();
        canonical.id = TerminalId::new();
        if initialized {
            canonical.proof = Some(proof());
            canonical.launch.session_id = Some("17570000-0000-4000-8000-000000000003".into());
        }
        assert_ne!(canonical.id, source.id);
        assert_eq!(canonical.is_unstarted(), !initialized);
        view.revision += 1;
        view.panes[0].kind = CardKind::Chat(canonical.clone());
        app.workspace_model.views = vec![view];
        let projection = app.reconcile_shared_views();
        let expected = if initialized { source } else { canonical };
        assert_eq!(
            app.card_state(card).unwrap().kind,
            CardKind::Chat(expected.clone())
        );
        assert!(
            app.chat_ui.retiring.is_empty(),
            "replacement grants no retirement authority"
        );
        assert!(
            app.chat_ui.abandoned.is_empty(),
            "no creator start was abandoned here"
        );
        assert!(
            app.chat_ui.draft_checkpoints.is_empty(),
            "no local Send was requested"
        );
        assert!(app.chat_ui.explicit_starts.is_empty());
        // A complete host proof still cannot confirm a replacement without
        // its writer. Keep A and the editor, reserving no passive attempt.
        if initialized {
            assert_eq!(runtime(&app, card).target, target);
        }
        drop(projection); // never connect to a daemon or provider
        assert!(!app.chat_start_pending(card));
        assert!(app.chat_ui.shared_draft_attachments.is_empty());
        let shown = runtime(&app, card);
        assert_eq!(shown.session, expected);
        assert_eq!(shown.composer.text(), draft, "initialized: {initialized}");
        assert_eq!(shown.attachments, images, "initialized: {initialized}");
        assert!(shown.queued.is_none() && shown.pending.is_none());
        assert!(shown.followups.is_empty() && shown.history.is_empty());
        assert!(!shown.closed_elsewhere);
        assert!(app.chat_ui.retiring.is_empty() && app.chat_ui.abandoned.is_empty());
        assert!(!app.chat_start_pending(card));
        let saved = app.persistent_workspace();
        assert_eq!(saved.chat_drafts.get(&card), Some(&draft));
        assert_eq!(saved.chat_draft_attachments.get(&card), Some(&images));
        assert!(!saved.local_only_cards.contains(&card));
        assert!(
            matches!(&saved.cards.iter().find(|state| state.id == card).unwrap().kind,
                CardKind::Chat(session) if session == &expected)
        );
    });
}

#[test]
fn passive_session_replacement_keeps_the_local_unsent_text_and_images() {
    peer_replacement_without_writer(false);
}

#[test]
fn initialized_peer_without_storage_keeps_its_draft_without_attaching() {
    peer_replacement_without_writer(true);
}

#[test]
fn passive_unstarted_peer_restore_cannot_close_creator_accepted_first_prompt() {
    isolated(false, false);
}

#[test]
fn initialized_peer_without_complete_canonical_proof_keeps_its_saved_draft() {
    let mut incomplete = proof();
    incomplete.host_incarnation = crate::model::HostIncarnation::new("");
    assert!(!incomplete.is_complete());
    for canonical_proof in [None, Some(incomplete)] {
        let (mut app, _directory, card, _target) = fixture_for_provider(Provider::Claude);
        let source = runtime(&app, card).session.clone();
        let draft = runtime(&app, card).composer.text();
        let images = runtime(&app, card).attachments.clone();
        let channel = crate::model::ChannelId::new();
        Arc::make_mut(&mut app.durable)
            .channels
            .push(crate::model::ProjectChannel {
                id: channel,
                project_id: ProjectId::new(),
                title: "Waiting for peer proof".into(),
                title_source: Default::default(),
                ordinal: 1,
                shared: true,
                cards: vec![card],
                focused: Some(card),
            });
        let scope = Scope::Channel(channel);
        let mut view = View::empty(channel);
        view.revision = 1;
        view.panes
            .push(Pane::from_card(app.card_state(card).unwrap(), None).unwrap());
        app.workspace_model.identity = uuid::Uuid::new_v4();
        app.workspace_model.views = vec![view.clone()];
        app.publish_view_changes();
        app.acknowledge_view_edit(scope.clone(), None);
        drop(app.reconcile_shared_views());
        app.session.as_ref().unwrap().flush(WAIT).unwrap();
        assert_eq!(app.session.as_ref().unwrap().last_error(), None);
        assert_eq!(stored(&app, card), source);

        let mut canonical = source.clone();
        canonical.id = TerminalId::new();
        canonical.launch.session_id = Some("17570000-0000-4000-8000-000000000004".into());
        canonical.proof = canonical_proof;
        assert!(!canonical.is_unstarted());
        view.revision += 1;
        view.panes[0].kind = CardKind::Chat(canonical);
        app.workspace_model.views = vec![view];
        // Even the unpolled task may not reserve a Start/Restore attempt:
        // this descriptor has not established its authoritative host.
        drop(app.reconcile_shared_views());
        assert!(!app.chat_start_pending(card));
        assert!(app.chat_ui.shared_draft_attachments.is_empty());
        assert!(app.chat_ui.draft_checkpoints.is_empty());
        assert!(app.chat_ui.explicit_starts.is_empty());
        assert!(app.chat_ui.retiring.is_empty() && app.chat_ui.abandoned.is_empty());
        let shown = runtime(&app, card);
        assert_eq!(shown.session, source);
        assert_eq!(shown.composer.text(), draft);
        assert_eq!(shown.attachments, images);
        assert!(shown.queued.is_none() && shown.pending.is_none());
        app.persist();
        app.session.as_ref().unwrap().flush(WAIT).unwrap();
        assert_eq!(app.session.as_ref().unwrap().last_error(), None);
        assert_eq!(stored(&app, card), source);
    }
}

#[test]
fn passive_initialized_peer_restore_schedules_attachment_without_retirement() {
    isolated(true, false);
}

#[test]
fn passive_sequential_provider_id_enrichment_settles_after_another_window_wins() {
    isolated(false, true);
}

#[test]
fn passive_direct_provider_id_enrichment_settles_after_another_window_wins() {
    isolated(true, true);
}
