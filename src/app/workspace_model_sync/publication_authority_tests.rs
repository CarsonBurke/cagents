use super::{ModelEdit, ModelUpdate, PublicationContext, publish_with};
use crate::{
    agent_execution::Authority,
    control::{ControlClient, ControlError},
    model::{
        BootId, CardId, ChannelId, HostIncarnation, HostSessionId, HostSessionProof, ProjectKind,
    },
    terminal::ipc::{
        ClientEnvelope, ClientRequest, PROTOCOL_VERSION, ProtocolError, ProtocolErrorCode,
        ServerEnvelope, ServerMessage, ServerResponse, read_frame, write_frame,
    },
    workspace_model::{
        WorkspaceModel,
        views::{Change, Scope},
    },
};
use std::{
    cell::{Cell, RefCell},
    io,
    os::unix::net::{UnixListener, UnixStream},
    path::{Path, PathBuf},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

fn proof() -> HostSessionProof {
    HostSessionProof {
        host_session_id: HostSessionId::new("workspace-publication-fixture"),
        boot_id: BootId::new("fixture-boot"),
        host_incarnation: HostIncarnation::new("fixture-incarnation"),
    }
}

fn response(peer: &mut UnixStream, request: &ClientEnvelope, message: ServerMessage) {
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

fn accepted(peer: &mut UnixStream, request: &ClientEnvelope) {
    response(
        peer,
        request,
        ServerMessage::Response {
            response: ServerResponse::Accepted,
        },
    );
}

fn refused() -> ServerMessage {
    ServerMessage::Error(ProtocolError::new(
        ProtocolErrorCode::PermissionDenied,
        "The private fixture refused this publication",
        false,
    ))
}

fn model(peer: &mut UnixStream, request: &ClientEnvelope) {
    response(
        peer,
        request,
        ServerMessage::Response {
            response: ServerResponse::WorkspaceModel {
                model: WorkspaceModel::default(),
            },
        },
    );
}

fn next(peer: &mut UnixStream) -> ClientEnvelope {
    let request: ClientEnvelope = read_frame(peer).unwrap().unwrap();
    assert_eq!(
        request.proof,
        Some(proof()),
        "requests must retain the actual handshake proof"
    );
    request
}

fn authenticate(peer: &mut UnixStream, authority: &Authority) {
    let request = next(peer);
    assert!(
        matches!(&request.request, ClientRequest::Authenticate { authority: actual } if actual == authority),
        "the exact captured authority must precede every publication connection's edit"
    );
    accepted(peer, &request);
}

fn reopen(name: &str) -> ModelEdit {
    ModelEdit::ReopenProject {
        repository: PathBuf::from("/private-publication-fixture").join(name),
        name: name.into(),
        kind: ProjectKind::Folder,
    }
}

fn expect_edit(peer: &mut UnixStream, expected: &ModelEdit) -> ClientEnvelope {
    let request = next(peer);
    assert!(
        matches!(&request.request, ClientRequest::EditWorkspaceModel { edit } if edit == expected),
        "the queued edit must be sent once with its original identity"
    );
    request
}

/// Real private framed transport, not a daemon, provider, or production socket.
/// The server returns false only when deliberately dropping a connection before
/// its reply. All accepts and stream IO have finite bounds; no helper process.
struct Peer {
    _directory: tempfile::TempDir,
    socket: PathBuf,
    worker: Option<JoinHandle<UnixListener>>,
}

impl Peer {
    fn new(
        handshakes: Vec<ServerMessage>,
        mut serve: impl FnMut(usize, &mut UnixStream) -> bool + Send + 'static,
    ) -> Self {
        let directory = tempfile::Builder::new().prefix("wp-").tempdir().unwrap();
        let socket = directory.path().join("publication.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        let worker = thread::spawn(move || {
            for (index, handshake) in handshakes.into_iter().enumerate() {
                let deadline = Instant::now() + Duration::from_secs(5);
                let mut peer = loop {
                    match listener.accept() {
                        Ok((peer, _)) => break peer,
                        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                            assert!(
                                Instant::now() < deadline,
                                "private publication peer was not connected"
                            );
                            thread::sleep(Duration::from_millis(2));
                        }
                        Err(error) => panic!("private publication listener: {error}"),
                    }
                };
                peer.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
                peer.set_write_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let hello: ClientEnvelope = read_frame(&mut peer).unwrap().unwrap();
                assert!(matches!(&hello.request, ClientRequest::Connect { .. }));
                assert!(hello.proof.is_none());
                response(&mut peer, &hello, handshake);
                if serve(index, &mut peer) {
                    assert!(
                        read_frame::<ClientEnvelope>(&mut peer).unwrap().is_none(),
                        "no additional edit, authentication, or User fallback after the result"
                    );
                }
            }
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
            "publication must not make an extra retry connection"
        );
    }
}

impl Drop for Peer {
    fn drop(&mut self) {
        // On an assertion failure, retain the private listener/thread until its
        // finite accept/read bound retires it; no detached background test work.
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn existing(context: &PublicationContext, socket: &Path) -> Result<ControlClient, ControlError> {
    context.connect_with(
        || Ok(socket.to_owned()),
        || panic!("present authority or explicit no-start must not enter readiness"),
        ControlClient::connect,
    )
}

#[test]
fn user_publication_preserves_ready_no_start_remote_and_ordered_connection_reuse() {
    for remote in [false, true] {
        for no_start in [false, true] {
            let first = reopen("first");
            let second = reopen("second");
            let expected = [first.clone(), second.clone()];
            let peer = Peer::new(vec![connected()], move |_, peer| {
                // No token is the ordinary User path; global constructor auth
                // is not introduced. The first frame is the real model edit.
                for edit in &expected {
                    let request = expect_edit(peer, edit);
                    model(peer, &request);
                }
                true
            });
            let context = PublicationContext {
                authority: Authority::default(),
                remote,
                no_start,
            };
            let selections = Cell::new(0);
            let starts = Cell::new(0);
            let connections = Cell::new(0);
            let (tx, requests) = async_channel::unbounded();
            tx.send_blocking(first).unwrap();
            tx.send_blocking(second).unwrap();
            drop(tx);
            let (updates, received) = async_channel::unbounded();
            let mut pending = None;
            let error = publish_with(&updates, &requests, &mut pending, || {
                connections.set(connections.get() + 1);
                context.connect_with(
                    || {
                        selections.set(selections.get() + 1);
                        Ok(peer.socket.clone())
                    },
                    || {
                        starts.set(starts.get() + 1);
                        Ok(peer.socket.clone())
                    },
                    ControlClient::connect,
                )
            })
            .unwrap_err();
            assert_eq!(error, "the window stopped publishing");
            assert!(pending.is_none() && requests.is_empty());
            assert_eq!(connections.get(), 1);
            assert_eq!(selections.get(), usize::from(no_start));
            assert_eq!(starts.get(), usize::from(!no_start));
            for name in ["first", "second"] {
                assert!(
                    matches!(received.try_recv().unwrap(), ModelUpdate::Reopened { repository, .. }
                    if repository == PathBuf::from("/private-publication-fixture").join(name))
                );
            }
            assert!(received.try_recv().is_err());
            peer.finished();
        }
    }
}

#[test]
fn scoped_publication_authenticates_original_capture_before_one_refused_edit() {
    let authority = Authority::session("fixture-original-publication-scope".into());
    let expected_authority = authority.clone();
    let edit = reopen("scoped");
    let expected_edit = edit.clone();
    let peer = Peer::new(vec![connected()], move |_, peer| {
        authenticate(peer, &expected_authority);
        let request = expect_edit(peer, &expected_edit);
        response(peer, &request, refused());
        true
    });
    let later_sample = RefCell::new(authority.clone());
    let context = PublicationContext {
        authority: later_sample.borrow().clone(),
        remote: false,
        no_start: false,
    };
    let (tx, requests) = async_channel::unbounded();
    tx.send_blocking(edit).unwrap();
    drop(tx);
    let (updates, received) = async_channel::unbounded();
    let mut pending = None;
    let selections = Cell::new(0);
    let error = publish_with(&updates, &requests, &mut pending, || {
        context.connect_with(
            || {
                selections.set(selections.get() + 1);
                *later_sample.borrow_mut() = Authority::default();
                Ok(peer.socket.clone())
            },
            || panic!("scoped publication must not start a daemon"),
            ControlClient::connect,
        )
    })
    .unwrap_err();
    assert_eq!(selections.get(), 1);
    assert!(pending.is_none() && requests.is_empty());
    assert!(error.contains("PermissionDenied") && !error.contains(authority.token().unwrap()));
    assert!(
        matches!(received.try_recv().unwrap(), ModelUpdate::ReopenFailed { repository, .. }
        if repository == PathBuf::from("/private-publication-fixture/scoped"))
    );
    assert!(received.try_recv().is_err());
    peer.finished();
}

#[test]
fn invalid_and_empty_authentication_refusals_drain_distinct_edits_without_user_fallback() {
    for authority in [
        Authority::session("fixture-invalid-publication-scope".into()),
        Authority::session(String::new()),
    ] {
        let expected = authority.clone();
        let peer = Peer::new(vec![connected(), connected()], move |_, peer| {
            let request = next(peer);
            assert!(
                matches!(&request.request, ClientRequest::Authenticate { authority } if authority == &expected)
            );
            response(peer, &request, refused());
            true
        });
        let context = PublicationContext {
            authority: authority.clone(),
            remote: false,
            no_start: false,
        };
        let (tx, requests) = async_channel::unbounded();
        for name in ["denied-first", "denied-second"] {
            tx.send_blocking(reopen(name)).unwrap();
        }
        drop(tx);
        let (updates, received) = async_channel::unbounded();
        let mut pending = None;
        let connections = Cell::new(0);
        for name in ["denied-first", "denied-second"] {
            let error = publish_with(&updates, &requests, &mut pending, || {
                connections.set(connections.get() + 1);
                existing(&context, &peer.socket)
            })
            .unwrap_err();
            assert!(
                pending.is_none(),
                "authentication refusal must not strand the edit"
            );
            assert!(error.contains("PermissionDenied"));
            if let Some(token) = authority.token().filter(|token| !token.is_empty()) {
                assert!(!error.contains(token));
            }
            assert!(
                matches!(received.try_recv().unwrap(), ModelUpdate::ReopenFailed { repository, .. }
                if repository == PathBuf::from("/private-publication-fixture").join(name))
            );
        }
        assert_eq!(
            connections.get(),
            2,
            "one auth attempt per distinct edit; no fallback/replay"
        );
        assert!(requests.is_empty() && received.try_recv().is_err());
        peer.finished();
    }
}

#[test]
fn handshake_refusal_or_proof_mismatch_never_authenticates_edits_or_retries() {
    let mut different_proof = proof();
    different_proof.host_incarnation = HostIncarnation::new("other-fixture-incarnation");
    for (handshake, permanent) in [
        (refused(), true),
        (
            ServerMessage::Response {
                response: ServerResponse::Connected {
                    proof: different_proof,
                    resumed: vec![],
                },
            },
            false,
        ),
    ] {
        let peer = Peer::new(vec![handshake], |_, _| true);
        let context = PublicationContext {
            authority: Authority::session("fixture-handshake-publication".into()),
            remote: false,
            no_start: false,
        };
        let error = existing(&context, &peer.socket)
            .err()
            .expect("handshake must fail");
        if permanent {
            assert!(matches!(
                error,
                ControlError::Remote {
                    code: ProtocolErrorCode::PermissionDenied,
                    retryable: false,
                    ..
                }
            ));
        } else {
            assert!(matches!(error, ControlError::ProofMismatch));
        }
        peer.finished();
    }
}

#[test]
fn scoped_remote_refuses_before_all_resolvers_and_rejects_the_current_edit() {
    for authority in [
        Authority::session("fixture-remote-publication".into()),
        Authority::session(String::new()),
    ] {
        for no_start in [false, true] {
            let context = PublicationContext {
                authority: authority.clone(),
                remote: true,
                no_start,
            };
            let (tx, requests) = async_channel::unbounded();
            tx.send_blocking(reopen("remote")).unwrap();
            drop(tx);
            let (updates, received) = async_channel::unbounded();
            let mut pending = None;
            let error = publish_with(&updates, &requests, &mut pending, || {
                context.connect_with(
                    || panic!("scoped remote selection must not create an SSH bridge"),
                    || panic!("scoped remote publication must not start a daemon"),
                    |_| panic!("scoped remote publication must not connect"),
                )
            })
            .unwrap_err();
            assert!(pending.is_none() && requests.is_empty());
            assert!(error.contains("require your own terminal on the remote device"));
            if let Some(token) = authority.token().filter(|token| !token.is_empty()) {
                assert!(!error.contains(token));
            }
            assert!(
                matches!(received.try_recv().unwrap(), ModelUpdate::ReopenFailed { repository, .. }
                if repository == PathBuf::from("/private-publication-fixture/remote"))
            );
            assert!(received.try_recv().is_err());
        }
    }
}

#[test]
fn selected_socket_and_connection_failures_never_fall_back_to_readiness() {
    for (authority, no_start, remote) in [
        (
            Authority::session("fixture-selected-publication".into()),
            false,
            false,
        ),
        (Authority::session(String::new()), false, false),
        (Authority::default(), true, false),
        (Authority::default(), true, true),
    ] {
        let context = PublicationContext {
            authority,
            remote,
            no_start,
        };
        let selections = Cell::new(0);
        let error = context
            .connect_with(
                || {
                    selections.set(selections.get() + 1);
                    Err("fixed selected socket unavailable".into())
                },
                || panic!("selected resolver failure cannot become User readiness"),
                |_| panic!("resolution failed before connect"),
            )
            .err()
            .unwrap();
        assert_eq!(selections.get(), 1);
        assert_eq!(
            error.to_string(),
            "could not connect to the daemon: fixed selected socket unavailable"
        );
        let connections = Cell::new(0);
        let error = context
            .connect_with(
                || Ok(PathBuf::from("/fixed-private-publication.sock")),
                || panic!("connection failure cannot start a daemon"),
                |path| {
                    assert_eq!(path, Path::new("/fixed-private-publication.sock"));
                    connections.set(connections.get() + 1);
                    Err(ControlError::Connect(io::Error::from(
                        io::ErrorKind::NotFound,
                    )))
                },
            )
            .err()
            .unwrap();
        assert!(matches!(error, ControlError::Connect(_)));
        assert_eq!(connections.get(), 1);
    }
    let context = PublicationContext {
        authority: Authority::default(),
        remote: false,
        no_start: false,
    };
    let error = context
        .connect_with(
            || panic!("ordinary User keeps readiness selection"),
            || Err("User readiness failed".into()),
            |_| panic!("readiness failed before connect"),
        )
        .err()
        .unwrap();
    assert_eq!(
        error.to_string(),
        "could not connect to the daemon: User readiness failed"
    );
}

#[test]
fn transient_disconnect_keeps_pending_order_and_reconnects_with_original_authority() {
    let authority = Authority::session("fixture-reconnected-publication".into());
    let expected = authority.clone();
    let first = reopen("retained-first");
    let second = reopen("queued-second");
    let wanted_first = first.clone();
    let wanted_second = second.clone();
    let peer = Peer::new(vec![connected(), connected()], move |index, peer| {
        authenticate(peer, &expected);
        let request = expect_edit(peer, &wanted_first);
        if index == 0 {
            return false;
        } // Real framed EOF before acknowledgement.
        model(peer, &request);
        let request = expect_edit(peer, &wanted_second);
        model(peer, &request);
        true
    });
    let later_sample = RefCell::new(authority.clone());
    let context = PublicationContext {
        authority: later_sample.borrow().clone(),
        remote: false,
        no_start: false,
    };
    let (tx, requests) = async_channel::unbounded();
    tx.send_blocking(first.clone()).unwrap();
    tx.send_blocking(second).unwrap();
    drop(tx);
    let (updates, received) = async_channel::unbounded();
    let mut pending = None;
    let connections = Cell::new(0);
    let mut connect = || {
        connections.set(connections.get() + 1);
        *later_sample.borrow_mut() = Authority::default();
        existing(&context, &peer.socket)
    };
    let error = publish_with(&updates, &requests, &mut pending, &mut connect).unwrap_err();
    assert!(error.contains("closed the connection"));
    assert_eq!(pending, Some(first));
    assert_eq!(requests.len(), 1);
    assert!(received.try_recv().is_err());
    let error = publish_with(&updates, &requests, &mut pending, &mut connect).unwrap_err();
    assert_eq!(error, "the window stopped publishing");
    assert!(pending.is_none() && requests.is_empty());
    assert_eq!(connections.get(), 2);
    for name in ["retained-first", "queued-second"] {
        assert!(
            matches!(received.try_recv().unwrap(), ModelUpdate::Reopened { repository, .. }
            if repository == PathBuf::from("/private-publication-fixture").join(name))
        );
    }
    assert!(received.try_recv().is_err());
    peer.finished();
}

#[test]
fn authentication_refusal_uses_existing_navigation_view_and_reopen_rejections() {
    let authority = Authority::session("fixture-rejected-queued-publication".into());
    let expected = authority.clone();
    let peer = Peer::new(
        vec![connected(), connected(), connected()],
        move |_, peer| {
            let request = next(peer);
            assert!(
                matches!(&request.request, ClientRequest::Authenticate { authority } if authority == &expected)
            );
            response(peer, &request, refused());
            true
        },
    );
    let scope = Scope::Channel(ChannelId::new());
    let operation = uuid::Uuid::new_v4();
    let edits = [
        ModelEdit::ResetNavigation {
            repository: PathBuf::from("/private-publication-fixture/navigation"),
        },
        ModelEdit::ChangeView {
            scope: scope.clone(),
            expected_revision: 17,
            operation,
            change: Change::Remove { id: CardId::new() },
        },
        reopen("rejected-reopen"),
    ];
    let context = PublicationContext {
        authority,
        remote: false,
        no_start: false,
    };
    let (tx, requests) = async_channel::unbounded();
    for edit in edits {
        tx.send_blocking(edit).unwrap();
    }
    drop(tx);
    let (updates, received) = async_channel::unbounded();
    let mut pending = None;
    let connections = Cell::new(0);
    for _ in 0..3 {
        publish_with(&updates, &requests, &mut pending, || {
            connections.set(connections.get() + 1);
            existing(&context, &peer.socket)
        })
        .unwrap_err();
        assert!(pending.is_none());
    }
    assert_eq!(connections.get(), 3);
    assert!(requests.is_empty());
    assert!(
        matches!(received.try_recv().unwrap(), ModelUpdate::NavigationFailed(error) if error.contains("PermissionDenied"))
    );
    assert!(
        matches!(received.try_recv().unwrap(), ModelUpdate::ViewRefused { scope: actual_scope, operation: actual_operation, .. }
        if actual_scope == scope && actual_operation == operation)
    );
    assert!(
        matches!(received.try_recv().unwrap(), ModelUpdate::ReopenFailed { repository, .. }
        if repository == PathBuf::from("/private-publication-fixture/rejected-reopen"))
    );
    assert!(
        received.try_recv().is_err(),
        "refused edits must never produce success/reconciliation receipts"
    );
    peer.finished();
}

#[test]
fn failed_authentication_drains_title_and_reports_original_candidate_without_read() {
    let authority = Authority::session("fixture-refused-title-publication".into());
    let expected = authority.clone();
    let peer = Peer::new(vec![connected()], move |_, peer| {
        let request = next(peer);
        assert!(
            matches!(&request.request, ClientRequest::Authenticate { authority } if authority == &expected)
        );
        response(peer, &request, refused());
        true
    });
    let edit = ModelEdit::SuggestConversationTitle {
        id: ChannelId::new(),
        card: CardId::new(),
        expected_revision: 17,
        title: "Inert private fixture title".into(),
        source: crate::model::ChannelTitleSource::Provider,
    };
    let context = PublicationContext {
        authority,
        remote: false,
        no_start: false,
    };
    let (tx, requests) = async_channel::unbounded();
    let expected_edit = edit.clone();
    tx.send_blocking(edit).unwrap();
    drop(tx);
    let (updates, received) = async_channel::unbounded();
    let mut pending = None;
    let connections = Cell::new(0);
    let error = publish_with(&updates, &requests, &mut pending, || {
        connections.set(connections.get() + 1);
        existing(&context, &peer.socket)
    })
    .unwrap_err();
    assert!(error.contains("PermissionDenied"));
    assert_eq!(connections.get(), 1);
    assert!(pending.is_none() && requests.is_empty());
    assert!(
        matches!(received.try_recv().unwrap(), ModelUpdate::TitleRefused(actual)
        if actual == expected_edit),
        "refusal must retain the original title target, ownership proof and candidate"
    );
    assert!(
        received.try_recv().is_err(),
        "failed authentication supplies no client for a title fallback read and no success/defer receipt"
    );
    peer.finished();
}

#[test]
fn intentionally_global_model_read_and_watch_remain_unauthenticated() {
    let peer = Peer::new(vec![connected()], |_, peer| {
        let read = next(peer);
        assert!(matches!(&read.request, ClientRequest::ReadWorkspaceModel));
        model(peer, &read);
        let watch = next(peer);
        assert!(matches!(&watch.request, ClientRequest::WatchWorkspaceModel));
        model(peer, &watch);
        true
    });
    let mut client = ControlClient::connect(&peer.socket).unwrap();
    assert_eq!(
        client.read_workspace_model().unwrap(),
        WorkspaceModel::default()
    );
    assert_eq!(
        client.watch_workspace_model().unwrap(),
        WorkspaceModel::default()
    );
    drop(client);
    peer.finished();
}

/// The existing App fixture has no session/store, provider, or publisher thread.
/// Real suggest_channel_title mutates durable/sidebar optimism and queues the
/// candidate; only that exact candidate is later fed through the real reducer.
fn title_fixture(
    confirmed_title: &str,
    confirmed_source: crate::model::ChannelTitleSource,
) -> (
    crate::app::Cagents,
    ModelEdit,
    async_channel::Receiver<ModelEdit>,
) {
    use crate::model::{
        CardKind, ChannelTitleSource, ProjectChannel, ProjectId, RepositoryId, TerminalId,
    };
    use crate::workspace_model::{
        ModelChannel, ModelProject,
        views::{Pane, View},
    };
    use std::sync::Arc;

    let mut app = crate::app::tests::context_inspector_test_app(true);
    assert!(app.session.is_none());
    let card = app.durable.cards[0].id;
    let project = ProjectId::new();
    let channel = ChannelId::new();
    let unrelated = ChannelId::new();
    let repository = PathBuf::from("/private-publication-fixture/title-project");
    let session = crate::chat::Session {
        native_continuity: Default::default(),
        owned_codex_listener: None,
        id: TerminalId::new(),
        launch: crate::chat::Launch {
            provider: crate::agent::Provider::Omp,
            recipe: crate::model::LaunchRecipe {
                unset_environment: Default::default(),
                program: "/bin/sh".into(),
                arguments: Vec::new(),
                working_directory: repository.clone(),
                environment: Default::default(),
            },
            session_id: None,
        },
        proof: None,
        terminal: None,
    };
    let durable = Arc::make_mut(&mut app.durable);
    durable.cards[0].kind = CardKind::Chat(session);
    durable.card_scopes.insert(card, repository.clone());
    durable.active_channel = Some(channel);
    durable.channels.push(ProjectChannel {
        id: channel,
        project_id: project,
        title: confirmed_title.into(),
        // A stale local prompt fallback may coexist with a confirmed manual
        // name. The real suggestion checks local source plus current view proof.
        title_source: ChannelTitleSource::Prompt,
        ordinal: 1,
        shared: true,
        cards: vec![card],
        focused: Some(card),
    });
    durable.channels.push(ProjectChannel {
        id: unrelated,
        project_id: project,
        title: "Unrelated optimistic title".into(),
        title_source: ChannelTitleSource::Provider,
        ordinal: 2,
        shared: true,
        cards: vec![],
        focused: None,
    });
    app.sidebar_model
        .projects
        .push(crate::sidebar::SidebarProject::new(
            project,
            RepositoryId::new(),
            "fixture".into(),
            repository.clone(),
            Vec::new(),
            Vec::new(),
        ));
    app.workspace_model = WorkspaceModel {
        identity: uuid::Uuid::new_v4(),
        sequence: 17,
        projects: vec![ModelProject {
            repository: repository.clone(),
            name: "fixture".into(),
            kind: ProjectKind::Git,
            ordinal: 1,
        }],
        channels: vec![
            ModelChannel {
                id: channel,
                repository: repository.clone(),
                title: confirmed_title.into(),
                title_source: confirmed_source,
                ordinal: 1,
            },
            ModelChannel {
                id: unrelated,
                repository: repository.clone(),
                title: "Unrelated confirmed title".into(),
                title_source: ChannelTitleSource::Manual,
                ordinal: 2,
            },
        ],
        views: vec![
            View {
                scope: Scope::Channel(channel),
                revision: 17,
                panes: vec![Pane::from_card(&app.durable.cards[0], Some(repository)).unwrap()],
                retired: vec![],
            },
            View {
                scope: Scope::Channel(unrelated),
                revision: 0,
                panes: vec![],
                retired: vec![],
            },
        ],
        ..WorkspaceModel::default()
    };
    app.refresh_sidebar_channels();
    let (tx, queued) = async_channel::unbounded();
    app.workspace_model_tx = tx;
    // Establish both views through the existing seed-publication/acknowledgment
    // APIs before measuring title optimism. Do not suppress publication with an
    // applying flag: a later rollback is still allowed to expose an extra edit.
    app.publish_view_changes();
    let mut seeds = Vec::new();
    while let Ok(seed) = queued.try_recv() {
        seeds.push(seed);
    }
    assert_eq!(
        seeds.len(),
        2,
        "the fixture declares exactly two channel views"
    );
    let mut seeded = Vec::new();
    for seed in seeds {
        let ModelEdit::SeedView {
            scope,
            panes,
            closed,
        } = seed
        else {
            panic!("unexpected fixture initialization edit");
        };
        assert!(scope == Scope::Channel(channel) || scope == Scope::Channel(unrelated));
        assert!(!seeded.contains(&scope));
        let confirmed = app
            .workspace_model
            .views
            .iter()
            .find(|view| view.scope == scope)
            .unwrap();
        assert_eq!(panes, confirmed.panes);
        assert!(closed.is_empty());
        seeded.push(scope.clone());
        app.acknowledge_view_edit(scope, None);
    }
    assert!(
        queued.try_recv().is_err(),
        "accepted fixture baselines must be settled before title capture"
    );
    assert!(app.suggest_channel_title(
        card,
        "Candidate to refuse".into(),
        ChannelTitleSource::Provider
    ));
    let edit = queued.try_recv().unwrap();
    assert!(
        matches!(&edit, ModelEdit::SuggestConversationTitle { id, card: actual_card, expected_revision: 17, title, source: ChannelTitleSource::Provider }
        if *id == channel && *actual_card == card && title == "Candidate to refuse")
    );
    assert_eq!(app.durable.channels[0].title, "Candidate to refuse");
    assert_eq!(
        app.sidebar_model.projects[0]
            .channels
            .iter()
            .find(|row| row.id == channel)
            .unwrap()
            .title,
        "Candidate to refuse"
    );
    assert!(queued.try_recv().is_err());
    (app, edit, queued)
}

#[test]
fn title_refusal_projects_only_the_confirmed_row_including_empty_and_manual_titles() {
    use crate::model::ChannelTitleSource;
    for (title, source) in [
        ("Confirmed prompt", ChannelTitleSource::Prompt),
        ("Saved manual", ChannelTitleSource::Manual),
        ("", ChannelTitleSource::Manual),
    ] {
        let (mut app, edit, queued) = title_fixture(title, source);
        let target = app.durable.channels[0].id;
        let unrelated_before = app.durable.channels[1].clone();
        let model_before = app.workspace_model.clone();
        let cards_before = app.durable.cards.clone();
        drop(app.apply_workspace_model_update(ModelUpdate::TitleRefused(edit.clone())));
        assert_eq!(app.durable.channels[0].title, title);
        assert_eq!(app.durable.channels[0].title_source, source);
        let row = app.sidebar_model.projects[0]
            .channels
            .iter()
            .find(|row| row.id == target)
            .unwrap();
        assert_eq!(
            row.title,
            if title.is_empty() {
                "New channel"
            } else {
                title
            }
        );
        assert_eq!(
            app.durable.channels[1], unrelated_before,
            "a targeted refusal must not project an unrelated cached canonical title"
        );
        assert_eq!(app.workspace_model, model_before);
        assert_eq!(app.durable.cards, cards_before);
        assert!(
            queued.try_recv().is_err(),
            "rollback must not publish another edit"
        );
        let restored = app.durable.channels.clone();
        drop(app.apply_workspace_model_update(ModelUpdate::TitleRefused(edit)));
        assert_eq!(
            app.durable.channels, restored,
            "a repeated old refusal cannot create another transition"
        );
        assert!(queued.try_recv().is_err());
    }
}

#[test]
fn a_newer_real_optimistic_title_survives_an_older_title_refusal() {
    use crate::model::ChannelTitleSource;
    let (mut app, old_edit, queued) = title_fixture("Confirmed", ChannelTitleSource::Prompt);
    let card = app.durable.channels[0].cards[0];
    assert!(app.suggest_channel_title(
        card,
        "Newer candidate".into(),
        ChannelTitleSource::Provider
    ));
    let new_edit = queued.try_recv().unwrap();
    let before = app.durable.channels.clone();
    drop(app.apply_workspace_model_update(ModelUpdate::TitleRefused(old_edit)));
    assert_eq!(app.durable.channels, before);
    assert_eq!(app.durable.channels[0].title, "Newer candidate");
    assert!(queued.try_recv().is_err());
    drop(app.apply_workspace_model_update(ModelUpdate::TitleRefused(new_edit)));
    assert_eq!(
        app.durable.channels[0].title, "Confirmed",
        "the current exact candidate can still be reconciled after the stale refusal"
    );
    assert_eq!(
        app.durable.channels[0].title_source,
        ChannelTitleSource::Prompt
    );
    assert!(queued.try_recv().is_err());
}

#[test]
fn title_refusal_does_not_project_after_source_channel_or_ownership_replacement() {
    use crate::{
        app::Cagents,
        model::{CardKind, ChannelTitleSource, Existence, OperationId, ProjectId, TerminalId},
    };
    use std::sync::Arc;
    type Mutation = fn(&mut Cagents);
    let mutations: [(&str, Mutation); 14] = [
        ("channel deletion underway", |app| {
            let id = app.durable.channels[0].id;
            Arc::make_mut(&mut app.durable).active_channel = None;
            // Drop the returned task: no session-close future is executed.
            drop(app.delete_channel(id));
            assert!(app.channel_ui.is_deleting(id));
        }),
        ("local title source changed", |app| {
            Arc::make_mut(&mut app.durable).channels[0].title_source = ChannelTitleSource::Manual
        }),
        ("local channel replaced", |app| {
            Arc::make_mut(&mut app.durable).channels[0].id = ChannelId::new()
        }),
        ("local primary removed", |app| {
            Arc::make_mut(&mut app.durable).channels[0].cards.clear()
        }),
        ("local chat closing", |app| {
            Arc::make_mut(&mut app.durable).cards[0].existence = Existence::Closing {
                operation_id: OperationId::new(),
            }
        }),
        ("current source session replaced", |app| {
            if let CardKind::Chat(session) = &mut Arc::make_mut(&mut app.durable).cards[0].kind {
                session.id = TerminalId::new();
            } else {
                panic!("fixture chat missing");
            }
        }),
        ("confirmed source session replaced", |app| {
            if let CardKind::Chat(session) = &mut app.workspace_model.views[0].panes[0].kind {
                session.id = TerminalId::new();
            } else {
                panic!("fixture pane missing");
            }
        }),
        ("confirmed revision advanced", |app| {
            app.workspace_model.views[0].revision += 1
        }),
        ("confirmed primary replaced", |app| {
            app.workspace_model.views[0].panes[0].id = CardId::new()
        }),
        ("confirmed view missing", |app| {
            app.workspace_model.views.clear()
        }),
        ("confirmed channel missing", |app| {
            app.workspace_model.channels.remove(0);
        }),
        ("confirmed repository changed", |app| {
            app.workspace_model.channels[0].repository = PathBuf::from("/replacement-project")
        }),
        ("local project replaced", |app| {
            Arc::make_mut(&mut app.durable).channels[0].project_id = ProjectId::new()
        }),
        ("no confirmed registry", |app| {
            app.workspace_model.identity = crate::workspace_model::NO_REGISTRY
        }),
    ];
    for (label, mutate) in mutations {
        let (mut app, edit, queued) = title_fixture("Confirmed", ChannelTitleSource::Prompt);
        mutate(&mut app);
        let channels_before = app.durable.channels.clone();
        let cards_before = app.durable.cards.clone();
        let model_before = app.workspace_model.clone();
        drop(app.apply_workspace_model_update(ModelUpdate::TitleRefused(edit)));
        assert_eq!(app.durable.channels, channels_before, "{label}");
        assert_eq!(app.durable.cards, cards_before, "{label}");
        assert_eq!(app.workspace_model, model_before, "{label}");
        assert!(
            queued.try_recv().is_err(),
            "{label}: no compensating mutation"
        );
    }
}

#[test]
fn scoped_remote_title_refusal_restores_the_confirmed_row_without_a_resolver() {
    use crate::model::ChannelTitleSource;
    let (mut app, edit, requests) = title_fixture("Confirmed", ChannelTitleSource::Prompt);
    app.workspace_model_tx.send_blocking(edit.clone()).unwrap();
    let context = PublicationContext {
        authority: Authority::session("fixture-remote-title-refusal".into()),
        remote: true,
        no_start: false,
    };
    let (updates, received) = async_channel::unbounded();
    let mut pending = None;
    let error = publish_with(&updates, &requests, &mut pending, || {
        context.connect_with(
            || panic!("remote scoped title must not resolve/SSH"),
            || panic!("remote scoped title must not start a daemon"),
            |_| panic!("remote scoped title must not connect"),
        )
    })
    .unwrap_err();
    assert!(error.contains("require your own terminal on the remote device"));
    assert!(pending.is_none() && requests.is_empty());
    let refusal = received.try_recv().unwrap();
    assert!(matches!(&refusal, ModelUpdate::TitleRefused(actual) if actual == &edit));
    assert!(received.try_recv().is_err());
    drop(app.apply_workspace_model_update(refusal));
    assert_eq!(app.durable.channels[0].title, "Confirmed");
    assert_eq!(
        app.durable.channels[0].title_source,
        ChannelTitleSource::Prompt
    );
    assert_eq!(app.durable.channels[1].title, "Unrelated optimistic title");
    assert!(
        requests.try_recv().is_err(),
        "reconciliation must not enqueue a replacement edit"
    );
}

#[test]
fn post_edit_title_refusal_reads_the_same_authenticated_or_user_connection() {
    for authority in [
        Authority::default(),
        Authority::session("fixture-valid-title-publication".into()),
    ] {
        let expected_authority = authority.clone();
        let edit = ModelEdit::SuggestConversationTitle {
            id: ChannelId::new(),
            card: CardId::new(),
            expected_revision: 17,
            title: "Post-edit refused title".into(),
            source: crate::model::ChannelTitleSource::Provider,
        };
        let expected_edit = edit.clone();
        let peer = Peer::new(vec![connected()], move |_, peer| {
            if expected_authority.token().is_some() {
                authenticate(peer, &expected_authority);
            }
            let request = expect_edit(peer, &expected_edit);
            response(peer, &request, refused());
            let read = next(peer);
            assert!(
                matches!(&read.request, ClientRequest::ReadWorkspaceModel),
                "a refused edit with an installed client retains its original same-client canonical read"
            );
            model(peer, &read);
            true
        });
        let context = PublicationContext {
            authority,
            remote: false,
            no_start: true,
        };
        let (tx, requests) = async_channel::unbounded();
        tx.send_blocking(edit).unwrap();
        drop(tx);
        let (updates, received) = async_channel::unbounded();
        let mut pending = None;
        let connections = Cell::new(0);
        let error = publish_with(&updates, &requests, &mut pending, || {
            connections.set(connections.get() + 1);
            existing(&context, &peer.socket)
        })
        .unwrap_err();
        assert!(error.contains("PermissionDenied"));
        assert_eq!(connections.get(), 1);
        assert!(pending.is_none() && requests.is_empty());
        assert!(
            matches!(received.try_recv().unwrap(), ModelUpdate::Read(actual) if actual == WorkspaceModel::default())
        );
        assert!(
            received.try_recv().is_err(),
            "the installed-client refusal must not also emit TitleRefused"
        );
        peer.finished();
    }
}
