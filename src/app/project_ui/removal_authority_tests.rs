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
    let removal = ProjectRemoval {
        project: ProjectId::new(),
        repository: repository.clone(),
        kind: ProjectKind::Folder,
        name: "Private removal fixture".into(),
        worktrees: vec![repository.clone()],
        channels: 0,
        project_sessions: ProjectSessionOwnership::default(),
    };
    let model = WorkspaceModel {
        identity: uuid::Uuid::new_v4(),
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
