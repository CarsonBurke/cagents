//! Actual public parsing and legacy dispatch over a private framed Unix peer.
//! No Host policy, staged executable, provider or remote transport is simulated
//! as a successful production integration; these tests pin the caller boundary.
use super::*;
use crate::{
    model::{BootId, CardId, CardKind, HostIncarnation, HostSessionId, ProjectKind},
    terminal::ipc::ProtocolError,
    workspace_model::views::{Change, Pane, Scope, View},
};
use std::{
    cell::{Cell, RefCell},
    os::unix::net::UnixListener,
    thread::JoinHandle,
};

fn proof() -> HostSessionProof {
    HostSessionProof {
        host_session_id: HostSessionId::new("project-channel-fixture"),
        boot_id: BootId::new("fixture-boot"),
        host_incarnation: HostIncarnation::new("fixture-incarnation"),
    }
}

fn reply(peer: &mut UnixStream, request: &ClientEnvelope, message: ServerMessage) {
    write_frame(
        peer,
        &ServerEnvelope {
            version: PROTOCOL_VERSION,
            request_id: Some(request.request_id),
            proof: proof(),
            message,
        },
    )
    .unwrap();
}

fn connected() -> ServerMessage {
    ServerMessage::Response {
        response: ServerResponse::Connected {
            proof: proof(),
            resumed: vec![],
        },
    }
}

fn denied() -> ServerMessage {
    ServerMessage::Error(ProtocolError::new(
        ProtocolErrorCode::PermissionDenied,
        "The private fixture refused this mutation",
        false,
    ))
}

fn model_reply(peer: &mut UnixStream, request: &ClientEnvelope, model: WorkspaceModel) {
    reply(
        peer,
        request,
        ServerMessage::Response {
            response: ServerResponse::WorkspaceModel { model },
        },
    );
}

fn next(peer: &mut UnixStream) -> ClientEnvelope {
    let request: ClientEnvelope = read_frame(peer).unwrap().unwrap();
    assert_eq!(
        request.proof,
        Some(proof()),
        "every request must retain the actual handshake proof"
    );
    request
}

fn auth(peer: &mut UnixStream, authority: &Authority, accept: bool) {
    let request = next(peer);
    assert!(
        matches!(&request.request, ClientRequest::Authenticate { authority: actual } if actual == authority)
    );
    reply(
        peer,
        &request,
        if accept {
            ServerMessage::Response {
                response: ServerResponse::Accepted,
            }
        } else {
            denied()
        },
    );
}

fn read_model(peer: &mut UnixStream, model: &WorkspaceModel) {
    let request = next(peer);
    assert!(matches!(request.request, ClientRequest::ReadWorkspaceModel));
    model_reply(peer, &request, model.clone());
}

/// Every accept/read/write is bounded. Drop joins even on an assertion failure;
/// no detached thread, helper process, production socket, or ambient token.
struct Peer {
    _directory: tempfile::TempDir,
    socket: PathBuf,
    worker: Option<JoinHandle<UnixListener>>,
}
impl Peer {
    fn new(handshake: ServerMessage, serve: impl FnOnce(&mut UnixStream) + Send + 'static) -> Self {
        let directory = tempfile::Builder::new().prefix("pc-").tempdir().unwrap();
        let socket = directory.path().join("control.sock");
        Self::bound(directory, socket, handshake, serve)
    }
    fn bound(
        directory: tempfile::TempDir,
        socket: PathBuf,
        handshake: ServerMessage,
        serve: impl FnOnce(&mut UnixStream) + Send + 'static,
    ) -> Self {
        let listener = UnixListener::bind(&socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        let worker = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(5);
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline, "private peer was not connected");
                        thread::sleep(Duration::from_millis(2));
                    }
                    Err(error) => panic!("private listener: {error}"),
                }
            };
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            stream
                .set_write_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let hello: ClientEnvelope = read_frame(&mut stream).unwrap().unwrap();
            assert!(matches!(hello.request, ClientRequest::Connect { .. }));
            assert!(hello.proof.is_none());
            reply(&mut stream, &hello, handshake);
            serve(&mut stream);
            assert!(
                read_frame::<ClientEnvelope>(&mut stream).unwrap().is_none(),
                "no extra frame, User fallback, or mutation after refusal"
            );
            listener
        });
        Self {
            _directory: directory,
            socket,
            worker: Some(worker),
        }
    }
    fn finished(mut self) {
        let listener = self.worker.take().unwrap().join().unwrap();
        assert!(
            matches!(listener.accept(), Err(error) if error.kind() == io::ErrorKind::WouldBlock),
            "legacy command must not open a retry/fallback connection"
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

struct Fixture {
    _directory: tempfile::TempDir,
    repository: PathBuf,
    model: WorkspaceModel,
    source: ChannelId,
    destination: ChannelId,
    pane: CardId,
}
impl Fixture {
    fn new() -> Self {
        let directory = tempfile::Builder::new()
            .prefix("pc-folder with spaces-")
            .tempdir()
            .unwrap();
        let repository = directory.path().canonicalize().unwrap();
        let source = ChannelId::new();
        let destination = ChannelId::new();
        let pane = CardId::new();
        let terminal = TerminalSession {
            id: TerminalId::new(),
            owned_codex_listener: None,
            recovery_origin: None,
            predecessor: None,
            launch: LaunchRecipe {
                program: "/bin/sh".into(),
                arguments: vec![],
                working_directory: repository.clone(),
                environment: Default::default(),
                unset_environment: Default::default(),
            },
            transcript_path: None,
            resume_hint: None,
            continuity: TerminalContinuity::Live {
                host_session_id: proof().host_session_id,
                boot_id: proof().boot_id,
                host_incarnation: proof().host_incarnation,
            },
        };
        let model = WorkspaceModel {
            identity: uuid::Uuid::new_v4(),
            sequence: 17,
            projects: vec![ModelProject {
                repository: repository.clone(),
                name: "private folder".into(),
                kind: ProjectKind::Folder,
                ordinal: 2,
            }],
            channels: [source, destination]
                .into_iter()
                .enumerate()
                .map(|(ordinal, id)| ModelChannel {
                    id,
                    repository: repository.clone(),
                    title: format!("channel {ordinal}"),
                    title_source: Default::default(),
                    ordinal: ordinal as u32,
                })
                .collect(),
            views: vec![
                View {
                    scope: Scope::Channel(source),
                    revision: 7,
                    panes: vec![Pane {
                        id: pane,
                        title: "inert descriptor".into(),
                        user_title: None,
                        pending_terminal_start: false,
                        width_fraction: 0.6,
                        kind: CardKind::Terminal(terminal),
                        scope: Some(repository.clone()),
                    }],
                    retired: vec![],
                },
                View {
                    scope: Scope::Channel(destination),
                    revision: 3,
                    panes: vec![],
                    retired: vec![],
                },
            ],
            ..Default::default()
        };
        Self {
            _directory: directory,
            repository,
            model,
            source,
            destination,
            pane,
        }
    }
    fn argv(&self, verb: &str) -> Vec<String> {
        let path = self.repository.to_str().unwrap().to_owned();
        match verb {
            "open" => vec!["project".into(), "open".into(), path],
            "project-close" => vec!["project".into(), "close".into(), path, "--yes".into()],
            "spawn" => vec![
                "channel".into(),
                "spawn".into(),
                "--repository".into(),
                path,
                "--title".into(),
                "original title".into(),
            ],
            "channel-close" => vec!["channel".into(), "close".into(), self.source.to_string()],
            "move" => vec![
                "channel".into(),
                "move".into(),
                self.pane.to_string(),
                "--to".into(),
                self.destination.to_string(),
            ],
            _ => panic!("unknown fixture verb"),
        }
    }
}

fn parse(args: &[String]) -> Cli {
    Cli::try_parse_from(std::iter::once("cagents".to_owned()).chain(args.iter().cloned())).unwrap()
}

fn dispatch(
    cli: Cli,
    connect: impl FnOnce(bool) -> Result<ControlClient, AppError>,
) -> Result<CommandOutput, AppError> {
    match cli.command {
        TopCommand::Project { command } => execute_project_with(command, connect),
        TopCommand::Channel { command } => execute_channel_with(command, connect),
        _ => panic!("not a legacy project/channel command"),
    }
}

fn selected(
    context: &ProjectChannelContext,
    mutation: bool,
    socket: &Path,
) -> Result<ControlClient, AppError> {
    context.connect_with(
        mutation,
        || Ok(socket.to_owned()),
        || panic!("no readiness for a present token or --no-start"),
        ControlClient::connect,
    )
}

fn assert_refused(error: AppError, authority: &Authority) {
    assert_eq!(error.kind, "protocol");
    assert_eq!(
        error.details.unwrap(),
        json!({"code": ProtocolErrorCode::PermissionDenied, "retryable": false})
    );
    if let Some(token) = authority.token().filter(|token| !token.is_empty()) {
        assert!(!error.message.contains(token));
    }
}

fn first_edit(peer: &mut UnixStream, verb: &str, model: &WorkspaceModel) -> ClientEnvelope {
    match verb {
        "open" | "spawn" => read_model(peer, model),
        "project-close" => {
            read_model(peer, model);
            read_model(peer, model);
        }
        "move" => read_model(peer, model),
        "channel-close" => {}
        _ => panic!("unknown verb"),
    }
    let request = next(peer);
    assert!(matches!(
        request.request,
        ClientRequest::EditWorkspaceModel { .. }
    ));
    request
}

#[test]
fn public_user_open_and_spawn_preserve_folder_identity_order_and_readiness_policy() {
    for verb in ["open", "spawn"] {
        for no_start in [false, true] {
            for remote in [false, true] {
                let fixture = Fixture::new();
                let initial = fixture.model.clone();
                let repository = fixture.repository.clone();
                let peer = Peer::new(connected(), move |peer| {
                    read_model(peer, &initial);
                    let request = next(peer);
                    let expected = if verb == "open" {
                        ModelEdit::ReopenProject {
                            repository: repository.clone(),
                            kind: ProjectKind::Folder,
                            name: repository_display_name(&repository),
                        }
                    } else {
                        ModelEdit::OpenProject {
                            repository: repository.clone(),
                            kind: ProjectKind::Folder,
                            name: repository_display_name(&repository),
                        }
                    };
                    assert_eq!(
                        request.request,
                        ClientRequest::EditWorkspaceModel { edit: expected }
                    );
                    model_reply(peer, &request, initial.clone());
                    if verb == "spawn" {
                        let created = next(peer);
                        let ClientRequest::EditWorkspaceModel {
                            edit:
                                ModelEdit::CreateChannel {
                                    id,
                                    repository: actual,
                                    title,
                                    title_source,
                                },
                        } = &created.request
                        else {
                            panic!("expected minted channel creation");
                        };
                        assert_eq!(actual, &repository);
                        assert_eq!(title, "original title");
                        assert_eq!(*title_source, Default::default());
                        let mut result = initial;
                        result.channels.push(ModelChannel {
                            id: *id,
                            repository,
                            title: title.clone(),
                            title_source: *title_source,
                            ordinal: 9,
                        });
                        model_reply(peer, &created, result);
                    }
                });
                let mut args = fixture.argv(verb);
                if no_start {
                    args.insert(0, "--no-start".into());
                }
                let cli = parse(&args);
                let context = ProjectChannelContext {
                    authority: Authority::default(),
                    remote,
                    no_start: cli.no_start,
                };
                let existing = Cell::new(0);
                let ready = Cell::new(0);
                let output = dispatch(cli, |mutation| {
                    assert!(mutation);
                    context.connect_with(
                        mutation,
                        || {
                            existing.set(existing.get() + 1);
                            Ok(peer.socket.clone())
                        },
                        || {
                            ready.set(ready.get() + 1);
                            Ok(peer.socket.clone())
                        },
                        ControlClient::connect,
                    )
                })
                .unwrap();
                assert_eq!(
                    output.command,
                    if verb == "open" {
                        "project.open"
                    } else {
                        "channel.spawn"
                    }
                );
                assert_eq!(existing.get(), usize::from(no_start));
                assert_eq!(ready.get(), usize::from(!no_start));
                let row = if verb == "open" {
                    &output.data["project"]
                } else {
                    &output.data["channel"]
                };
                assert_eq!(row["repository"], json!(fixture.repository));
                peer.finished();
            }
        }
    }
}

#[test]
fn public_user_close_and_move_preserve_exact_destructive_intent_and_receipts() {
    for verb in ["project-close", "channel-close", "move"] {
        let fixture = Fixture::new();
        let model = fixture.model.clone();
        let repository = fixture.repository.clone();
        let source = fixture.source;
        let destination = fixture.destination;
        let pane = fixture.pane;
        let peer = Peer::new(connected(), move |peer| {
            let request = first_edit(peer, verb, &model);
            match &request.request {
                ClientRequest::EditWorkspaceModel {
                    edit: ModelEdit::CloseProject { repository: actual },
                } if verb == "project-close" => assert_eq!(actual, &repository),
                ClientRequest::EditWorkspaceModel {
                    edit: ModelEdit::DeleteChannel { id },
                } if verb == "channel-close" => assert_eq!(*id, source),
                ClientRequest::EditWorkspaceModel {
                    edit:
                        ModelEdit::ChangeView {
                            scope,
                            expected_revision,
                            operation,
                            change,
                        },
                } if verb == "move" => {
                    assert_eq!(scope, &Scope::Channel(source));
                    assert_eq!(*expected_revision, 7);
                    assert!(!operation.is_nil());
                    assert_eq!(
                        change,
                        &Change::Transfer {
                            id: pane,
                            destination: Scope::Channel(destination),
                            before: None
                        }
                    );
                }
                _ => panic!("wrong destructive edit: {:?}", request.request),
            }
            let mut result = model;
            if verb == "move" {
                let moved = result.views[0].panes.remove(0);
                result.views[1].panes.push(moved);
            }
            model_reply(peer, &request, result);
        });
        let context = ProjectChannelContext {
            authority: Authority::default(),
            remote: false,
            no_start: true,
        };
        let output = dispatch(parse(&fixture.argv(verb)), |mutation| {
            selected(&context, mutation, &peer.socket)
        })
        .unwrap();
        assert_eq!(
            output.command,
            match verb {
                "project-close" => "project.close",
                "channel-close" => "channel.close",
                _ => "channel.move",
            }
        );
        if verb == "move" {
            assert_eq!(output.data["placement"], json!(Scope::Channel(destination)));
        } else {
            assert_eq!(output.data["closed"], true);
        }
        peer.finished();
    }
}

#[test]
fn every_scoped_legacy_mutation_authenticates_captured_authority_before_reads_and_edits() {
    for verb in ["open", "project-close", "spawn", "channel-close", "move"] {
        let fixture = Fixture::new();
        let model = fixture.model.clone();
        let authority = Authority::session("fixture-original-cli-scope".into());
        let expected = authority.clone();
        let peer = Peer::new(connected(), move |peer| {
            auth(peer, &expected, true);
            let request = first_edit(peer, verb, &model);
            reply(peer, &request, denied());
        });
        let later_sample = RefCell::new(authority.clone());
        let context = ProjectChannelContext {
            authority: later_sample.borrow().clone(),
            remote: false,
            no_start: false,
        };
        let resolves = Cell::new(0);
        let error = dispatch(parse(&fixture.argv(verb)), |mutation| {
            assert!(mutation);
            context.connect_with(
                mutation,
                || {
                    resolves.set(resolves.get() + 1);
                    *later_sample.borrow_mut() = Authority::default();
                    Ok(peer.socket.clone())
                },
                || panic!("scoped caller must not start a daemon"),
                ControlClient::connect,
            )
        })
        .err()
        .expect("the synthetic scoped edit is refused");
        assert_refused(error, &authority);
        assert_eq!(resolves.get(), 1);
        assert!(later_sample.borrow().token().is_none());
        peer.finished();
    }
}

#[test]
fn invalid_and_empty_inherited_tokens_cannot_read_or_edit_after_authentication_refusal() {
    for authority in [
        Authority::session("fixture-invalid-cli-scope".into()),
        Authority::session(String::new()),
    ] {
        for verb in ["open", "project-close", "spawn", "channel-close", "move"] {
            let fixture = Fixture::new();
            let expected = authority.clone();
            let peer = Peer::new(connected(), move |peer| auth(peer, &expected, false));
            let context = ProjectChannelContext {
                authority: authority.clone(),
                remote: false,
                no_start: false,
            };
            let error = dispatch(parse(&fixture.argv(verb)), |mutation| {
                selected(&context, mutation, &peer.socket)
            })
            .err()
            .unwrap();
            assert_refused(error, &authority);
            peer.finished();
        }
    }
}

#[test]
fn public_lists_remain_global_unauthenticated_reads_with_explicit_folder_filter_only() {
    for authority in [
        Authority::default(),
        Authority::session("fixture-invalid-read-scope".into()),
        Authority::session(String::new()),
    ] {
        for mode in ["projects", "channels", "filtered"] {
            let fixture = Fixture::new();
            let other = PathBuf::from("/inert-unrelated-project");
            let mut model = fixture.model.clone();
            model.projects.push(ModelProject {
                repository: other.clone(),
                name: "other".into(),
                kind: ProjectKind::Folder,
                ordinal: 99,
            });
            model.channels.push(ModelChannel {
                id: ChannelId::new(),
                repository: other.clone(),
                title: "other channel".into(),
                title_source: Default::default(),
                ordinal: 99,
            });
            let peer = Peer::new(connected(), move |peer| {
                if mode == "filtered" {
                    read_model(peer, &model);
                }
                read_model(peer, &model);
            });
            let args = match mode {
                "projects" => vec!["project".into(), "list".into()],
                "channels" => vec!["channel".into(), "list".into()],
                _ => vec![
                    "channel".into(),
                    "list".into(),
                    "--repository".into(),
                    fixture.repository.to_str().unwrap().into(),
                ],
            };
            let context = ProjectChannelContext {
                authority: authority.clone(),
                remote: false,
                no_start: authority.token().is_none(),
            };
            let output = dispatch(parse(&args), |mutation| {
                assert!(!mutation);
                selected(&context, mutation, &peer.socket)
            })
            .unwrap();
            let rows = output.data[if mode == "projects" {
                "projects"
            } else {
                "channels"
            }]
            .as_array()
            .unwrap();
            assert_eq!(
                rows.len(),
                if mode == "projects" {
                    2
                } else if mode == "channels" {
                    3
                } else {
                    2
                }
            );
            assert_eq!(
                rows[0]["repository"],
                json!(if mode == "filtered" {
                    &fixture.repository
                } else {
                    &other
                })
            );
            peer.finished();
        }
    }
}

#[test]
fn scoped_remote_refusal_precedes_resolvers_connect_and_public_path_discovery() {
    for authority in [
        Authority::session("fixture-remote-cli-scope".into()),
        Authority::session(String::new()),
    ] {
        for no_start in [false, true] {
            let context = ProjectChannelContext {
                authority: authority.clone(),
                remote: true,
                no_start,
            };
            for mutation in [false, true] {
                let error = context
                    .connect_with(
                        mutation,
                        || panic!("selected socket/SSH must not resolve"),
                        || panic!("readiness must not run"),
                        |_| panic!("must not connect"),
                    )
                    .err()
                    .unwrap();
                assert_eq!(error.kind, "permission_denied");
            }
            // The actual *_as wrappers must refuse before even validating this
            // definitely absent private path, rather than reaching discovery.
            let private = tempfile::tempdir().unwrap();
            let missing = private.path().join("missing");
            for args in [
                vec![
                    "project".into(),
                    "open".into(),
                    missing.to_str().unwrap().into(),
                ],
                vec![
                    "channel".into(),
                    "spawn".into(),
                    "--repository".into(),
                    missing.to_str().unwrap().into(),
                ],
            ] {
                let error = match parse(&args).command {
                    TopCommand::Project { command } => execute_project_as(command, &context),
                    TopCommand::Channel { command } => execute_channel_as(command, &context),
                    _ => unreachable!(),
                }
                .err()
                .unwrap();
                assert_eq!(error.kind, "permission_denied");
            }
        }
    }
}

#[test]
fn public_path_validation_and_project_confirmation_remain_effective() {
    let fixture = Fixture::new();
    let missing = fixture.repository.join("absent");
    for args in [
        vec![
            "project".into(),
            "open".into(),
            missing.to_str().unwrap().into(),
        ],
        vec![
            "project".into(),
            "close".into(),
            missing.to_str().unwrap().into(),
            "--yes".into(),
        ],
        vec![
            "channel".into(),
            "spawn".into(),
            "--repository".into(),
            missing.to_str().unwrap().into(),
        ],
        vec![
            "channel".into(),
            "list".into(),
            "--repository".into(),
            missing.to_str().unwrap().into(),
        ],
    ] {
        let error = dispatch(parse(&args), |_| {
            panic!("invalid path must fail before readiness/connect")
        })
        .err()
        .unwrap();
        assert_eq!(error.kind, "repository");
    }
    for has_channels in [false, true] {
        let mut model = fixture.model.clone();
        if !has_channels {
            model.channels.clear();
        }
        let expected_channels = model.channels.clone();
        let repository = fixture.repository.clone();
        let peer = Peer::new(connected(), move |peer| {
            read_model(peer, &model);
            read_model(peer, &model);
            if !has_channels {
                let request = next(peer);
                assert_eq!(
                    request.request,
                    ClientRequest::EditWorkspaceModel {
                        edit: ModelEdit::CloseProject { repository }
                    }
                );
                model_reply(peer, &request, WorkspaceModel::default());
            }
        });
        let context = ProjectChannelContext {
            authority: Authority::default(),
            remote: false,
            no_start: true,
        };
        let args = vec![
            "project".into(),
            "close".into(),
            fixture.repository.to_str().unwrap().into(),
        ];
        let result = dispatch(parse(&args), |mutation| {
            selected(&context, mutation, &peer.socket)
        });
        if has_channels {
            let error = result.err().unwrap();
            assert_eq!(error.kind, "confirmation");
            let mut sorted = expected_channels;
            sorted.sort_by_key(|channel| std::cmp::Reverse(channel.ordinal));
            assert_eq!(error.details.unwrap(), json!({"channels": sorted}));
            assert!(error.message.contains("--yes"));
        } else {
            assert_eq!(result.unwrap().data["closed"], true);
        }
        peer.finished();
    }
}

#[test]
fn handshake_refusal_and_mismatched_proof_never_authenticate_or_fall_back() {
    let mut wrong = proof();
    wrong.boot_id = BootId::new("different-boot");
    for handshake in [
        denied(),
        ServerMessage::Response {
            response: ServerResponse::Connected {
                proof: wrong,
                resumed: vec![],
            },
        },
    ] {
        let peer = Peer::new(handshake, |_| {});
        let context = ProjectChannelContext {
            authority: Authority::session("fixture-handshake-scope".into()),
            remote: false,
            no_start: false,
        };
        let attempts = Cell::new(0);
        let error = context
            .connect_with(
                true,
                || Ok(peer.socket.clone()),
                || panic!("no startup"),
                |socket| {
                    attempts.set(attempts.get() + 1);
                    ControlClient::connect(socket)
                },
            )
            .err()
            .unwrap();
        assert_eq!(error.kind, "daemon_connection");
        assert_eq!(attempts.get(), 1);
        assert!(!error.message.contains("fixture-handshake-scope"));
        peer.finished();
    }
}

#[test]
fn selected_readiness_and_connection_failures_keep_original_error_and_never_retry() {
    for scoped in [false, true] {
        for no_start in [false, true] {
            let context = ProjectChannelContext {
                authority: if scoped {
                    Authority::session(String::new())
                } else {
                    Authority::default()
                },
                remote: false,
                no_start,
            };
            let existing = Cell::new(0);
            let ready = Cell::new(0);
            let error = context
                .connect_with(
                    true,
                    || {
                        existing.set(existing.get() + 1);
                        Err(AppError::new("selected_fixture", "original selected error"))
                    },
                    || {
                        ready.set(ready.get() + 1);
                        Err(AppError::new("ready_fixture", "original ready error"))
                    },
                    |_| panic!("resolver refusal must not connect"),
                )
                .err()
                .unwrap();
            assert_eq!(
                error.kind,
                if scoped || no_start {
                    "selected_fixture"
                } else {
                    "ready_fixture"
                }
            );
            assert_eq!(existing.get(), usize::from(scoped || no_start));
            assert_eq!(ready.get(), usize::from(!scoped && !no_start));
        }
    }
    let directory = tempfile::tempdir().unwrap();
    let missing = directory.path().join("absent.sock");
    let context = ProjectChannelContext {
        authority: Authority::session("fixture-no-fallback".into()),
        remote: false,
        no_start: false,
    };
    let attempts = Cell::new(0);
    let error = context
        .connect_with(
            true,
            || Ok(missing.clone()),
            || panic!("no fallback startup"),
            |socket| {
                attempts.set(attempts.get() + 1);
                assert_eq!(socket, missing);
                ControlClient::connect(socket)
            },
        )
        .err()
        .unwrap();
    assert_eq!(error.kind, "daemon_connection");
    assert_eq!(attempts.get(), 1);
}

#[test]
fn public_move_refuses_cross_project_before_editing() {
    let fixture = Fixture::new();
    let mut model = fixture.model.clone();
    model
        .channels
        .iter_mut()
        .find(|channel| channel.id == fixture.destination)
        .unwrap()
        .repository = "/inert-other-project".into();
    let peer = Peer::new(connected(), move |peer| read_model(peer, &model));
    let context = ProjectChannelContext {
        authority: Authority::default(),
        remote: false,
        no_start: true,
    };
    let error = dispatch(parse(&fixture.argv("move")), |mutation| {
        selected(&context, mutation, &peer.socket)
    })
    .err()
    .unwrap();
    assert_eq!(error.kind, "invalid_target");
    assert_eq!(
        error.message,
        "Panes can only move between channels of the same project"
    );
    peer.finished();
}

#[test]
fn public_classification_and_staged_routing_preserve_reads_users_and_channel_work() {
    let fixture = Fixture::new();
    let scoped = ProjectChannelContext {
        authority: Authority::session("fixture-staged-scope".into()),
        remote: false,
        no_start: false,
    };
    let empty = ProjectChannelContext {
        authority: Authority::session(String::new()),
        remote: false,
        no_start: false,
    };
    let user = ProjectChannelContext {
        authority: Authority::default(),
        remote: false,
        no_start: false,
    };
    for verb in ["open", "project-close", "spawn", "channel-close", "move"] {
        let cli = parse(&fixture.argv(verb));
        assert!(legacy_model_command_mutates(&cli.command));
        assert!(!routes_to_staged_owner_as(&cli.command, Some(&scoped)));
        assert!(!routes_to_staged_owner_as(&cli.command, Some(&empty)));
        assert_eq!(
            routes_to_staged_owner_as(&cli.command, Some(&user)),
            routes_to_staged_owner(&cli.command)
        );
    }
    for args in [
        vec!["project".into(), "list".into()],
        vec!["channel".into(), "list".into()],
        vec![
            "channel".into(),
            "work".into(),
            "context".into(),
            "--channel".into(),
            fixture.source.to_string(),
        ],
        vec![
            "channel".into(),
            "work".into(),
            "show".into(),
            "--limit".into(),
            "2".into(),
        ],
    ] {
        let cli = parse(&args);
        assert!(!legacy_model_command_mutates(&cli.command));
        assert_eq!(
            routes_to_staged_owner_as(&cli.command, Some(&scoped)),
            routes_to_staged_owner(&cli.command)
        );
        match cli.command {
            TopCommand::Project { command } => assert!(!project_command_mutates(&command)),
            TopCommand::Channel {
                command: ChannelCommand::Work { command },
            } => {
                assert!(matches!(
                    command,
                    channel_work::WorkCommand::Context { .. }
                        | channel_work::WorkCommand::Show { .. }
                ));
                // This classification tests the separate public route; actual
                // ChannelWork authority is carried by its unchanged dispatcher.
                let work = ChannelCommand::Work { command };
                assert_eq!(channel_command_mutates(&work), None);
            }
            TopCommand::Channel { command } => {
                assert_eq!(channel_command_mutates(&command), Some(false))
            }
            _ => unreachable!(),
        }
    }
}

/// The real public implementation body, including capture BEFORE staged lookup,
/// is exercised in isolated self-exec children. Their output is captured, so
/// these six children are not six additional owning libtest parents. The parent
/// neither reads nor mutates its ambient authority, device, or start policy.
#[test]
fn actual_run_from_captures_scoped_writes_before_staged_lookup_without_user_fallback() {
    use std::{
        ffi::OsString,
        fs::{self, File},
        os::unix::{ffi::OsStringExt, fs::PermissionsExt},
    };
    const ROLE: &str = "CAGENTS_TEST_PROJECT_CHANNEL_STAGED_ROLE";
    const TOKEN: &str = "fixture-staged-original-capability";
    let selector = thread::current().name().unwrap().to_owned();
    let Some(role) = std::env::var_os(ROLE) else {
        for role in [
            "scoped",
            "invalid",
            "empty",
            "non-unicode",
            "user",
            "user-no-start",
        ] {
            let private = tempfile::Builder::new()
                .prefix("pc-stage-")
                .tempdir()
                .unwrap();
            for leaf in ["home", "runtime", "config", "data", "cache", "state"] {
                let path = private.path().join(leaf);
                fs::create_dir(&path).unwrap();
                fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
            }
            let stdout = private.path().join("stdout");
            let stderr = private.path().join("stderr");
            let mut command = Command::new(std::env::current_exe().unwrap());
            command
                .env_clear()
                .args(["--exact", &selector, "--nocapture", "--test-threads=1"])
                .current_dir(private.path())
                .env(ROLE, role)
                .env("PATH", "/usr/bin:/bin")
                .env("HOME", private.path().join("home"))
                .env("XDG_RUNTIME_DIR", private.path().join("runtime"))
                .env("XDG_CONFIG_HOME", private.path().join("config"))
                .env("XDG_DATA_HOME", private.path().join("data"))
                .env("XDG_CACHE_HOME", private.path().join("cache"))
                .env("XDG_STATE_HOME", private.path().join("state"))
                .stdin(Stdio::null())
                .stdout(File::create(&stdout).unwrap())
                .stderr(File::create(&stderr).unwrap());
            match role {
                "scoped" => {
                    command.env(crate::agent_execution::AUTHORITY_ENV, TOKEN);
                }
                "invalid" => {
                    command.env(
                        crate::agent_execution::AUTHORITY_ENV,
                        "fixture-invalid-stage-token",
                    );
                }
                "empty" => {
                    command.env(crate::agent_execution::AUTHORITY_ENV, "");
                }
                "non-unicode" => {
                    command.env(
                        crate::agent_execution::AUTHORITY_ENV,
                        OsString::from_vec(vec![0xff]),
                    );
                }
                _ => {}
            }
            let mut child = command.spawn().unwrap();
            let deadline = Instant::now() + Duration::from_secs(30);
            let status = loop {
                if let Some(status) = child.try_wait().unwrap() {
                    break status;
                }
                if Instant::now() >= deadline {
                    child.kill().unwrap();
                    child.wait().unwrap();
                    panic!("private staged-routing child exceeded its bound: {role}");
                }
                thread::sleep(Duration::from_millis(5));
            };
            assert!(fs::metadata(&stdout).unwrap().len() <= 1024 * 1024);
            assert!(fs::metadata(&stderr).unwrap().len() <= 1024 * 1024);
            let stdout = fs::read_to_string(stdout).unwrap();
            let stderr = fs::read_to_string(stderr).unwrap();
            assert!(status.success(), "{role}: {stdout}\n{stderr}");
            assert!(stdout.contains("1 passed; 0 failed; 0 ignored;"));
            assert!(!stdout.contains(TOKEN) && !stderr.contains(TOKEN));
            assert!(
                !stdout.contains("fixture-invalid-stage-token")
                    && !stderr.contains("fixture-invalid-stage-token")
            );
            let errors = stderr
                .lines()
                .filter_map(|line| serde_json::from_str::<Value>(line).ok())
                .collect::<Vec<_>>();
            assert_eq!(errors.len(), 1);
            assert_eq!(errors[0]["ok"], false);
            if role.starts_with("user") {
                assert_eq!(errors[0]["error"]["kind"], "deferred_install");
            } else {
                assert_eq!(
                    errors[0]["error"]["details"]["code"],
                    json!(ProtocolErrorCode::PermissionDenied)
                );
                assert_eq!(errors[0]["error"]["details"]["retryable"], false);
            }
        }
        return;
    };
    let role = role.to_str().unwrap();
    let channel = ChannelId::new();
    let mut args = vec![
        "cagents".to_owned(),
        "--device".into(),
        "local".into(),
        "--json".into(),
    ];
    if role == "user-no-start" {
        args.push("--no-start".into());
    }
    args.extend(["channel".into(), "close".into(), channel.to_string()]);
    if role.starts_with("user") {
        let lookups = Cell::new(0);
        let result = run_from_with_staged_lookup(args, |executable, recover| {
            lookups.set(lookups.get() + 1);
            assert_eq!(executable, std::env::current_exe().unwrap());
            assert_eq!(recover, role == "user");
            Err(io::Error::other("controlled staged-lookup refusal"))
        });
        assert_eq!(result, ExitCode::FAILURE);
        assert_eq!(lookups.get(), 1);
        return;
    }
    let expected = if role == "scoped" {
        Authority::session(TOKEN.into())
    } else if role == "invalid" {
        Authority::session("fixture-invalid-stage-token".into())
    } else {
        Authority::session(String::new())
    };
    let socket = daemon_socket_path().unwrap();
    fs::create_dir_all(socket.parent().unwrap()).unwrap();
    let directory = tempfile::Builder::new()
        .prefix("pc-stage-peer-")
        .tempdir()
        .unwrap();
    let scoped = role == "scoped";
    let peer = Peer::bound(directory, socket, connected(), move |peer| {
        auth(peer, &expected, scoped);
        if scoped {
            let request = next(peer);
            assert_eq!(
                request.request,
                ClientRequest::EditWorkspaceModel {
                    edit: ModelEdit::DeleteChannel { id: channel }
                }
            );
            reply(peer, &request, denied());
        }
    });
    let result = run_from_with_staged_lookup(args, |_, _| {
        panic!("scoped legacy writes must not consult or recover a staged User owner")
    });
    assert_eq!(result, ExitCode::FAILURE);
    peer.finished();
}
