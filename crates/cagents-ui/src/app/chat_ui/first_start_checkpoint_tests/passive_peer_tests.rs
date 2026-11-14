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
fn isolated(initialized: bool) {
    let name = std::thread::current().name().unwrap().to_owned();
    if std::env::var(CHILD).as_deref() == Ok(name.as_str()) {
        run(initialized);
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
fn handshake(stream: &mut UnixStream) {
    stream.set_read_timeout(Some(WAIT)).unwrap();
    stream.set_write_timeout(Some(WAIT)).unwrap();
    let request: ClientEnvelope = crate::terminal_ipc::read_frame(stream).unwrap().unwrap();
    assert_eq!(request.version, PROTOCOL_VERSION);
    assert_eq!(request.proof, None);
    assert!(matches!(
        request.request,
        ClientRequest::Connect {
            expected_host: None,
            ..
        }
    ));
    respond(
        stream,
        request.request_id,
        ServerResponse::Connected {
            proof: proof(),
            resumed: vec![],
        },
    );
}

#[derive(Default)]
struct Observed {
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
    let (stop, stopped) = mpsc::channel();
    let worker = std::thread::spawn(move || {
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
            handshake(&mut stream);
            for _ in 0..8 {
                let request = request(&mut stream);
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
                        let (expected, snapshot) = live.lock().unwrap().clone().unwrap();
                        assert_eq!(
                            session, expected,
                            "cleanup attaches the live inventory's exact descriptor"
                        );
                        observed.lock().unwrap().attaches.push(session.clone());
                        ServerResponse::ChatAttached { session, snapshot }
                    }
                    ClientRequest::ChatClose { chat_id } => {
                        let (session, snapshot) = live.lock().unwrap().clone().unwrap();
                        assert_eq!(chat_id, session.id);
                        assert_eq!(observed.lock().unwrap().attaches.last(), Some(&session));
                        observed.lock().unwrap().closes.push(chat_id);
                        respond(
                            &mut stream,
                            request.request_id,
                            ServerResponse::ChatClosed { chat_id, snapshot },
                        );
                        break;
                    }
                    other => panic!("unexpected cleanup request: {other:?}"),
                };
                respond(&mut stream, request.request_id, response);
            }
        }
        panic!("private accept loop exhausted its 40-second/count bound");
    });
    WirePeer {
        stopping: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        stop: Some(stop),
        sockets,
        worker: Some(worker),
    }
}

fn creator_peer(
    actor: TerminalId,
    live: Arc<Mutex<Option<(chat::Session, Snapshot)>>>,
) -> (UnixStream, WirePeer) {
    let (client, mut server) = UnixStream::pair().unwrap();
    let sockets = Arc::new(Mutex::new(vec![server.try_clone().unwrap()]));
    let stopping = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stopping_worker = stopping.clone();
    let worker = std::thread::spawn(move || {
        handshake(&mut server);
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
        session.launch.session_id = Some("17570000-0000-4000-8000-000000000002".into());
        let initial = Snapshot {
            generation: 1,
            ready: true,
            session_id: session.launch.session_id.clone(),
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

fn run(initialized: bool) {
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
    peer.durable = creator.durable.clone();
    Arc::make_mut(&mut peer.durable).active_channel = None;
    peer.workspace_model = creator.workspace_model.clone();
    drop(peer.open_chat(card, source.clone(), chat::Intent::Start));
    peer.publish_view_changes();
    peer.acknowledge_view_edit(scope.clone(), None);
    drop(peer.reconcile_shared_views());
    assert!(peer.chat_ui.retiring.is_empty());

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
    let change = std::iter::from_fn(|| published.try_recv().ok())
        .find_map(|edit| match edit {
            ModelEdit::ChangeView {
                scope: edit_scope,
                change: change @ Change::Update { .. },
                ..
            } if edit_scope == scope
                && matches!(&change, Change::Update { before, pane }
                if before.id == card && before.kind == CardKind::Chat(source.clone())
                    && pane.kind == CardKind::Chat(candidate.clone())) =>
            {
                Some(change)
            }
            _ => None,
        })
        .expect("checkpoint reducer must really publish unstarted B replacing A");
    assert!(view.apply(&change).unwrap());
    view.revision += 1;
    peer.workspace_model.views = vec![view.clone()];
    // In the regression, capture the actual peer projection's task now and do
    // not poll it until B has accepted the creator's original prompt.
    let delayed = (!initialized).then(|| peer.reconcile_shared_views());
    assert_eq!(
        peer.card_state(card).unwrap().kind,
        if initialized {
            CardKind::Chat(source.clone())
        } else {
            CardKind::Chat(candidate.clone())
        }
    );

    let (client, creator_wire) = creator_peer(candidate.id, live.clone());
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

    if initialized {
        // Actual creator publication now supplies initialized B. A passive peer
        // receiving this descriptor goes to ordinary Restore/Starting, never
        // auto-retirement. Do not poll that guarded daemon-discovery task.
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
        peer.workspace_model.views = vec![view];
        let attach = peer.reconcile_shared_views();
        assert!(
            peer.chat_ui.retiring.is_empty(),
            "initialized peer Restore must not schedule retirement"
        );
        assert!(peer.chat_start_matches(card, &accepted));
        drop(attach);
    } else {
        for message in outputs(delayed.unwrap(), false) {
            assert!(
                matches!(
                    &message,
                    Message::ChatUi(Action::Retired(_, Ok(())) | Action::DraftSkills(..))
                ),
                "unexpected peer task output: {message:?}"
            );
            // Apply task replies, dropping any follow-on work before another
            // discovery path could run. The actual cleanup already completed.
            drop(peer.update(message));
        }
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

#[test]
fn passive_unstarted_peer_restore_cannot_close_creator_accepted_first_prompt() {
    isolated(false);
}

#[test]
fn passive_initialized_peer_restore_schedules_attachment_without_retirement() {
    isolated(true);
}
