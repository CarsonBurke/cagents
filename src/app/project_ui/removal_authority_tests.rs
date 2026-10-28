//! Real removal helpers against private framed peers in env-cleared children.
//! Synthetic authentication proves client custody, not Host authorization.
//! Recovery deliberately permits otherwise forbidden scoped inventories to
//! reach every reconnect; no daemon, provider or filesystem removal runs.
use super::*;
use crate::{
    agent_execution::Authority,
    model::{BootId, HostIncarnation, HostSessionId, HostSessionProof, TerminalExit},
    terminal::ipc::{
        ClientEnvelope, ClientRequest, PROTOCOL_VERSION, ProtocolError, ProtocolErrorCode,
        ServerEnvelope, ServerMessage, ServerResponse, TerminalCloseResult, TerminalDescriptor,
        read_frame, write_frame,
    },
    workspace_model::{ModelEdit, ModelProject, WorkspaceModel},
};
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

const ROLE: &str = "CAGENTS_TEST_REMOVAL_SCOPE_ROLE";
const ROOT: &str = "CAGENTS_TEST_REMOVAL_PRIVATE_ROOT";
const TOKEN: &str = "synthetic-removal-original-scope";
const REFUSAL: &str = "Synthetic peer refused the original removal scope";

fn proof() -> HostSessionProof {
    HostSessionProof {
        host_session_id: HostSessionId::new("removal-fixture"),
        boot_id: BootId::new("removal-boot"),
        host_incarnation: HostIncarnation::new("removal-incarnation"),
    }
}

struct Connection {
    stream: UnixStream,
    first: Option<ClientEnvelope>,
}
impl Connection {
    fn request(&mut self) -> ClientEnvelope {
        let request = self
            .first
            .take()
            .unwrap_or_else(|| read_frame(&mut self.stream).unwrap().unwrap());
        assert_eq!(request.proof.as_ref(), Some(&proof()));
        request
    }
    fn answer(&mut self, request: &ClientEnvelope, message: ServerMessage) {
        write_frame(
            &mut self.stream,
            &ServerEnvelope {
                version: PROTOCOL_VERSION,
                request_id: Some(request.request_id),
                proof: proof(),
                message,
            },
        )
        .unwrap();
    }
    fn response(&mut self, request: &ClientEnvelope, response: ServerResponse) {
        self.answer(request, ServerMessage::Response { response });
    }
    fn refuse(&mut self, request: &ClientEnvelope) {
        self.answer(
            request,
            ServerMessage::Error(ProtocolError::new(
                ProtocolErrorCode::PermissionDenied,
                REFUSAL,
                false,
            )),
        );
    }
    fn eof(mut self) {
        assert!(
            read_frame::<ClientEnvelope>(&mut self.stream)
                .unwrap()
                .is_none()
        );
    }
    fn model(&mut self, model: &WorkspaceModel) {
        let request = self.request();
        assert!(matches!(request.request, ClientRequest::ReadWorkspaceModel));
        self.response(
            &request,
            ServerResponse::WorkspaceModel {
                model: model.clone(),
            },
        );
    }
    fn chats(&mut self, sessions: Vec<crate::chat::Session>) {
        let request = self.request();
        assert!(matches!(request.request, ClientRequest::ChatList));
        self.response(&request, ServerResponse::ChatListed { sessions });
    }
    fn terminals(&mut self, terminals: Vec<TerminalDescriptor>) {
        let request = self.request();
        assert!(matches!(request.request, ClientRequest::List));
        self.response(&request, ServerResponse::Listed { terminals });
    }
    fn chat_close(&mut self, session: &crate::chat::Session) -> ClientEnvelope {
        let attach = self.request();
        assert!(
            matches!(&attach.request, ClientRequest::ChatAttach { session: actual } if actual == session)
        );
        self.response(
            &attach,
            ServerResponse::ChatAttached {
                session: session.clone(),
                snapshot: Default::default(),
            },
        );
        let close = self.request();
        assert!(
            matches!(close.request, ClientRequest::ChatClose { chat_id } if chat_id == session.id)
        );
        close
    }
    fn terminal_close(&mut self, terminal: &TerminalDescriptor) -> ClientEnvelope {
        let close = self.request();
        assert!(
            matches!(&close.request, ClientRequest::ReleaseCompletedTerminal { session, allow_active: true } if session == &terminal.session)
        );
        close
    }
}

struct Server {
    listener: UnixListener,
    probes: usize,
}
impl Server {
    fn connect(&mut self, authority: &Authority, refuse: bool) -> Option<Connection> {
        let deadline = Instant::now() + Duration::from_secs(8);
        loop {
            let (mut stream, _) = loop {
                match self.listener.accept() {
                    Ok(accepted) => break accepted,
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline, "helper did not connect");
                        thread::sleep(Duration::from_millis(2));
                    }
                    Err(error) => panic!("private accept: {error}"),
                }
            };
            stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            stream
                .set_write_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let Some(hello) = read_frame::<ClientEnvelope>(&mut stream).unwrap() else {
                self.probes += 1;
                assert!(self.probes <= 1, "no repeated raw readiness probes");
                continue;
            };
            assert!(matches!(hello.request, ClientRequest::Connect { .. }));
            assert!(hello.proof.is_none());
            let mut connection = Connection {
                stream,
                first: None,
            };
            connection.response(
                &hello,
                ServerResponse::Connected {
                    proof: proof(),
                    resumed: vec![],
                },
            );
            let Some(request) = read_frame::<ClientEnvelope>(&mut connection.stream).unwrap()
            else {
                self.probes += 1;
                assert!(self.probes <= 1, "no repeated protocol readiness probes");
                continue;
            };
            if authority.token().is_some() {
                assert!(
                    matches!(&request.request, ClientRequest::Authenticate { authority: actual } if actual == authority),
                    "scope must precede every inventory, recovery and final edit"
                );
                assert_eq!(request.proof.as_ref(), Some(&proof()));
                if refuse {
                    connection.refuse(&request);
                    connection.eof();
                    return None;
                }
                connection.response(&request, ServerResponse::Accepted);
            } else {
                connection.first = Some(request);
            }
            return Some(connection);
        }
    }
}

struct Peer {
    worker: Option<JoinHandle<Server>>,
}
impl Peer {
    fn new(script: impl FnOnce(&mut Server) + Send + 'static) -> Self {
        let root = PathBuf::from(std::env::var_os(ROOT).unwrap());
        let socket = crate::terminal::ipc::daemon_socket_path().unwrap();
        assert!(socket.starts_with(&root));
        fs::create_dir_all(socket.parent().unwrap()).unwrap();
        fs::set_permissions(socket.parent().unwrap(), fs::Permissions::from_mode(0o700)).unwrap();
        if socket.exists() {
            fs::remove_file(&socket).unwrap();
        }
        let listener = UnixListener::bind(socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        Self {
            worker: Some(thread::spawn(move || {
                let mut server = Server {
                    listener,
                    probes: 0,
                };
                script(&mut server);
                server
            })),
        }
    }
    fn finished(mut self, expected_probes: usize) {
        let server = self.worker.take().unwrap().join().unwrap();
        assert_eq!(
            server.probes, expected_probes,
            "only ordinary User may probe readiness before the final connection"
        );
        assert!(
            matches!(server.listener.accept(), Err(error) if error.kind() == io::ErrorKind::WouldBlock),
            "no fallback connection after refusal or completion"
        );
    }
}
impl Drop for Peer {
    fn drop(&mut self) {
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn expected_authority(role: &str) -> Authority {
    match role {
        "user" | "user-no-start" => Authority::default(),
        "empty" | "non-unicode" => Authority::session(String::new()),
        _ => Authority::session(TOKEN.into()),
    }
}

fn fixture() -> (
    ProjectRemoval,
    WorkspaceModel,
    crate::chat::Session,
    TerminalDescriptor,
) {
    let root = PathBuf::from(std::env::var_os(ROOT).unwrap());
    let repository = root.join("project");
    fs::create_dir_all(&repository).unwrap();
    fs::write(
        repository.join("retained.txt"),
        "The folder is never removed",
    )
    .unwrap();
    let registry = uuid::Uuid::new_v4();
    let removal = ProjectRemoval {
        registry,
        project: ProjectId::new(),
        repository: repository.clone(),
        kind: ProjectKind::Folder,
        name: "Private removal fixture".into(),
        worktrees: vec![repository.clone()],
        channels: 0,
        project_sessions: ProjectSessionOwnership::default(),
    };
    let model = WorkspaceModel {
        identity: registry,
        sequence: 7,
        projects: vec![ModelProject {
            repository: repository.clone(),
            name: removal.name.clone(),
            kind: ProjectKind::Folder,
            ordinal: 1,
        }],
        ..WorkspaceModel::default()
    };
    let CardKind::Terminal(mut session) = crate::app::tests::terminal_card_state().kind else {
        unreachable!()
    };
    session.launch.working_directory = repository;
    let chat = crate::chat::Session {
        id: TerminalId::new(),
        launch: crate::chat::Launch {
            provider: crate::agent::Provider::Codex,
            recipe: session.launch.clone(),
            session_id: Some("fixture-conversation".into()),
        },
        proof: Some(proof()),
        terminal: None,
        owned_codex_listener: None,
        native_continuity: Default::default(),
    };
    let mut terminal = TerminalDescriptor::relaunchable(session.id, session.launch, None);
    terminal.session.continuity = TerminalContinuity::Exited {
        last_exit: TerminalExit {
            code: Some(0),
            signal: None,
            occurred_at_unix_ms: 7,
        },
    };
    (removal, model, chat, terminal)
}

fn successful_inventory(
    connection: &mut Connection,
    chat: crate::chat::Session,
    terminal: TerminalDescriptor,
) {
    connection.chats(vec![chat.clone()]);
    let close = connection.chat_close(&chat);
    connection.response(
        &close,
        ServerResponse::ChatClosed {
            chat_id: chat.id,
            snapshot: Default::default(),
        },
    );
    connection.terminals(vec![terminal.clone()]);
    let close = connection.terminal_close(&terminal);
    connection.response(
        &close,
        ServerResponse::TerminalCloseChecked {
            terminal_id: terminal.session.id,
            outcome: TerminalCloseResult::Closed,
        },
    );
}

fn public_helpers(role: &str) {
    let expected = expected_authority(role);
    for remove in [false, true] {
        let (removal, model, chat, terminal) = fixture();
        let scoped = expected.token().is_some();
        for refuse_auth in if scoped {
            vec![true, false]
        } else {
            vec![false]
        } {
            let authority = expected.clone();
            let before = model.clone();
            let native = chat.clone();
            let completed = terminal.clone();
            let peer = Peer::new(move |server| {
                let Some(mut connection) = server.connect(&authority, refuse_auth) else {
                    return;
                };
                if remove {
                    connection.model(&before);
                }
                if scoped {
                    let list = connection.request();
                    assert!(matches!(list.request, ClientRequest::ChatList));
                    connection.refuse(&list);
                    connection.eof();
                    return;
                }
                successful_inventory(&mut connection, native, completed);
                connection.eof();
                if remove {
                    let mut final_connection = server.connect(&authority, false).unwrap();
                    final_connection.model(&before);
                    let edit = final_connection.request();
                    assert!(
                        matches!(&edit.request, ClientRequest::EditWorkspaceModel { edit: ModelEdit::CloseProject { repository } } if repository == &before.projects[0].repository)
                    );
                    let mut after = before;
                    after.fold(
                        8,
                        &ModelEdit::CloseProject {
                            repository: after.projects[0].repository.clone(),
                        },
                    );
                    final_connection
                        .response(&edit, ServerResponse::WorkspaceModel { model: after });
                    final_connection.eof();
                }
            });
            let result = if remove {
                remove_project_from_cagents(&removal, model.identity).map(|model| {
                    assert!(model.closed_projects.contains(&removal.repository));
                    assert!(model.project(&removal.repository).is_none());
                })
            } else {
                close_sessions_in_scope(&removal.worktrees, None, &removal.project_sessions, None)
                    .map(|closure| assert!(closure.scope.contains(&removal.repository)))
            };
            if scoped {
                assert!(result.unwrap_err().contains(REFUSAL));
            } else {
                result.unwrap();
            }
            assert_eq!(
                fs::read_to_string(removal.repository.join("retained.txt")).unwrap(),
                "The folder is never removed"
            );
            peer.finished(usize::from(remove && role == "user"));
        }
    }
    // Absence cannot stand in for scoped close verification: otherwise a
    // caller could proceed with deleting a directory without authenticating.
    // Both a missing path and a stale socket exercise real connect failures.
    let socket = crate::terminal::ipc::daemon_socket_path().unwrap();
    for refused in [false, true] {
        if socket.exists() {
            fs::remove_file(&socket).unwrap();
        }
        if refused {
            drop(UnixListener::bind(&socket).unwrap());
        }
        let (removal, _, _, _) = fixture();
        let result = close_sessions_in_worktrees(&removal.worktrees);
        if expected.token().is_some() {
            assert!(result.unwrap_err().contains("daemon is unavailable"));
        } else {
            result.unwrap();
        }
        assert_eq!(socket.exists(), refused);
        assert_eq!(
            UnixStream::connect(&socket).unwrap_err().kind(),
            if refused {
                io::ErrorKind::ConnectionRefused
            } else {
                io::ErrorKind::NotFound
            },
            "the helper must leave the unavailable endpoint unavailable",
        );
        assert!(removal.repository.join("retained.txt").is_file());
    }
}

#[test]
fn removal_helpers_authenticate_original_scope_and_preserve_ordinary_user_closure() {
    isolated(
        &["user", "user-no-start", "present", "empty", "non-unicode"],
        public_helpers,
    );
}

fn recover_with_original_scope(role: &str) {
    let (removal, model, chat, terminal) = fixture();
    let mut context = ProjectRemovalContext::capture();
    context.expected_registry = Some(model.identity);
    assert_eq!(context.authority, expected_authority(role));
    // This child inherited only our synthetic capability. Mutate it before
    // any peer thread exists to prove reconnects do not recapture User scope.
    unsafe {
        std::env::remove_var(crate::agent_execution::AUTHORITY_ENV);
    }
    let authority = expected_authority(role);
    let before = model.clone();
    let peer = Peer::new(move |server| {
        let mut initial = server.connect(&authority, false).unwrap();
        initial.model(&before);
        initial.chats(vec![chat.clone()]);
        initial.chat_close(&chat);
        drop(initial); // unknown chat-close outcome must trigger verified recovery
        let mut recovered_chat = server.connect(&authority, false).unwrap();
        recovered_chat.model(&before);
        recovered_chat.chats(vec![]);
        recovered_chat.terminals(vec![terminal.clone()]);
        recovered_chat.terminal_close(&terminal);
        drop(recovered_chat); // completed PTY release needs a second recovery
        let mut recovered_terminal = server.connect(&authority, false).unwrap();
        recovered_terminal.model(&before);
        recovered_terminal.terminals(vec![]);
        recovered_terminal.eof();
        let mut final_connection = server.connect(&authority, false).unwrap();
        final_connection.model(&before);
        let final_edit = final_connection.request();
        assert!(
            matches!(&final_edit.request, ClientRequest::EditWorkspaceModel { edit: ModelEdit::CloseProject { repository } } if repository == &before.projects[0].repository)
        );
        final_connection.refuse(&final_edit);
        final_connection.eof();
    });
    assert!(
        remove_project_from_cagents_with_context(&removal, &context)
            .unwrap_err()
            .contains(REFUSAL)
    );
    peer.finished(0);
}

#[test]
fn captured_removal_scope_survives_chat_terminal_recovery_and_final_write() {
    isolated(&["recovery"], recover_with_original_scope);
}

fn registry_refusal(role: &str) {
    let (removal, model, chat, terminal) = fixture();
    let mut context = ProjectRemovalContext::capture();
    context.expected_registry = Some(model.identity);
    if role == "no-registry" {
        context.expected_registry = Some(crate::workspace_model::NO_REGISTRY);
        let socket = crate::terminal::ipc::daemon_socket_path().unwrap();
        fs::create_dir_all(socket.parent().unwrap()).unwrap();
        let listener = UnixListener::bind(socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        let error = remove_project_from_cagents_with_context(&removal, &context).unwrap_err();
        assert!(error.contains("Wait for the selected device to connect"));
        assert!(
            matches!(listener.accept(), Err(error) if error.kind() == io::ErrorKind::WouldBlock),
            "an unbound registry must refuse before connecting"
        );
        return;
    }
    let authority = expected_authority(role);
    let stage = role.to_owned();
    let mut foreign = model.clone();
    foreign.identity = uuid::Uuid::new_v4();
    assert_ne!(foreign.identity, model.identity);
    let peer = Peer::new(move |server| {
        let mut initial = server.connect(&authority, false).unwrap();
        if stage == "initial" {
            initial.model(&foreign);
            initial.eof(); // no inventory or actor effect under foreign roots
            return;
        }
        initial.model(&model);
        if stage == "chat-recovery" {
            initial.chats(vec![chat.clone()]);
            initial.chat_close(&chat);
            drop(initial);
            let mut recovery = server.connect(&authority, false).unwrap();
            recovery.model(&foreign);
            recovery.eof(); // no stale chat census against a replacement
        } else if stage == "terminal-recovery" {
            initial.chats(vec![]);
            initial.terminals(vec![terminal.clone()]);
            initial.terminal_close(&terminal);
            drop(initial);
            let mut recovery = server.connect(&authority, false).unwrap();
            recovery.model(&foreign);
            recovery.eof(); // no terminal census against a replacement
        } else {
            assert_eq!(stage, "final");
            initial.chats(vec![]);
            initial.terminals(vec![]);
            initial.eof();
            let mut final_connection = server.connect(&authority, false).unwrap();
            final_connection.model(&foreign);
            final_connection.eof(); // no CloseProject after final identity swap
        }
    });
    assert!(
        remove_project_from_cagents_with_context(&removal, &context)
            .unwrap_err()
            .contains("workspace changed")
    );
    assert!(removal.repository.join("retained.txt").is_file());
    peer.finished(0);
}

#[test]
fn removal_refuses_foreign_initial_recovery_and_final_registry_before_later_effects() {
    isolated(
        &[
            "initial",
            "chat-recovery",
            "terminal-recovery",
            "final",
            "no-registry",
        ],
        registry_refusal,
    );
}

fn remote_refusal(_role: &str) {
    let root = PathBuf::from(std::env::var_os(ROOT).unwrap());
    let configuration = crate::device::configuration_path().unwrap();
    fs::create_dir_all(configuration.parent().unwrap()).unwrap();
    fs::write(configuration, r#"{"devices":[{"name":"fixture","host":"removal-fixture.invalid","executable":"cagents"}]}"#).unwrap();
    crate::device::select(Some("fixture")).unwrap();
    let (removal, model, _, _) = fixture();
    for result in [
        close_sessions_in_scope(&removal.worktrees, None, &removal.project_sessions, None)
            .map(|_| ()),
        remove_project_from_cagents(&removal, model.identity).map(|_| ()),
    ] {
        assert!(result.unwrap_err().contains("remote device"));
    }
    assert!(
        !root.join("ssh-entered").exists(),
        "scope refusal must precede the actual bridge resolver"
    );
}

#[test]
fn scoped_remote_removal_refuses_before_the_actual_ssh_resolver() {
    isolated(&["remote"], remote_refusal);
}

fn project_completion(task: Task<Message>) -> Message {
    use cosmic::iced::futures::StreamExt;
    let mut stream = cosmic::iced::runtime::task::into_stream(task).expect("project close task");
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async move {
            while let Some(action) = stream.next().await {
                if let cosmic::iced::runtime::Action::Output(cosmic::Action::App(
                    message @ Message::ProjectUi(
                        Action::RemovedProject { .. } | Action::ProjectSessionsClosed(..),
                    ),
                )) = action
                {
                    return message;
                }
            }
            panic!("project close task produced no owning completion")
        })
}

fn standalone_retained_app(
    label: &str,
) -> (
    Cagents,
    ProjectRemoval,
    WorkspaceModel,
    crate::chat::Session,
    CardId,
) {
    let (removal, model, mut session, _) = fixture();
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let runtime = PathBuf::from(std::env::var_os(ROOT).unwrap()).join(label);
    fs::create_dir(&runtime).unwrap();
    app.worktree_protections.use_fixture_runtime(runtime);
    app.workspace_model = model.clone();
    Arc::make_mut(&mut app.durable).workspace_model_identity = Some(model.identity);
    app.sidebar_model.projects.push(
        SidebarProject::new(
            removal.project,
            RepositoryId::new(),
            removal.name.clone(),
            removal.repository.clone(),
            vec![],
            vec![],
        )
        .with_kind(ProjectKind::Folder),
    );
    app.known_worktree_roots.insert(removal.repository.clone());
    app.active_workspace = Some(removal.repository.clone());
    Arc::make_mut(&mut app.durable).active_channel = None;
    session.launch.provider = crate::agent::Provider::Claude;
    session.launch.session_id = Some(uuid::Uuid::new_v4().to_string());
    session.native_continuity = crate::chat::NativeContinuity::RetainedClaudeActor { protocol: 2 };
    assert!(session.has_valid_native_continuity());
    let card = crate::app::chat_ui::tests::restored_chat_with_draft_fixture(
        &mut app,
        session.clone(),
        "Keep this standalone retained Claude draft",
    );
    assert_eq!(app.durable.active_channel, None);
    assert!(
        app.durable
            .channels
            .iter()
            .all(|channel| !channel.cards.contains(&card))
    );
    assert_eq!(app.workspace.focused(), Some(card));
    let CardKind::Chat(session) = app.card_state(card).unwrap().kind.clone() else {
        panic!("Ready retained the native conversation")
    };
    assert!(session.has_valid_native_continuity());
    (app, removal, model, session, card)
}

fn exact_retained_close(
    connection: &mut Connection,
    session: &crate::chat::Session,
) -> ClientEnvelope {
    let request = connection.request();
    assert!(
        matches!(&request.request,
        ClientRequest::ChatCloseRetained { session: actual } if actual == session),
        "empty inventory must demand the exact retained source close receipt"
    );
    request
}

fn standalone_retained_removal(role: &str) {
    let authority = expected_authority(role);
    for accepted in [false, true] {
        let (mut app, removal, before, session, card) =
            standalone_retained_app(&format!("standalone-{accepted}"));
        let saved = app.card_state(card).unwrap().clone();
        let drafts = app.persistent_workspace().chat_drafts.clone();
        let focus = app.workspace.focused();
        let expected = authority.clone();
        let original = session.clone();
        let mut closed = before.clone();
        closed.fold(
            8,
            &ModelEdit::CloseProject {
                repository: removal.repository.clone(),
            },
        );
        let peer = Peer::new(move |server| {
            let mut initial = server.connect(&expected, false).unwrap();
            initial.model(&before);
            initial.chats(vec![]);
            let close = exact_retained_close(&mut initial, &original);
            if !accepted {
                initial.refuse(&close);
                initial.eof();
                return;
            }
            initial.response(&close, ServerResponse::Accepted);
            initial.terminals(vec![]);
            initial.eof();
            let mut final_connection = server.connect(&expected, false).unwrap();
            final_connection.model(&before);
            let edit = final_connection.request();
            assert!(matches!(&edit.request,
                ClientRequest::EditWorkspaceModel { edit: ModelEdit::CloseProject { repository } }
                    if repository == &before.projects[0].repository));
            final_connection.response(
                &edit,
                ServerResponse::WorkspaceModel {
                    model: closed.clone(),
                },
            );
            final_connection.eof();
            // Canonical close has its own local completion gate. Reconfirm its
            // exact saved owner, as a real Host may answer from its close receipt.
            let mut local = server.connect(&expected, false).unwrap();
            local.model(&closed);
            local.chats(vec![]);
            let close = exact_retained_close(&mut local, &original);
            local.response(&close, ServerResponse::Accepted);
            local.terminals(vec![]);
            local.eof();
        });
        drop(app.handle_project_action(Action::Command {
            project_id: removal.project,
            worktree_id: None,
            command: ProjectCommand::Remove,
        }));
        assert!(app.project_ui.remove_dialog.is_some());
        let completion = project_completion(app.handle_project_action(Action::ConfirmRemove));
        assert!(matches!(&completion,
            Message::ProjectUi(Action::RemovedProject { result, .. }) if result.is_ok() == accepted));
        let local = app.update(completion);
        if accepted {
            let completion = project_completion(local);
            assert!(matches!(
                &completion,
                Message::ProjectUi(Action::ProjectSessionsClosed(_, _, Ok(_)))
            ));
            drop(app.update(completion));
            assert!(app.sidebar_model.project(removal.project).is_none());
            assert!(!app.card_state(card).unwrap().existence.is_open());
        } else {
            drop(local);
            assert!(app.sidebar_model.project(removal.project).is_some());
            assert_eq!(app.card_state(card), Some(&saved));
            assert_eq!(app.persistent_workspace().chat_drafts, drafts);
            assert_eq!(app.workspace.focused(), focus);
            assert!(app.workspace_model.project(&removal.repository).is_some());
            assert!(
                !app.workspace_model
                    .closed_projects
                    .contains(&removal.repository)
            );
            assert!(app.project_ui.removal_attempts.is_empty());
            assert!(app.project_ui.removing.is_empty());
            assert!(
                app.notices
                    .entries
                    .iter()
                    .any(|entry| entry.notice.message.contains(REFUSAL))
            );
        }
        peer.finished(usize::from(accepted && role == "user"));
    }
}

#[test]
fn standalone_retained_project_removal_requires_a_receipt_and_keeps_refused_work() {
    isolated(
        &["user", "user-no-start", "present"],
        standalone_retained_removal,
    );
}

fn retained_project_recovery(role: &str) {
    let authority = expected_authority(role);
    for accepted in [false, true] {
        let (app, removal, model, session, _) =
            standalone_retained_app(&format!("recovery-{accepted}"));
        let ownership = app.project_session_ownership(removal.project);
        let mut context = ProjectRemovalContext::capture();
        context.expected_registry = Some(model.identity);
        let expected = authority.clone();
        let source = session.clone();
        let peer = Peer::new(move |server| {
            let mut initial = server.connect(&expected, false).unwrap();
            initial.model(&model);
            initial.chats(vec![source.clone()]);
            let close = initial.chat_close(&source);
            initial.refuse(&close);
            let detach = initial.request();
            assert!(
                matches!(detach.request, ClientRequest::ChatDetach { chat_id } if chat_id == source.id)
            );
            initial.response(&detach, ServerResponse::Detached);
            initial.eof();
            let mut recovery = server.connect(&expected, false).unwrap();
            recovery.model(&model);
            recovery.chats(vec![]);
            let close = exact_retained_close(&mut recovery, &source);
            if accepted {
                recovery.response(&close, ServerResponse::Accepted);
                recovery.terminals(vec![]);
            } else {
                recovery.refuse(&close);
            }
            recovery.eof();
        });
        let result = close_sessions_in_scope_with_context(
            &removal.worktrees,
            Some(&removal.repository),
            &ownership,
            None,
            &context,
        );
        assert_eq!(result.is_ok(), accepted);
        peer.finished(0);
    }
}

#[test]
fn retained_project_recovery_requires_exact_teardown_after_actor_inventory_disappears() {
    isolated(
        &["user-no-start", "present", "empty"],
        retained_project_recovery,
    );
}

fn retained_project_neighbors(role: &str) {
    for same_id in [false, true] {
        let (mut app, removal, model, source, original) =
            standalone_retained_app(&format!("neighbors-{same_id}"));
        let foreign = ProjectId::new();
        let mut neighbor = source.clone();
        if !same_id {
            neighbor.id = TerminalId::new();
        }
        neighbor.launch.session_id = Some(uuid::Uuid::new_v4().to_string());
        let card = crate::app::chat_ui::tests::restored_chat_with_draft_fixture(
            &mut app,
            neighbor.clone(),
            "Keep the neighboring project's draft",
        );
        Arc::make_mut(&mut app.durable)
            .channels
            .push(crate::model::ProjectChannel {
                id: ChannelId::new(),
                project_id: foreign,
                title: "Neighbor".into(),
                title_source: Default::default(),
                ordinal: 1,
                shared: true,
                cards: vec![card],
                focused: Some(card),
            });
        let ownership = app.project_session_ownership(removal.project);
        let mut context = ProjectRemovalContext::capture();
        context.expected_registry = Some(model.identity);
        let expected = expected_authority(role);
        let peer = Peer::new(move |server| {
            let mut connection = server.connect(&expected, false).unwrap();
            connection.model(&model);
            connection.chats(vec![neighbor]);
            if !same_id {
                let close = exact_retained_close(&mut connection, &source);
                connection.response(&close, ServerResponse::Accepted);
            }
            connection.terminals(vec![]);
            connection.eof();
        });
        close_sessions_in_scope_with_context(
            &removal.worktrees,
            Some(&removal.repository),
            &ownership,
            None,
            &context,
        )
        .unwrap();
        peer.finished(0);
        assert!(app.card_state(original).unwrap().existence.is_open());
        assert!(app.card_state(card).unwrap().existence.is_open());
    }
}

#[test]
fn retained_project_census_preserves_foreign_views_even_with_the_same_directory_or_session_id() {
    isolated(&["user-no-start", "present"], retained_project_neighbors);
}

fn retained_project_successor(role: &str) {
    let (mut app, removal, mut model, source, card) = standalone_retained_app("successor");
    let drafts = app.persistent_workspace().chat_drafts.clone();
    let focused = app.workspace.focused();
    let mut replacement = source.clone();
    replacement.launch.session_id = Some(uuid::Uuid::new_v4().to_string());
    model.fold(
        8,
        &ModelEdit::CloseProject {
            repository: removal.repository.clone(),
        },
    );
    let expected = expected_authority(role);
    let peer = Peer::new(move |server| {
        let mut connection = server.connect(&expected, false).unwrap();
        connection.model(&model);
        connection.chats(vec![]);
        let close = exact_retained_close(&mut connection, &source);
        connection.response(&close, ServerResponse::Accepted);
        connection.terminals(vec![]);
        connection.eof();
    });
    let task =
        app.apply_workspace_model_update(super::super::workspace_model_sync::ModelUpdate::Edited {
            sequence: 8,
            edit: ModelEdit::CloseProject {
                repository: removal.repository.clone(),
            },
        });
    let completion = project_completion(task);
    assert!(matches!(
        &completion,
        Message::ProjectUi(Action::ProjectSessionsClosed(_, _, Ok(_)))
    ));
    peer.finished(0);
    // A completed old close is not a receipt for a new conversation which
    // acquired the same allocation while its UI callback waited.
    app.install_chat_session(card, replacement.clone());
    let saved = app.card_state(card).unwrap().clone();
    drop(app.update(completion));
    assert!(app.sidebar_model.project(removal.project).is_some());
    assert_eq!(app.card_state(card), Some(&saved));
    assert_eq!(
        app.card_state(card).unwrap().kind,
        CardKind::Chat(replacement)
    );
    assert_eq!(app.persistent_workspace().chat_drafts, drafts);
    assert_eq!(app.workspace.focused(), focused);
    assert!(app.project_ui.close_failed.contains(&removal.project));
}

#[test]
fn completed_project_close_cannot_retire_a_replacement_retained_conversation() {
    isolated(&["user-no-start", "present"], retained_project_successor);
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
        let private = tempfile::Builder::new()
            .prefix("rm-scope-")
            .tempdir()
            .unwrap();
        let root = private.path().canonicalize().unwrap();
        for leaf in ["home", "runtime", "config", "data", "cache", "state", "bin"] {
            let path = root.join(leaf);
            fs::create_dir(&path).unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
        }
        let ssh = root.join("bin/ssh");
        fs::write(
            &ssh,
            "#!/bin/sh\n: > \"$CAGENTS_TEST_REMOVAL_PRIVATE_ROOT/ssh-entered\"\nexit 73\n",
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
        if *role == "user-no-start" {
            command.env("CAGENTS_NO_START", "YeS");
        }
        if !matches!(*role, "user" | "user-no-start") {
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
                panic!("private removal child exceeded its bound: {role}");
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
    }
}
