//! Owning channel deletion and real private framed close helpers.
//! Synthetic peers exercise client custody, not Host authorization or providers.
use super::*;
use crate::{
    agent_execution::Authority,
    model::{BootId, HostIncarnation, HostSessionId, HostSessionProof},
    terminal::ipc::{
        ClientEnvelope, ClientRequest, PROTOCOL_VERSION, ProtocolError, ProtocolErrorCode,
        ServerEnvelope, ServerMessage, ServerResponse, TerminalDescriptor, read_frame, write_frame,
    },
    workspace_model::WorkspaceModel,
};
use cosmic::iced::futures::StreamExt;
use std::{
    ffi::OsString,
    fs::{self, File},
    io,
    os::unix::{
        ffi::OsStringExt,
        fs::PermissionsExt,
        net::{UnixListener, UnixStream},
    },
    process::{Command, Stdio},
    thread::{self, JoinHandle},
};

const ROLE: &str = "CAGENTS_TEST_CHANNEL_CLOSE_ROLE";
const ROOT: &str = "CAGENTS_TEST_CHANNEL_CLOSE_ROOT";
const TOKEN: &str = "synthetic-original-channel-close-scope";
const REFUSAL: &str = "Synthetic peer refused original channel close scope";

fn proof() -> HostSessionProof {
    HostSessionProof {
        host_session_id: HostSessionId::new("channel-close-fixture"),
        boot_id: BootId::new("channel-close-boot"),
        host_incarnation: HostIncarnation::new("channel-close-incarnation"),
    }
}
fn authority(role: &str) -> Authority {
    match role {
        "user" => Authority::default(),
        "empty" | "non-unicode" => Authority::session(String::new()),
        _ => Authority::session(TOKEN.into()),
    }
}
struct Connection(UnixStream);
impl Connection {
    fn next(&mut self) -> ClientEnvelope {
        let request: ClientEnvelope = read_frame(&mut self.0).unwrap().unwrap();
        assert_eq!(request.proof.as_ref(), Some(&proof()));
        request
    }
    fn answer(&mut self, request: Option<&ClientEnvelope>, message: ServerMessage) {
        write_frame(
            &mut self.0,
            &ServerEnvelope {
                version: PROTOCOL_VERSION,
                request_id: request.map(|request| request.request_id),
                proof: proof(),
                message,
            },
        )
        .unwrap();
    }
    fn response(&mut self, request: &ClientEnvelope, response: ServerResponse) {
        self.answer(Some(request), ServerMessage::Response { response });
    }
    fn refuse(&mut self, request: &ClientEnvelope) {
        self.answer(
            Some(request),
            ServerMessage::Error(ProtocolError::new(
                ProtocolErrorCode::PermissionDenied,
                REFUSAL,
                false,
            )),
        );
    }
    fn eof(mut self) {
        assert!(
            read_frame::<ClientEnvelope>(&mut self.0).unwrap().is_none(),
            "no extra request or User retry"
        );
    }
    fn terminals(&mut self, terminals: Vec<TerminalDescriptor>) {
        let request = self.next();
        assert!(matches!(request.request, ClientRequest::List));
        self.response(&request, ServerResponse::Listed { terminals });
    }
    fn chats(&mut self, sessions: Vec<crate::chat::Session>) {
        let request = self.next();
        assert!(matches!(request.request, ClientRequest::ChatList));
        self.response(&request, ServerResponse::ChatListed { sessions });
    }
    fn close_chat(&mut self, session: &crate::chat::Session) -> ClientEnvelope {
        let attach = self.next();
        assert!(matches!(&attach.request,
            ClientRequest::ChatAttach { session: actual } if actual == session));
        self.response(
            &attach,
            ServerResponse::ChatAttached {
                session: session.clone(),
                snapshot: Default::default(),
            },
        );
        let close = self.next();
        assert!(
            matches!(close.request, ClientRequest::ChatClose { chat_id } if chat_id == session.id)
        );
        close
    }
    fn close_terminal(&mut self, terminal: &TerminalDescriptor) -> ClientEnvelope {
        let attach = self.next();
        assert!(
            matches!(attach.request, ClientRequest::Attach { terminal_id, .. } if terminal_id == terminal.session.id)
        );
        self.response(
            &attach,
            ServerResponse::Attached {
                terminal: terminal.clone(),
            },
        );
        self.answer(
            None,
            ServerMessage::Synchronized {
                terminal_id: terminal.session.id,
                generation: 0,
                event_sequence: 0,
            },
        );
        let close = self.next();
        assert!(matches!(close.request,
            ClientRequest::CloseTerminalChecked { terminal_id, allow_active: true } if terminal_id == terminal.session.id));
        close
    }
}
struct Server(UnixListener);
impl Server {
    fn connect(
        &mut self,
        authority: &Authority,
        registry: Option<uuid::Uuid>,
        refuse_auth: bool,
    ) -> Option<Connection> {
        let deadline = Instant::now() + Duration::from_secs(8);
        let mut connection = loop {
            match self.0.accept() {
                Ok((stream, _)) => break Connection(stream),
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    assert!(
                        Instant::now() < deadline,
                        "helper did not reach its private peer"
                    );
                    thread::sleep(Duration::from_millis(2));
                }
                Err(error) => panic!("private accept: {error}"),
            }
        };
        connection
            .0
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        connection
            .0
            .set_write_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let hello: ClientEnvelope = read_frame(&mut connection.0).unwrap().unwrap();
        assert!(matches!(hello.request, ClientRequest::Connect { .. }));
        assert!(hello.proof.is_none());
        connection.response(
            &hello,
            ServerResponse::Connected {
                proof: proof(),
                resumed: vec![],
            },
        );
        if authority.token().is_some() {
            let request = connection.next();
            assert!(
                matches!(&request.request, ClientRequest::Authenticate { authority: actual } if actual == authority),
                "original scope before any inventory or recovery"
            );
            if refuse_auth {
                connection.refuse(&request);
                connection.eof();
                return None;
            }
            connection.response(&request, ServerResponse::Accepted);
        }
        if let Some(identity) = registry {
            let request = connection.next();
            assert!(matches!(request.request, ClientRequest::ReadWorkspaceModel));
            connection.response(
                &request,
                ServerResponse::WorkspaceModel {
                    model: WorkspaceModel {
                        identity,
                        sequence: 7,
                        ..Default::default()
                    },
                },
            );
        }
        Some(connection)
    }
}
struct Peer(Option<JoinHandle<Server>>);
impl Peer {
    fn new(script: impl FnOnce(&mut Server) + Send + 'static) -> Self {
        let root = PathBuf::from(std::env::var_os(ROOT).unwrap());
        let socket = crate::terminal::ipc::daemon_socket_path().unwrap();
        assert!(socket.starts_with(&root));
        fs::create_dir_all(socket.parent().unwrap()).unwrap();
        if socket.exists() {
            fs::remove_file(&socket).unwrap();
        }
        let listener = UnixListener::bind(socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        Self(Some(thread::spawn(move || {
            let mut server = Server(listener);
            script(&mut server);
            server
        })))
    }
    fn finished(mut self) {
        let server = self.0.take().unwrap().join().unwrap();
        assert!(
            matches!(server.0.accept(), Err(error) if error.kind() == io::ErrorKind::WouldBlock),
            "no readiness or fallback connection"
        );
    }
}
impl Drop for Peer {
    fn drop(&mut self) {
        if let Some(worker) = self.0.take() {
            let _ = worker.join();
        }
    }
}

fn close_message(task: Task<Message>) -> Message {
    let mut stream = cosmic::iced::runtime::task::into_stream(task).expect("actual close task");
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async move {
            while let Some(action) = stream.next().await {
                if let cosmic::iced::runtime::Action::Output(cosmic::Action::App(
                    message @ Message::ChannelUi(Action::SessionsClosed(..)),
                )) = action
                {
                    return message;
                }
            }
            panic!("channel close task produced no owning completion")
        })
}

fn owning_delete(role: &str) {
    let original = authority(role);
    let actorless = role == "actorless";
    for refuse_auth in if original.token().is_some() {
        vec![true, false]
    } else {
        vec![false]
    } {
        let (mut app, channel, survivor, card) = tests::channel_with_a_conversation();
        assert!(app.session.is_none());
        // Keep the target inactive: closure refusal must not change focus, and
        // driving the returned task runs no unrelated navigation/start tasks.
        Arc::make_mut(&mut app.durable).active_channel = Some(survivor);
        let registry = uuid::Uuid::new_v4();
        app.workspace_model.identity = registry;
        Arc::make_mut(&mut app.durable).workspace_model_identity = Some(registry);
        if actorless {
            let durable = Arc::make_mut(&mut app.durable);
            durable
                .channels
                .iter_mut()
                .find(|row| row.id == channel)
                .unwrap()
                .cards
                .clear();
            durable
                .channels
                .iter_mut()
                .find(|row| row.id == channel)
                .unwrap()
                .focused = None;
            durable.cards.retain(|state| state.id != card);
        } else {
            let drafts = HashMap::from([(card, "Keep this unsent channel draft".into())]);
            app.chat_ui = crate::app::chat_ui::State::with_drafts(drafts.clone());
            Arc::make_mut(&mut app.durable).chat_drafts = drafts;
        }
        let saved = app.card_state(card).cloned();
        let drafts = app.persistent_workspace().chat_drafts.clone();
        let focused = app.workspace.focused();
        let chat = saved.as_ref().and_then(|state| match &state.kind {
            CardKind::Chat(session) => Some(session.clone()),
            _ => None,
        });
        let (tx, publications) = async_channel::unbounded();
        app.workspace_model_tx = tx;
        let expected = original.clone();
        let peer = Peer::new(move |server| {
            let Some(mut connection) = server.connect(&expected, Some(registry), refuse_auth)
            else {
                return;
            };
            connection.terminals(vec![]);
            if expected.token().is_some() {
                let request = connection.next();
                assert!(matches!(request.request, ClientRequest::ChatList));
                connection.refuse(&request);
            } else {
                let chat = chat.unwrap();
                connection.chats(vec![chat.clone()]);
                let close = connection.close_chat(&chat);
                connection.response(
                    &close,
                    ServerResponse::ChatClosed {
                        chat_id: chat.id,
                        snapshot: Default::default(),
                    },
                );
            }
            connection.eof();
        });
        let target = app.channel_deletion_target(channel).unwrap();
        let task = app.delete_confirmed_channel(channel, target);
        assert!(app.durable.channels.iter().any(|row| row.id == channel));
        assert!(!app.channel_ui.was_deleted(channel));
        let completion = close_message(task);
        assert!(
            matches!(&completion, Message::ChannelUi(Action::SessionsClosed(id, _, result)) if *id == channel && result.is_err() == original.token().is_some())
        );
        drop(app.update(completion));
        peer.finished();
        let edits = std::iter::from_fn(|| publications.try_recv().ok()).collect::<Vec<_>>();
        if original.token().is_some() {
            assert!(app.durable.channels.iter().any(|row| row.id == channel));
            assert!(!app.channel_ui.is_deleting(channel) && !app.channel_ui.was_deleted(channel));
            assert!(app.channel_ui.failed.contains_key(&channel));
            assert_eq!(app.card_state(card), saved.as_ref());
            assert_eq!(app.persistent_workspace().chat_drafts, drafts);
            assert_eq!(app.workspace.focused(), focused);
            assert_eq!(app.durable.active_channel, Some(survivor));
            assert!(
                !edits
                    .iter()
                    .any(|edit| matches!(edit, ModelEdit::DeleteChannel { id } if *id == channel))
            );
        } else {
            assert!(app.channel_ui.was_deleted(channel));
            assert!(!app.card_state(card).unwrap().existence.is_open());
            assert!(
                edits
                    .iter()
                    .any(|edit| matches!(edit, ModelEdit::DeleteChannel { id } if *id == channel))
            );
        }
    }
}
#[test]
fn owning_channel_delete_task_preserves_scoped_conversations_and_actorless_channels() {
    isolated(
        &["user", "present", "empty", "non-unicode", "actorless"],
        owning_delete,
    );
}

fn chat_close_message(task: Task<Message>) -> Message {
    use crate::app::chat_ui::Action as ChatAction;
    let mut stream =
        cosmic::iced::runtime::task::into_stream(task).expect("actual chat close task");
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async move {
            while let Some(action) = stream.next().await {
                if let cosmic::iced::runtime::Action::Output(cosmic::Action::App(
                    message @ Message::ChatUi(
                        ChatAction::ScopedParkedCloseFinished(..) | ChatAction::CloseFinished(..),
                    ),
                )) = action
                {
                    return message;
                }
            }
            panic!("chat close task produced no owning completion")
        })
}

fn scoped_live_surface_close(role: &str) {
    use crate::app::chat_ui::{self, Action as ChatAction};
    let original = authority(role);
    for parked in [false, true] {
        for refuse_auth in [true, false] {
            let mut app = crate::app::tests::context_inspector_test_app(false);
            let root = PathBuf::from(std::env::var_os(ROOT).unwrap());
            let runtime = root.join(format!("surface-runtime-{parked}-{refuse_auth}"));
            fs::create_dir(&runtime).unwrap();
            app.worktree_protections.use_fixture_runtime(runtime);
            let (card, target) = if parked {
                chat_ui::tests::parked_close_scope_fixture(&mut app)
            } else {
                chat_ui::tests::unresumable_chat(&mut app)
            };
            let registry = uuid::Uuid::new_v4();
            app.workspace_model.identity = registry;
            Arc::make_mut(&mut app.durable).workspace_model_identity = Some(registry);
            let before = app.card_state(card).unwrap().clone();
            let drafts = app.persistent_workspace().chat_drafts.clone();
            let focused = app.workspace.focused();
            let expected = original.clone();
            let peer = Peer::new(move |server| {
                let Some(mut connection) = server.connect(&expected, Some(registry), refuse_auth)
                else {
                    return;
                };
                connection.terminals(vec![]);
                let census = connection.next();
                assert!(matches!(census.request, ClientRequest::ChatList));
                connection.refuse(&census);
                connection.eof();
            });
            // The fixture handle has no responding peer. A regression to its
            // User close path cannot satisfy the private authenticated peer.
            let task = if parked {
                app.begin_chat_close(card)
            } else {
                app.handle_chat_ui(ChatAction::StartFresh(card))
            };
            let completion = chat_close_message(task);
            assert!(
                matches!(&completion,
                Message::ChatUi(ChatAction::ScopedParkedCloseFinished(current, Err(_))) if parked && *current == target)
                    || matches!(&completion,
                    Message::ChatUi(ChatAction::CloseFinished(current, Err(_))) if !parked && *current == target)
            );
            drop(app.update(completion));
            peer.finished();
            assert_eq!(app.card_state(card), Some(&before));
            assert_eq!(app.persistent_workspace().chat_drafts, drafts);
            assert_eq!(app.workspace.focused(), focused);
            assert!(!app.chat_ui.transition_pending());
            assert!(!app.chat_start_pending(card));
            assert_eq!(app.workspace.pending_close(), None);
            assert_eq!(app.chat_has_parked_runtime(card), parked);
        }
    }
}

#[test]
fn scoped_parked_close_and_fresh_start_keep_the_original_surface_on_refusal() {
    isolated(
        &["present", "empty", "non-unicode"],
        scoped_live_surface_close,
    );
}

fn sessions() -> (crate::chat::Session, TerminalDescriptor) {
    let chat = tests::chat_session();
    let CardKind::Terminal(session) = crate::app::tests::terminal_card_state().kind else {
        unreachable!()
    };
    let terminal = TerminalDescriptor::relaunchable(session.id, session.launch, None);
    (chat, terminal)
}
fn recovery(role: &str) {
    let registry = uuid::Uuid::new_v4();
    let context = SessionCloseContext::capture(registry);
    let original = authority(role);
    let (chat, terminal) = sessions();
    let kinds = vec![
        CardKind::Chat(chat.clone()),
        CardKind::Terminal(terminal.session.clone()),
    ];
    // SAFETY: env-cleared, dedicated child; only our synthetic credential
    // changes before creating any peer/runtime thread. It is never printed.
    unsafe {
        std::env::set_var(
            crate::agent_execution::AUTHORITY_ENV,
            "synthetic-later-channel-scope",
        );
    }
    let peer = Peer::new(move |server| {
        // Deliberately permit scoped censuses to exercise both recovery paths;
        // this is transport coverage, not the real Host's permission policy.
        let mut initial = server.connect(&original, Some(registry), false).unwrap();
        initial.terminals(vec![terminal.clone()]);
        initial.chats(vec![chat.clone()]);
        initial.close_chat(&chat);
        drop(initial);
        let mut chat_recovery = server.connect(&original, Some(registry), false).unwrap();
        chat_recovery.chats(vec![]);
        chat_recovery.close_terminal(&terminal);
        drop(chat_recovery);
        let mut terminal_recovery = server.connect(&original, Some(registry), false).unwrap();
        terminal_recovery.terminals(vec![]);
        terminal_recovery.eof();
    });
    close_sessions(kinds, &context).unwrap();
    peer.finished();
}
#[test]
fn session_close_recoveries_preserve_original_caller_and_registry() {
    isolated(&["user", "present"], recovery);
}

fn foreign_registry(role: &str) {
    let registry = uuid::Uuid::new_v4();
    let foreign = uuid::Uuid::new_v4();
    assert_ne!(registry, foreign);
    let context = SessionCloseContext::capture(registry);
    let original = authority(role);
    let (chat, terminal) = sessions();
    let kinds = vec![
        CardKind::Chat(chat.clone()),
        CardKind::Terminal(terminal.session.clone()),
    ];
    let stage = role.to_owned();
    let peer = Peer::new(move |server| {
        if stage == "initial" {
            server
                .connect(&original, Some(foreign), false)
                .unwrap()
                .eof();
            return;
        }
        let mut initial = server.connect(&original, Some(registry), false).unwrap();
        initial.terminals(vec![terminal.clone()]);
        if stage == "chat" {
            initial.chats(vec![chat.clone()]);
            initial.close_chat(&chat);
        } else {
            initial.chats(vec![]);
            initial.close_terminal(&terminal);
        }
        drop(initial);
        server
            .connect(&original, Some(foreign), false)
            .unwrap()
            .eof();
    });
    assert!(
        close_sessions(kinds, &context)
            .unwrap_err()
            .contains("workspace connection changed")
    );
    peer.finished();
}
#[test]
fn session_close_refuses_foreign_initial_and_recovery_registries_before_more_effects() {
    isolated(&["initial", "chat", "terminal"], foreign_registry);
}

fn absence_or_remote(role: &str) {
    let root = PathBuf::from(std::env::var_os(ROOT).unwrap());
    if role == "remote" {
        let config = crate::device::configuration_path().unwrap();
        fs::create_dir_all(config.parent().unwrap()).unwrap();
        fs::write(config, r#"{"devices":[{"name":"fixture","host":"channel-fixture.invalid","executable":"cagents"}]}"#).unwrap();
        crate::device::select(Some("fixture")).unwrap();
        let context = SessionCloseContext::capture(crate::workspace_model::NO_REGISTRY);
        assert!(
            close_sessions(vec![], &context)
                .unwrap_err()
                .contains("remote device")
        );
        assert!(!root.join("ssh-entered").exists());
        return;
    }
    let context = SessionCloseContext::capture(crate::workspace_model::NO_REGISTRY);
    let socket = crate::terminal::ipc::daemon_socket_path().unwrap();
    fs::create_dir_all(socket.parent().unwrap()).unwrap();
    let (chat, terminal) = sessions();
    let scoped = authority(role).token().is_some();
    for stale in [false, true] {
        if socket.exists() {
            fs::remove_file(&socket).unwrap();
        }
        if stale {
            drop(UnixListener::bind(&socket).unwrap());
        }
        for kinds in [
            vec![],
            vec![CardKind::Terminal(terminal.session.clone())],
            vec![CardKind::Chat(chat.clone())],
        ] {
            let result = close_sessions(kinds, &context);
            if scoped {
                assert!(result.unwrap_err().contains("daemon is unavailable"));
            } else {
                result.unwrap();
            }
        }
        let mut retained = chat.clone();
        retained.native_continuity = crate::chat::NativeContinuity::Durable {
            provider_version: "fixture".into(),
        };
        assert!(close_sessions(vec![CardKind::Chat(retained)], &context).is_err());
        assert_eq!(socket.exists(), stale);
        assert_eq!(
            UnixStream::connect(&socket).unwrap_err().kind(),
            if stale {
                io::ErrorKind::ConnectionRefused
            } else {
                io::ErrorKind::NotFound
            }
        );
    }
}
#[test]
fn session_close_absence_and_remote_admission_do_not_manufacture_scoped_success() {
    isolated(
        &["user", "present", "empty", "non-unicode", "remote"],
        absence_or_remote,
    );
}

fn isolated(roles: &[&str], exercise: fn(&str)) {
    if let Some(role) = std::env::var_os(ROLE) {
        let role = role.to_str().unwrap();
        assert!(roles.contains(&role));
        exercise(role);
        return;
    }
    let selector = thread::current().name().unwrap().to_owned();
    for role in roles {
        let private = tempfile::Builder::new().prefix("cc-").tempdir().unwrap();
        let root = private.path().canonicalize().unwrap();
        for leaf in ["home", "runtime", "config", "data", "cache", "state", "bin"] {
            let path = root.join(leaf);
            fs::create_dir(&path).unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
        }
        let ssh = root.join("bin/ssh");
        fs::write(
            &ssh,
            "#!/bin/sh\n: > \"$CAGENTS_TEST_CHANNEL_CLOSE_ROOT/ssh-entered\"\nexit 73\n",
        )
        .unwrap();
        fs::set_permissions(ssh, fs::Permissions::from_mode(0o700)).unwrap();
        let stdout = root.join("stdout");
        let stderr = root.join("stderr");
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .env_clear()
            .args(["--exact", &selector, "--nocapture", "--test-threads=1"])
            .current_dir(&root)
            .env(ROLE, role)
            .env(ROOT, &root)
            .env("PATH", root.join("bin"))
            .env("HOME", root.join("home"))
            .env("XDG_RUNTIME_DIR", root.join("runtime"))
            .env("XDG_CONFIG_HOME", root.join("config"))
            .env("XDG_DATA_HOME", root.join("data"))
            .env("XDG_CACHE_HOME", root.join("cache"))
            .env("XDG_STATE_HOME", root.join("state"))
            .stdin(Stdio::null())
            .stdout(File::create(&stdout).unwrap())
            .stderr(File::create(&stderr).unwrap());
        if *role != "user" {
            command.env(
                crate::agent_execution::AUTHORITY_ENV,
                match *role {
                    "empty" => OsString::new(),
                    "non-unicode" => OsString::from_vec(vec![0xff]),
                    _ => OsString::from(TOKEN),
                },
            );
        }
        let mut child = command.spawn().unwrap();
        let deadline = Instant::now() + Duration::from_secs(35);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if Instant::now() >= deadline {
                child.kill().unwrap();
                child.wait().unwrap();
                panic!("private channel close child exceeded bound: {role}");
            }
            thread::sleep(Duration::from_millis(5));
        };
        assert!(fs::metadata(&stdout).unwrap().len() <= 1024 * 1024);
        assert!(fs::metadata(&stderr).unwrap().len() <= 1024 * 1024);
        let stdout = fs::read_to_string(stdout).unwrap();
        let stderr = fs::read_to_string(stderr).unwrap();
        assert!(!stdout.contains(TOKEN) && !stderr.contains(TOKEN));
        assert!(status.success(), "{role}: {stdout}\n{stderr}");
        assert!(stdout.contains("1 passed; 0 failed; 0 ignored;"));
        assert!(!root.join("ssh-entered").exists());
    }
}
