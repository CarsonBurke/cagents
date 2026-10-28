use super::{ModelEdit, ModelUpdate, PublicationContext, QueuedEdit, publish_with};
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
            tx.send_blocking(first.into()).unwrap();
            tx.send_blocking(second.into()).unwrap();
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
    tx.send_blocking(edit.into()).unwrap();
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
            tx.send_blocking(reopen(name).into()).unwrap();
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
            tx.send_blocking(reopen("remote").into()).unwrap();
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
    tx.send_blocking(first.clone().into()).unwrap();
    tx.send_blocking(second.into()).unwrap();
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
    assert_eq!(pending.as_ref().map(|queued| &queued.edit), Some(&first));
    assert!(pending.as_ref().unwrap().reopen.is_none());
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
        tx.send_blocking(edit.into()).unwrap();
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
    tx.send_blocking(edit.into()).unwrap();
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
    async_channel::Receiver<QueuedEdit>,
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
        seeds.push(seed.edit);
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
    let edit = queued.try_recv().unwrap().edit;
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
    let new_edit = queued.try_recv().unwrap().edit;
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
    app.workspace_model_tx
        .send_blocking(edit.clone().into())
        .unwrap();
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
        tx.send_blocking(edit.into()).unwrap();
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

/// A real public Create action with temporary worktree-protection files and
/// no store, provider, daemon or executed UI task.
fn creation_fixture() -> (
    tempfile::TempDir,
    crate::app::Cagents,
    crate::model::ProjectId,
    ChannelId,
    PathBuf,
    async_channel::Receiver<QueuedEdit>,
) {
    use crate::{
        app::channel_ui::Action,
        model::{ProjectId, RepositoryId},
        sidebar::SidebarProject,
    };
    let directory = tempfile::tempdir().unwrap();
    let repository = directory.path().canonicalize().unwrap();
    let runtime = directory.path().join("runtime");
    std::fs::create_dir(&runtime).unwrap();
    let mut app = crate::app::tests::context_inspector_test_app(true);
    assert!(app.session.is_none());
    app.worktree_protections.use_fixture_runtime(runtime);
    let project = ProjectId::new();
    app.sidebar_model.projects.push(
        SidebarProject::new(
            project,
            RepositoryId::new(),
            "private fixture".into(),
            repository.clone(),
            Vec::new(),
            Vec::new(),
        )
        .with_kind(ProjectKind::Folder),
    );
    let (tx, requests) = async_channel::unbounded();
    app.workspace_model_tx = tx;
    app.status = None;
    // Call the actual UI action. Returned tasks are not executed: this
    // checks local creation/publication and starts no provider or daemon.
    drop(app.handle_channel_ui(Action::Create(project)));
    let channel = app.durable.active_channel.expect("new channel opens");
    (directory, app, project, channel, repository, requests)
}

fn write_creation_draft(app: &mut crate::app::Cagents, card: CardId, text: &str) {
    let target = match app.cards.get(&card).unwrap() {
        crate::app::CardRuntime::Chat(runtime) => runtime.target,
        _ => panic!("the new channel opens its chat composer"),
    };
    drop(app.update(crate::app::Message::ChatUi(
        crate::app::chat_ui::Action::Composer(
            target,
            cosmic::iced::widget::text_editor::Action::Edit(
                cosmic::iced::widget::text_editor::Edit::Paste(std::sync::Arc::new(
                    text.to_owned(),
                )),
            ),
        ),
    )));
}

/// Public creation reports permanent sharing refusal without losing local work.
/// Private peers do not issue real Host capabilities or run native providers.
#[test]
fn public_channel_creation_reports_permanent_publication_refusal_without_losing_local_work() {
    for authority in [
        Authority::session("fixture-valid-create-scope".into()),
        Authority::session("fixture-invalid-create-scope".into()),
        Authority::session(String::new()),
    ] {
        let (_directory, mut app, project, channel, repository, requests) = creation_fixture();
        let local_before = app
            .durable
            .channels
            .iter()
            .find(|local| local.id == channel)
            .unwrap()
            .clone();
        let draft_card = local_before.cards[0];
        write_creation_draft(&mut app, draft_card, "Unsaved private fixture draft");
        let drafts_before = app.durable.chat_drafts.clone();
        assert_eq!(
            drafts_before.get(&draft_card).map(String::as_str),
            Some("Unsaved private fixture draft")
        );
        assert!(!local_before.shared);
        assert!(
            app.status.is_none(),
            "creation and drafting must have succeeded"
        );
        assert_eq!(
            requests.len(),
            2,
            "public creation queues project then channel"
        );
        let expected = [
            ModelEdit::OpenProject {
                kind: ProjectKind::Folder,
                repository: repository.clone(),
                name: "private fixture".into(),
            },
            ModelEdit::CreateChannel {
                id: channel,
                repository,
                title: String::new(),
                title_source: Default::default(),
            },
        ];
        let expected_authority = authority.clone();
        let accept_auth = authority.token() == Some("fixture-valid-create-scope");
        let peer = Peer::new(vec![connected(), connected()], move |index, peer| {
            let request = next(peer);
            assert!(matches!(
                &request.request,
                ClientRequest::Authenticate { authority } if authority == &expected_authority
            ));
            let refusal = || {
                ServerMessage::Error(ProtocolError::new(
                    ProtocolErrorCode::PermissionDenied,
                    format!(
                        "private fixture echoed {}",
                        expected_authority.token().unwrap_or_default()
                    ),
                    false,
                ))
            };
            if accept_auth {
                accepted(peer, &request);
                let request = expect_edit(peer, &expected[index]);
                response(peer, &request, refusal());
            } else {
                response(peer, &request, refusal());
            }
            true
        });
        let private_token = authority.token().unwrap_or_default().to_owned();
        let context = PublicationContext {
            authority,
            remote: false,
            no_start: false,
        };
        let (updates, received) = async_channel::unbounded();
        let mut pending = None;
        let connections = Cell::new(0);
        for remaining in [1, 0] {
            let error = publish_with(&updates, &requests, &mut pending, || {
                connections.set(connections.get() + 1);
                existing(&context, &peer.socket)
            })
            .unwrap_err();
            assert!(error.contains("PermissionDenied"));
            assert!(pending.is_none());
            assert_eq!(requests.len(), remaining);
        }
        assert_eq!(connections.get(), 2);
        peer.finished();
        // Feed every real update through the owning reducer. Neither a false
        // success acknowledgement nor destruction of unsaved local work is OK.
        for key in [
            crate::app::NoticeKey::SharedProject(project),
            crate::app::NoticeKey::SharedChannel(channel, project),
        ] {
            let update = received
                .try_recv()
                .expect("each refused public edit reports its target");
            assert!(matches!(&update, ModelUpdate::PublicationFailed { .. }));
            drop(app.apply_workspace_model_update(update));
            let notice = app.status.as_ref().unwrap();
            assert_eq!(notice.key, Some(key));
            assert!(notice.details.as_deref().unwrap().contains("own terminal"));
            if !private_token.is_empty() {
                assert!(!notice.message.contains(&private_token));
                assert!(!notice.details.as_deref().unwrap().contains(&private_token));
            }
            app.notices.record(app.status.take().unwrap());
        }
        assert!(received.try_recv().is_err());
        assert_eq!(app.durable.chat_drafts, drafts_before);
        assert_eq!(
            app.durable
                .channels
                .iter()
                .find(|local| local.id == channel),
            Some(&local_before)
        );
        assert_eq!(app.durable.active_channel, Some(channel));
        assert!(
            app.notice_stands(crate::app::NoticeKey::SharedChannel(channel, project)),
            "permanent refusal of OpenProject/CreateChannel must reach the person; a debug log cannot explain why their visible channel is not shared"
        );
    }
}

#[test]
fn confirmed_publication_resolves_its_notice_without_replaying_or_losing_the_local_draft() {
    use crate::app::{NoticeKey, TransientNotice};
    use crate::workspace_model::{ModelChannel, ModelProject};
    let (_directory, mut app, project, channel, repository, requests) = creation_fixture();
    let queued = [
        requests.try_recv().unwrap().edit,
        requests.try_recv().unwrap().edit,
    ];
    assert!(requests.try_recv().is_err());
    let draft_card = app
        .durable
        .channels
        .iter()
        .find(|local| local.id == channel)
        .unwrap()
        .cards[0];
    write_creation_draft(&mut app, draft_card, "Keep this draft");
    let drafts_before = app.durable.chat_drafts.clone();
    for edit in &queued {
        drop(
            app.apply_workspace_model_update(ModelUpdate::PublicationFailed { edit: edit.clone() }),
        );
        app.notices.record(app.status.take().unwrap());
    }
    // Repeated refusal replaces the channel's keyed report; unrelated notices
    // survive a later successful acknowledgement for this channel.
    drop(
        app.apply_workspace_model_update(ModelUpdate::PublicationFailed {
            edit: queued[1].clone(),
        }),
    );
    app.notices.record(app.status.take().unwrap());
    app.notices.record(
        TransientNotice::warning("Unrelated visibility failure")
            .with_key(NoticeKey::SurfaceVisibility),
    );
    assert_eq!(app.notices.entries.len(), 3);
    let confirmed = WorkspaceModel {
        identity: uuid::Uuid::new_v4(),
        sequence: 9,
        projects: vec![ModelProject {
            repository: repository.clone(),
            name: "private fixture".into(),
            kind: ProjectKind::Folder,
            ordinal: 1,
        }],
        channels: vec![ModelChannel {
            id: channel,
            repository,
            title: String::new(),
            title_source: Default::default(),
            ordinal: 9,
        }],
        ..WorkspaceModel::default()
    };
    let reply_model = confirmed.clone();
    let expected = queued.clone();
    let peer = Peer::new(vec![connected()], move |_, peer| {
        for edit in &expected {
            let request = expect_edit(peer, edit);
            response(
                peer,
                &request,
                ServerMessage::Response {
                    response: ServerResponse::WorkspaceModel {
                        model: reply_model.clone(),
                    },
                },
            );
        }
        true
    });
    // A distinct admitted publication is acknowledged through the real User
    // connector/send path. The failure handler itself enqueued no retry.
    assert!(requests.is_empty());
    let (tx, admitted) = async_channel::unbounded();
    for edit in &queued {
        tx.send_blocking(edit.clone().into()).unwrap();
    }
    drop(tx);
    let context = PublicationContext {
        authority: Authority::default(),
        remote: false,
        no_start: true,
    };
    let (updates, received) = async_channel::unbounded();
    let mut pending = None;
    let connections = Cell::new(0);
    let error = publish_with(&updates, &admitted, &mut pending, || {
        connections.set(connections.get() + 1);
        existing(&context, &peer.socket)
    })
    .unwrap_err();
    assert_eq!(error, "the window stopped publishing");
    assert_eq!(connections.get(), 1);
    assert!(pending.is_none() && admitted.is_empty());
    peer.finished();
    let update = received.try_recv().unwrap();
    assert!(
        matches!(&update, ModelUpdate::Ranked { channel: id, ordinal: 9, .. } if *id == channel)
    );
    drop(app.apply_workspace_model_update(update));
    assert!(received.try_recv().is_err());
    assert!(!app.notice_stands(NoticeKey::SharedChannel(channel, project)));
    assert!(
        app.notice_stands(NoticeKey::SharedProject(project)),
        "channel ACK does not establish project snapshot visibility"
    );
    // An actual authoritative model observation settles the matching project.
    drop(app.apply_workspace_model_update(ModelUpdate::Connected(confirmed)));
    // A late refusal for the old request cannot reopen a settled notice.
    for edit in queued {
        drop(app.apply_workspace_model_update(ModelUpdate::PublicationFailed { edit }));
    }
    assert!(!app.notice_stands(NoticeKey::SharedProject(project)));
    assert!(app.notice_stands(NoticeKey::SurfaceVisibility));
    assert_eq!(app.durable.chat_drafts, drafts_before);
    assert_eq!(app.durable.active_channel, Some(channel));
}

#[test]
fn local_retirement_resolves_publication_notice_without_waiting_for_another_model_update() {
    use crate::app::{Message, NoticeKey};
    let (_directory, mut app, project, channel, _repository, requests) = creation_fixture();
    let project_edit = requests.try_recv().unwrap().edit;
    let channel_edit = requests.try_recv().unwrap().edit;
    assert!(requests.try_recv().is_err());
    drop(app.apply_workspace_model_update(ModelUpdate::PublicationFailed { edit: channel_edit }));
    app.notices.record(app.status.take().unwrap());
    let key = NoticeKey::SharedChannel(channel, project);
    assert!(app.notice_stands(key));
    // Retire the unstarted default through its owning synchronous chat-close
    // path before testing deletion of an actually empty channel. Returned
    // teardown tasks are not executed; this fixture has no issued actor.
    let card = app
        .durable
        .channels
        .iter()
        .find(|local| local.id == channel)
        .unwrap()
        .cards[0];
    assert!(matches!(&app.card_state(card).unwrap().kind,
        crate::model::CardKind::Chat(session) if session.is_unstarted() && session.proof.is_none()));
    drop(app.begin_chat_close(card));
    assert!(
        app.durable
            .channels
            .iter()
            .find(|local| local.id == channel)
            .unwrap()
            .cards
            .is_empty()
    );
    // An empty channel's public Delete action retires it locally. Its scoped
    // DeleteChannel can itself be refused with no model callback, so the real
    // all-update notice sweep must settle this report before any model arrives.
    drop(
        app.update(Message::ChannelUi(crate::app::channel_ui::Action::Delete(
            channel,
        ))),
    );
    assert!(app.durable.channels.iter().all(|local| local.id != channel));
    assert!(!app.notice_stands(key));
    // No registry callback or successful sharing receipt was needed for cleanup.
    assert!(matches!(project_edit, ModelEdit::OpenProject { .. }));
    let mut deleted = false;
    while let Ok(QueuedEdit { edit, .. }) = requests.try_recv() {
        deleted |= matches!(edit, ModelEdit::DeleteChannel { id } if id == channel);
    }
    assert!(deleted);
}

#[test]
fn publication_notice_does_not_follow_same_id_into_a_successor_project() {
    use crate::app::{Message, NoticeKey};
    use crate::model::{ProjectId, RepositoryId};
    let (_directory, mut app, project, channel, repository, requests) = creation_fixture();
    let _ = requests.try_recv().unwrap().edit;
    let old_edit = requests.try_recv().unwrap().edit;
    assert!(requests.try_recv().is_err());
    drop(
        app.apply_workspace_model_update(ModelUpdate::PublicationFailed {
            edit: old_edit.clone(),
        }),
    );
    app.notices.record(app.status.take().unwrap());
    let successor = ProjectId::new();
    app.sidebar_model
        .projects
        .push(crate::sidebar::SidebarProject::new(
            successor,
            RepositoryId::new(),
            "successor".into(),
            repository.join("successor"),
            Vec::new(),
            Vec::new(),
        ));
    std::sync::Arc::make_mut(&mut app.durable)
        .channels
        .iter_mut()
        .find(|local| local.id == channel)
        .unwrap()
        .project_id = successor;
    // The ordinary all-update completion sweep sees retirement of the original
    // local association; no synthetic successful registry receipt is involved.
    drop(app.update(Message::WindowResized(900.0, 700.0)));
    assert!(!app.notice_stands(NoticeKey::SharedChannel(channel, project)));
    assert!(!app.notice_stands(NoticeKey::SharedChannel(channel, successor)));
    drop(app.apply_workspace_model_update(ModelUpdate::PublicationFailed { edit: old_edit }));
    assert!(!app.notice_stands(NoticeKey::SharedChannel(channel, successor)));
    assert!(
        app.durable
            .channels
            .iter()
            .any(|local| local.id == channel && local.project_id == successor && !local.shared)
    );
}

/// Establish a known registry before an explicit local publication. Discard
/// only bootstrap requests from creation_fixture; no provider task is run.
fn publication_reconciliation_fixture() -> (
    tempfile::TempDir,
    crate::app::Cagents,
    crate::model::ProjectId,
    ChannelId,
    PathBuf,
    async_channel::Receiver<QueuedEdit>,
    WorkspaceModel,
) {
    let (directory, mut app, project, channel, repository, requests) = creation_fixture();
    while requests.try_recv().is_ok() {}
    let canonical = WorkspaceModel {
        identity: uuid::Uuid::new_v4(),
        sequence: 7,
        ..WorkspaceModel::default()
    };
    app.workspace_model = canonical.clone();
    std::sync::Arc::make_mut(&mut app.durable).workspace_model_identity = Some(canonical.identity);
    let card = app
        .durable
        .channels
        .iter()
        .find(|local| local.id == channel)
        .unwrap()
        .cards[0];
    write_creation_draft(&mut app, card, "Keep the refused local conversation draft");
    while requests.try_recv().is_ok() {}
    app.publish_channel_creation(channel, project);
    (
        directory, app, project, channel, repository, requests, canonical,
    )
}

fn take_creation_publications(requests: &async_channel::Receiver<QueuedEdit>) -> Vec<ModelEdit> {
    let mut creations = Vec::new();
    while let Ok(QueuedEdit { edit, .. }) = requests.try_recv() {
        if matches!(
            edit,
            ModelEdit::OpenProject { .. } | ModelEdit::CreateChannel { .. }
        ) {
            creations.push(edit);
        }
    }
    creations
}

/// Real publisher connection refusal, real PublicationFailed updates, and the
/// ordinary app notice drain. Scoped remote refusal invokes no resolver/peer.
fn refuse_creation_publications(
    app: &mut crate::app::Cagents,
    requests: &async_channel::Receiver<QueuedEdit>,
) {
    use crate::app::Message;
    let context = PublicationContext {
        authority: Authority::session("fixture-refused-reconciliation".into()),
        remote: true,
        no_start: true,
    };
    let (updates, received) = async_channel::unbounded();
    let mut pending = None;
    for _ in 0..2 {
        assert!(!requests.is_empty());
        let error = publish_with(&updates, requests, &mut pending, || {
            context.connect_with(
                || panic!("scoped remote refusal must precede socket lookup"),
                || panic!("scoped remote refusal must not start a daemon"),
                |_| panic!("scoped remote refusal must not connect"),
            )
        })
        .unwrap_err();
        assert!(error.contains("own terminal"));
        assert!(pending.is_none());
        let update = received.try_recv().unwrap();
        assert!(matches!(update, ModelUpdate::PublicationFailed { .. }));
        drop(app.update(Message::WorkspaceModelObserved(update)));
    }
    assert!(requests.is_empty() && received.is_empty());
}

#[test]
fn unacknowledged_publication_is_coalesced_across_equal_and_newer_reads() {
    use crate::app::Message;
    let (_directory, mut app, _project, _channel, _repository, requests, mut canonical) =
        publication_reconciliation_fixture();
    let in_flight = take_creation_publications(&requests);
    assert_eq!(
        in_flight.len(),
        2,
        "one original project/channel publication"
    );
    // Consuming the queued edit is not its ACK. Equal snapshots and unrelated
    // model sequence changes must not create additional physical requests.
    for sequence in [7, 7, 8, 9] {
        canonical.sequence = sequence;
        drop(
            app.update(Message::WorkspaceModelObserved(ModelUpdate::Read(
                canonical.clone(),
            ))),
        );
        assert!(
            take_creation_publications(&requests).is_empty(),
            "sequence {sequence}: the original publication is still in flight"
        );
    }
}

#[test]
fn permanent_publication_refusal_stays_local_across_reads_and_notice_dismissal() {
    use crate::app::{Message, NoticeKey};
    let (_directory, mut app, project, channel, _repository, requests, mut canonical) =
        publication_reconciliation_fixture();
    let channels = app.durable.channels.clone();
    let drafts = app.durable.chat_drafts.clone();
    refuse_creation_publications(&mut app, &requests);
    let keys = [
        NoticeKey::SharedProject(project),
        NoticeKey::SharedChannel(channel, project),
    ];
    assert!(keys.iter().all(|key| app.notice_stands(*key)));
    let notice_ids = app
        .notices
        .entries
        .iter()
        .filter_map(|entry| {
            entry
                .notice
                .key
                .filter(|key| keys.contains(key))
                .map(|_| entry.id)
        })
        .collect::<Vec<_>>();
    for id in notice_ids {
        drop(app.update(Message::DismissNotice(id)));
    }
    assert!(keys.iter().all(|key| !app.notice_stands(*key)));
    for sequence in [7, 7, 8, 9] {
        canonical.sequence = sequence;
        drop(
            app.update(Message::WorkspaceModelObserved(ModelUpdate::Read(
                canonical.clone(),
            ))),
        );
        assert!(
            take_creation_publications(&requests).is_empty(),
            "sequence {sequence}: permanent refusal is not a new user publication intent"
        );
        assert!(
            keys.iter().all(|key| !app.notice_stands(*key)),
            "ordinary observations must not resurrect dismissed refusal reports"
        );
        assert_eq!(app.durable.channels, channels);
        assert_eq!(app.durable.chat_drafts, drafts);
        assert_eq!(app.durable.active_channel, Some(channel));
    }
}

#[test]
fn refused_publication_settles_on_ack_and_republishes_into_a_replacement_registry() {
    use crate::app::{Message, NoticeKey};
    use crate::workspace_model::{ModelChannel, ModelProject};
    let (_directory, mut app, project, channel, repository, requests, mut canonical) =
        publication_reconciliation_fixture();
    refuse_creation_publications(&mut app, &requests);
    let drafts = app.durable.chat_drafts.clone();
    canonical.sequence += 1;
    canonical.projects.push(ModelProject {
        repository: repository.clone(),
        name: "Confirmed project".into(),
        kind: ProjectKind::Folder,
        ordinal: 1,
    });
    canonical.channels.push(ModelChannel {
        id: channel,
        repository: repository.clone(),
        title: String::new(),
        title_source: Default::default(),
        ordinal: 1,
    });
    drop(
        app.update(Message::WorkspaceModelObserved(ModelUpdate::Read(
            canonical,
        ))),
    );
    assert!(!app.notice_stands(NoticeKey::SharedProject(project)));
    assert!(!app.notice_stands(NoticeKey::SharedChannel(channel, project)));
    assert!(take_creation_publications(&requests).is_empty());
    let replacement = WorkspaceModel {
        identity: uuid::Uuid::new_v4(),
        sequence: 1,
        ..WorkspaceModel::default()
    };
    drop(
        app.update(Message::WorkspaceModelObserved(ModelUpdate::Connected(
            replacement,
        ))),
    );
    let republished = take_creation_publications(&requests);
    assert_eq!(
        republished.len(),
        2,
        "a genuinely replaced registry needs one migration"
    );
    assert!(
        matches!(&republished[0], ModelEdit::OpenProject { repository: actual, .. } if actual == &repository)
    );
    assert!(
        matches!(&republished[1], ModelEdit::CreateChannel { id, repository: actual, .. } if *id == channel && actual == &repository)
    );
    assert_eq!(app.durable.chat_drafts, drafts);
}

#[test]
fn refused_publication_does_not_block_a_successor_or_new_explicit_channel() {
    use crate::app::{Message, NoticeKey};
    use crate::model::{ProjectId, RepositoryId};
    let (_directory, mut app, project, channel, repository, requests, canonical) =
        publication_reconciliation_fixture();
    refuse_creation_publications(&mut app, &requests);
    let drafts = app.durable.chat_drafts.clone();
    let successor = ProjectId::new();
    let successor_repository = repository.join("successor");
    std::fs::create_dir(&successor_repository).unwrap();
    app.sidebar_model.projects.push(
        crate::sidebar::SidebarProject::new(
            successor,
            RepositoryId::new(),
            "Successor".into(),
            successor_repository.clone(),
            Vec::new(),
            Vec::new(),
        )
        .with_kind(ProjectKind::Folder),
    );
    std::sync::Arc::make_mut(&mut app.durable)
        .channels
        .iter_mut()
        .find(|local| local.id == channel)
        .unwrap()
        .project_id = successor;
    drop(app.update(Message::WindowResized(900.0, 700.0)));
    assert!(!app.notice_stands(NoticeKey::SharedChannel(channel, project)));
    drop(
        app.update(Message::WorkspaceModelObserved(ModelUpdate::Read(
            canonical,
        ))),
    );
    let republished = take_creation_publications(&requests);
    assert_eq!(
        republished.len(),
        2,
        "a changed project association owns a fresh publication"
    );
    assert!(
        matches!(&republished[1], ModelEdit::CreateChannel { id, repository: actual, .. } if *id == channel && actual == &successor_repository)
    );
    drop(
        app.update(Message::ChannelUi(crate::app::channel_ui::Action::Create(
            successor,
        ))),
    );
    let new_channel = app.durable.active_channel.unwrap();
    assert_ne!(new_channel, channel);
    let explicit = take_creation_publications(&requests);
    assert_eq!(
        explicit.len(),
        2,
        "a new explicit channel must still publish its project and channel"
    );
    assert!(
        matches!(&explicit[1], ModelEdit::CreateChannel { id, repository: actual, .. } if *id == new_channel && actual == &successor_repository)
    );
    assert_eq!(app.durable.chat_drafts, drafts);
}

#[test]
fn refused_title_with_failed_canonical_read_restores_only_its_still_owned_candidate() {
    use crate::model::{ChannelTitleSource, ProjectId, RepositoryId};
    for authority in [
        Authority::default(),
        Authority::session("fixture-title-read-failure".into()),
    ] {
        for disconnect in [false, true] {
            for changed in ["unchanged", "newer_candidate", "newer_scope"] {
                let (mut app, edit, queued) =
                    title_fixture("Confirmed", ChannelTitleSource::Prompt);
                let expected_edit = edit.clone();
                let expected_authority = authority.clone();
                let peer = Peer::new(vec![connected()], move |_, peer| {
                    if expected_authority.token().is_some() {
                        authenticate(peer, &expected_authority);
                    }
                    let request = expect_edit(peer, &expected_edit);
                    response(peer, &request, refused());
                    let read = next(peer);
                    assert!(
                        matches!(read.request, ClientRequest::ReadWorkspaceModel),
                        "try the original admitted connection before using cached title evidence"
                    );
                    if disconnect {
                        // Drop after observing the read, so this tests failed
                        // recovery after a definite edit refusal, not an
                        // ambiguous edit that may have been accepted.
                        false
                    } else {
                        response(peer, &read, refused());
                        true
                    }
                });
                let context = PublicationContext {
                    authority: authority.clone(),
                    remote: false,
                    no_start: true,
                };
                let (tx, requests) = async_channel::unbounded();
                tx.send_blocking(edit.clone().into()).unwrap();
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
                assert_eq!(
                    connections.get(),
                    1,
                    "no fresh or unauthenticated recovery connection"
                );
                assert!(pending.is_none() && requests.is_empty());
                peer.finished();
                let refusal = received.try_recv().expect(
                    "a permanently refused title still needs a reducer outcome when its canonical read fails"
                );
                assert!(matches!(&refusal, ModelUpdate::TitleRefused(actual) if actual == &edit));
                assert!(
                    received.is_empty(),
                    "one targeted fallback, no false canonical snapshot"
                );

                let unrelated = app.durable.channels[1].clone();
                let card = app.durable.channels[0].cards[0];
                if changed == "newer_candidate" {
                    assert!(app.suggest_channel_title(
                        card,
                        "Newer candidate".into(),
                        ChannelTitleSource::Provider
                    ));
                    assert!(matches!(queued.try_recv().unwrap().edit,
                        ModelEdit::SuggestConversationTitle { title, .. } if title == "Newer candidate"));
                } else if changed == "newer_scope" {
                    let successor = ProjectId::new();
                    app.sidebar_model
                        .projects
                        .push(crate::sidebar::SidebarProject::new(
                            successor,
                            RepositoryId::new(),
                            "Successor project".into(),
                            PathBuf::from("/private-publication-fixture/successor"),
                            Vec::new(),
                            Vec::new(),
                        ));
                    std::sync::Arc::make_mut(&mut app.durable).channels[0].project_id = successor;
                }
                let channels_before = app.durable.channels.clone();
                let cards_before = app.durable.cards.clone();
                let model_before = app.workspace_model.clone();
                drop(app.apply_workspace_model_update(refusal));
                if changed == "unchanged" {
                    assert_eq!(app.durable.channels[0].title, "Confirmed");
                    assert_eq!(
                        app.durable.channels[0].title_source,
                        ChannelTitleSource::Prompt
                    );
                    let channel = app.durable.channels[0].id;
                    assert_eq!(
                        app.sidebar_model.projects[0]
                            .channels
                            .iter()
                            .find(|row| row.id == channel)
                            .unwrap()
                            .title,
                        "Confirmed"
                    );
                } else {
                    assert_eq!(
                        app.durable.channels, channels_before,
                        "the old refused request cannot restore over {changed}"
                    );
                }
                assert_eq!(app.durable.channels[1], unrelated);
                assert_eq!(app.durable.cards, cards_before);
                assert_eq!(app.workspace_model, model_before);
                assert!(
                    queued.is_empty(),
                    "cached restoration must not publish a compensating mutation"
                );
            }
        }
    }
}

/// Obtain the actual publisher's local acknowledgement from a framed successful
/// CreateChannel response. Do not construct Ranked: its source metadata is part
/// of the contract these reordered-reducer tests exercise.
fn framed_channel_publication_ack(edit: ModelEdit, model: WorkspaceModel) -> ModelUpdate {
    assert!(matches!(edit, ModelEdit::CreateChannel { .. }));
    let expected = edit.clone();
    let peer = Peer::new(vec![connected()], move |_, peer| {
        let request = expect_edit(peer, &expected);
        response(
            peer,
            &request,
            ServerMessage::Response {
                response: ServerResponse::WorkspaceModel {
                    model: model.clone(),
                },
            },
        );
        true
    });
    let (tx, requests) = async_channel::unbounded();
    tx.send_blocking(edit.into()).unwrap();
    drop(tx);
    let (updates, received) = async_channel::unbounded();
    let context = PublicationContext {
        authority: Authority::default(),
        remote: false,
        no_start: true,
    };
    let mut pending = None;
    let connections = Cell::new(0);
    let error = publish_with(&updates, &requests, &mut pending, || {
        connections.set(connections.get() + 1);
        existing(&context, &peer.socket)
    })
    .unwrap_err();
    assert_eq!(error, "the window stopped publishing");
    assert_eq!(connections.get(), 1);
    assert!(pending.is_none() && requests.is_empty());
    peer.finished();
    let update = received.try_recv().unwrap();
    assert!(matches!(update, ModelUpdate::Ranked { .. }));
    assert!(received.is_empty());
    update
}

fn channel_publication_model(mut model: WorkspaceModel, edit: &ModelEdit) -> WorkspaceModel {
    use crate::workspace_model::{ModelChannel, ModelProject};
    let ModelEdit::CreateChannel {
        id,
        repository,
        title,
        title_source,
    } = edit
    else {
        panic!("expected the local channel publication")
    };
    model.sequence += 1;
    model.projects.push(ModelProject {
        repository: repository.clone(),
        name: "Admitted project".into(),
        kind: ProjectKind::Folder,
        ordinal: 1,
    });
    model.channels.push(ModelChannel {
        id: *id,
        repository: repository.clone(),
        title: title.clone(),
        title_source: *title_source,
        ordinal: 21,
    });
    model
}

#[test]
fn delayed_channel_ack_cannot_turn_replaced_registry_or_project_silence_into_local_deletion() {
    use crate::app::Message;
    use crate::model::{ProjectId, RepositoryId};
    for changed in ["registry", "project"] {
        let (_directory, mut app, _project, channel, repository, requests, canonical) =
            publication_reconciliation_fixture();
        let queued = take_creation_publications(&requests);
        let edit = queued
            .into_iter()
            .find(|edit| matches!(edit, ModelEdit::CreateChannel { .. }))
            .unwrap();
        let admitted = channel_publication_model(canonical.clone(), &edit);
        let delayed_ack = framed_channel_publication_ack(edit, admitted);
        let card = app
            .durable
            .channels
            .iter()
            .find(|local| local.id == channel)
            .unwrap()
            .cards[0];
        let original_kind = app.card_state(card).unwrap().kind.clone();
        let drafts = app.durable.chat_drafts.clone();
        let mut current = canonical;
        if changed == "registry" {
            current.identity = uuid::Uuid::new_v4();
            current.sequence = 1;
        } else {
            let successor = ProjectId::new();
            let successor_repository = repository.join("ack-successor");
            std::fs::create_dir(&successor_repository).unwrap();
            app.sidebar_model.projects.push(
                crate::sidebar::SidebarProject::new(
                    successor,
                    RepositoryId::new(),
                    "Successor project".into(),
                    successor_repository,
                    Vec::new(),
                    Vec::new(),
                )
                .with_kind(ProjectKind::Folder),
            );
            std::sync::Arc::make_mut(&mut app.durable)
                .channels
                .iter_mut()
                .find(|local| local.id == channel)
                .unwrap()
                .project_id = successor;
        }
        // Observe the real replacement authority before delivering the old
        // successful publisher result. No provider or teardown task is polled.
        drop(
            app.update(Message::WorkspaceModelObserved(ModelUpdate::Connected(
                current.clone(),
            ))),
        );
        let ordinal = app
            .durable
            .channels
            .iter()
            .find(|local| local.id == channel)
            .unwrap()
            .ordinal;
        drop(app.update(Message::WorkspaceModelObserved(delayed_ack)));
        drop(
            app.update(Message::WorkspaceModelObserved(ModelUpdate::Read(
                current.clone(),
            ))),
        );
        assert!(
            !app.channel_ui.is_deleting(channel) && !app.channel_ui.was_deleted(channel),
            "{changed}: a stale ACK must not schedule retirement under the current authority"
        );
        let local = app
            .durable
            .channels
            .iter()
            .find(|local| local.id == channel)
            .expect("an absent current-registry channel is still unshared local work");
        assert!(
            !local.shared,
            "{changed}: only the current publication may establish shared custody"
        );
        assert_eq!(
            local.ordinal, ordinal,
            "{changed}: an old reply must not rerank the successor"
        );
        assert!(local.cards.contains(&card));
        assert!(
            app.card_state(card).unwrap().existence.is_open(),
            "{changed}: retain the conversation"
        );
        assert_eq!(app.card_state(card).unwrap().kind, original_kind);
        assert!(app.cards.contains_key(&card));
        assert_eq!(app.persistent_workspace().chat_drafts, drafts);
        assert_eq!(app.durable.active_channel, Some(channel));
        assert_eq!(app.workspace_model, current);
    }
}

#[test]
fn current_channel_ack_accepts_matching_registry_and_bootstrap_before_first_connection() {
    use crate::app::Message;
    for bootstrap in [false, true] {
        let (_directory, mut app, _project, channel, _repository, requests, canonical) =
            if bootstrap {
                let (directory, mut app, project, channel, repository, requests) =
                    creation_fixture();
                let card = app
                    .durable
                    .channels
                    .iter()
                    .find(|local| local.id == channel)
                    .unwrap()
                    .cards[0];
                write_creation_draft(&mut app, card, "Keep the initial publication draft");
                assert_eq!(
                    app.workspace_model.identity,
                    crate::workspace_model::NO_REGISTRY
                );
                let canonical = WorkspaceModel {
                    identity: uuid::Uuid::new_v4(),
                    sequence: 7,
                    ..WorkspaceModel::default()
                };
                (
                    directory, app, project, channel, repository, requests, canonical,
                )
            } else {
                publication_reconciliation_fixture()
            };
        let queued = take_creation_publications(&requests);
        let edit = queued
            .into_iter()
            .find(|edit| matches!(edit, ModelEdit::CreateChannel { .. }))
            .unwrap();
        let admitted = channel_publication_model(canonical, &edit);
        let ack = framed_channel_publication_ack(edit, admitted.clone());
        let drafts = app.durable.chat_drafts.clone();
        drop(app.update(Message::WorkspaceModelObserved(ack)));
        let local = app
            .durable
            .channels
            .iter()
            .find(|local| local.id == channel)
            .unwrap();
        assert!(
            local.shared,
            "bootstrap {bootstrap}: a current real ACK establishes sharing"
        );
        assert_eq!(local.ordinal, 21);
        drop(
            app.update(Message::WorkspaceModelObserved(ModelUpdate::Connected(
                admitted.clone(),
            ))),
        );
        drop(app.update(Message::WorkspaceModelObserved(ModelUpdate::Read(admitted))));
        let local = app
            .durable
            .channels
            .iter()
            .find(|local| local.id == channel)
            .unwrap();
        assert!(local.shared && !local.cards.is_empty());
        assert!(!app.channel_ui.is_deleting(channel) && !app.channel_ui.was_deleted(channel));
        assert!(
            local
                .cards
                .iter()
                .all(|card| app.card_state(*card).unwrap().existence.is_open())
        );
        assert_eq!(app.persistent_workspace().chat_drafts, drafts);
        assert_eq!(app.durable.active_channel, Some(channel));
    }
}

/// Keep the actual publisher callback from one successful model-bearing edit.
/// A navigation reply emits Read; an explicit reopen emits Reopened.
fn enqueue_model_callback(
    app: &mut crate::app::Cagents,
    requests: &async_channel::Receiver<QueuedEdit>,
    edit: ModelEdit,
) -> QueuedEdit {
    if let ModelEdit::ReopenProject { repository, .. } = &edit {
        let project = app
            .sidebar_model
            .projects
            .iter()
            .find(|project| project.repository == *repository)
            .unwrap()
            .id;
        if !app.project_ui.is_reopening(repository)
            || !app.workspace_model_reopens.active.contains_key(&project)
        {
            app.project_ui.reopening.remove(&project);
            drop(app.project_load_task(
                repository.clone(),
                Some(project),
                true,
                Some(ProjectKind::Folder),
            ));
        }
    }
    app.publish_model_edit(edit.clone());
    loop {
        let queued = requests
            .try_recv()
            .expect("the real enqueue must publish its edit");
        if queued.edit == edit {
            if matches!(edit, ModelEdit::ReopenProject { .. }) {
                assert!(
                    queued.reopen.is_some(),
                    "the origin is captured before publication"
                );
            }
            return queued;
        }
    }
}

fn framed_model_publication_callback(edit: QueuedEdit, model: WorkspaceModel) -> ModelUpdate {
    let expected = edit.edit.clone();
    let peer = Peer::new(vec![connected()], move |_, peer| {
        let request = expect_edit(peer, &expected);
        response(
            peer,
            &request,
            ServerMessage::Response {
                response: ServerResponse::WorkspaceModel {
                    model: model.clone(),
                },
            },
        );
        true
    });
    let context = PublicationContext {
        authority: Authority::default(),
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
    assert_eq!(error, "the window stopped publishing");
    assert_eq!(connections.get(), 1);
    assert!(pending.is_none() && requests.is_empty());
    peer.finished();
    let callback = received.try_recv().unwrap();
    assert!(matches!(
        callback,
        ModelUpdate::Read(_) | ModelUpdate::Reopened { .. }
    ));
    assert!(received.is_empty());
    callback
}

#[test]
fn old_registry_model_reply_cannot_rebind_or_settle_replacement_workspace_intent() {
    use crate::app::Message;
    for reopened in [false, true] {
        let (_directory, mut app, project, channel, repository, requests, canonical) =
            publication_reconciliation_fixture();
        let create = take_creation_publications(&requests)
            .into_iter()
            .find(|edit| matches!(edit, ModelEdit::CreateChannel { .. }))
            .unwrap();
        let old_model = channel_publication_model(canonical, &create);
        let edit = if reopened {
            ModelEdit::ReopenProject {
                repository: repository.clone(),
                name: "Old reopen".into(),
                kind: ProjectKind::Folder,
            }
        } else {
            ModelEdit::ResetNavigation {
                repository: repository.clone(),
            }
        };
        let queued = enqueue_model_callback(&mut app, &requests, edit);
        let delayed = framed_model_publication_callback(queued, old_model);
        assert_eq!(matches!(&delayed, ModelUpdate::Reopened { .. }), reopened);
        let replacement = WorkspaceModel {
            identity: uuid::Uuid::new_v4(),
            sequence: 1,
            ..WorkspaceModel::default()
        };
        // Only the observer's Connected callback introduces the replacement.
        // Its ordinary migration request remains pending throughout this race.
        drop(
            app.update(Message::WorkspaceModelObserved(ModelUpdate::Connected(
                replacement.clone(),
            ))),
        );
        app.project_ui.closed.insert(repository.clone());
        app.project_ui.reopening.insert(project, repository.clone());
        let closed = app.project_ui.closed.clone();
        let reopening = app.project_ui.reopening.clone();
        let publications = app.workspace_model_publications.clone();
        let queued = requests.len();
        let channels = app.durable.channels.clone();
        let cards = app.durable.cards.clone();
        let drafts = app.durable.chat_drafts.clone();
        let focused = app.workspace.focused();
        let active_channel = app.durable.active_channel;

        drop(app.update(Message::WorkspaceModelObserved(delayed)));
        let identity_after_reply = app.workspace_model.identity;
        let durable_identity_after_reply = app.durable.workspace_model_identity;
        let closed_after_reply = app.project_ui.closed.clone();
        let reopening_after_reply = app.project_ui.reopening.clone();
        let publications_after_reply = app.workspace_model_publications.clone();
        // Execute the real newer-authority absence path too: the old callback
        // must not have promoted this local channel into shared deletion scope.
        drop(
            app.update(Message::WorkspaceModelObserved(ModelUpdate::Read(
                replacement.clone(),
            ))),
        );
        assert_eq!(
            identity_after_reply, replacement.identity,
            "reopened {reopened}: old callback cannot replace current registry"
        );
        assert_eq!(
            durable_identity_after_reply,
            Some(replacement.identity),
            "reopened {reopened}: old callback cannot roll back durable registry custody"
        );
        assert_eq!(closed_after_reply, closed);
        assert_eq!(
            reopening_after_reply, reopening,
            "an old reopen is not the replacement registry's pending intent"
        );
        assert_eq!(publications_after_reply, publications);
        assert_eq!(app.workspace_model, replacement);
        assert_eq!(
            app.durable.workspace_model_identity,
            Some(app.workspace_model.identity)
        );
        assert_eq!(app.project_ui.closed, closed);
        assert_eq!(app.project_ui.reopening, reopening);
        assert_eq!(app.workspace_model_publications, publications);
        assert_eq!(
            requests.len(),
            queued,
            "no replacement migration, replay or compensating retirement"
        );
        assert!(!app.channel_ui.is_deleting(channel) && !app.channel_ui.was_deleted(channel));
        assert_eq!(app.durable.channels, channels);
        assert!(
            app.durable
                .channels
                .iter()
                .any(|local| local.id == channel && !local.shared)
        );
        assert_eq!(app.durable.cards, cards);
        assert!(
            cards
                .iter()
                .all(|card| app.card_state(card.id).unwrap().existence.is_open())
        );
        assert_eq!(app.persistent_workspace().chat_drafts, drafts);
        assert_eq!(app.workspace.focused(), focused);
        assert_eq!(app.durable.active_channel, active_channel);
    }
}

#[test]
fn current_or_bootstrap_model_reply_preserves_read_and_reopen_acknowledgement_behavior() {
    use crate::app::Message;
    for reopened in [false, true] {
        for bootstrap in [false, true] {
            let (_directory, mut app, project, channel, repository, requests, canonical) =
                if bootstrap {
                    let (directory, mut app, project, channel, repository, requests) =
                        creation_fixture();
                    let card = app
                        .durable
                        .channels
                        .iter()
                        .find(|local| local.id == channel)
                        .unwrap()
                        .cards[0];
                    write_creation_draft(&mut app, card, "Keep bootstrap model-reply draft");
                    assert_eq!(
                        app.workspace_model.identity,
                        crate::workspace_model::NO_REGISTRY
                    );
                    let canonical = WorkspaceModel {
                        identity: uuid::Uuid::new_v4(),
                        sequence: 7,
                        ..WorkspaceModel::default()
                    };
                    (
                        directory, app, project, channel, repository, requests, canonical,
                    )
                } else {
                    publication_reconciliation_fixture()
                };
            let create = take_creation_publications(&requests)
                .into_iter()
                .find(|edit| matches!(edit, ModelEdit::CreateChannel { .. }))
                .unwrap();
            let admitted = channel_publication_model(canonical, &create);
            let edit = if reopened {
                ModelEdit::ReopenProject {
                    repository: repository.clone(),
                    name: "Current reopen".into(),
                    kind: ProjectKind::Folder,
                }
            } else {
                ModelEdit::ResetNavigation {
                    repository: repository.clone(),
                }
            };
            // Set the explicit-open state before enqueueing its receipt. A
            // later close is a different intent and must not be cleared by it.
            app.project_ui.closed.insert(repository.clone());
            if !reopened {
                app.project_ui.reopening.insert(project, repository.clone());
            }
            let queued = enqueue_model_callback(&mut app, &requests, edit);
            let callback = framed_model_publication_callback(queued, admitted.clone());
            let drafts = app.durable.chat_drafts.clone();
            let focused = app.workspace.focused();
            drop(app.update(Message::WorkspaceModelObserved(callback.clone())));
            assert_eq!(app.workspace_model.identity, admitted.identity);
            assert_eq!(app.project_ui.closed.contains(&repository), !reopened);
            assert_eq!(
                app.project_ui.is_reopening(&repository),
                !reopened,
                "only the matching Reopened receipt settles the pending local reopen"
            );
            let local = app
                .durable
                .channels
                .iter()
                .find(|local| local.id == channel)
                .unwrap();
            assert!(local.shared && local.ordinal == 21);
            // Connected remains the legitimate authority boundary. Equal
            // same-registry responses still perform ordinary reconciliation.
            drop(
                app.update(Message::WorkspaceModelObserved(ModelUpdate::Connected(
                    admitted.clone(),
                ))),
            );
            drop(app.update(Message::WorkspaceModelObserved(callback)));
            drop(
                app.update(Message::WorkspaceModelObserved(ModelUpdate::Read(
                    admitted.clone(),
                ))),
            );
            assert_eq!(app.workspace_model, admitted);
            assert_eq!(
                app.durable.workspace_model_identity,
                Some(app.workspace_model.identity)
            );
            assert!(!app.channel_ui.is_deleting(channel) && !app.channel_ui.was_deleted(channel));
            assert!(
                app.durable
                    .channels
                    .iter()
                    .any(|local| local.id == channel && local.shared && !local.cards.is_empty())
            );
            assert_eq!(app.persistent_workspace().chat_drafts, drafts);
            assert_eq!(app.workspace.focused(), focused);
            assert_eq!(app.durable.active_channel, Some(channel));
        }
    }
    // A restored window may have old shared claims before its observer mounts.
    // The first legitimate publisher snapshot must join its registry before
    // projecting absence, just as Connected does. A fresh NO_REGISTRY cache
    // alone does not mean the saved channels were never shared elsewhere.
    for reopened in [false, true] {
        let (_directory, mut app, project, channel, repository, requests, _old_model) =
            publication_reconciliation_fixture();
        take_creation_publications(&requests);
        std::sync::Arc::make_mut(&mut app.durable)
            .channels
            .iter_mut()
            .find(|local| local.id == channel)
            .unwrap()
            .shared = true;
        app.workspace_model = WorkspaceModel::default();
        let mut incoming = WorkspaceModel {
            identity: uuid::Uuid::new_v4(),
            sequence: 1,
            ..WorkspaceModel::default()
        };
        if reopened {
            incoming
                .projects
                .push(crate::workspace_model::ModelProject {
                    repository: repository.clone(),
                    name: "Restored reopened project".into(),
                    kind: ProjectKind::Folder,
                    ordinal: 1,
                });
        }
        assert_ne!(
            app.durable.workspace_model_identity,
            Some(incoming.identity)
        );
        let edit = if reopened {
            app.project_ui.reopening.insert(project, repository.clone());
            ModelEdit::ReopenProject {
                repository: repository.clone(),
                name: "Restored reopen".into(),
                kind: ProjectKind::Folder,
            }
        } else {
            ModelEdit::ResetNavigation {
                repository: repository.clone(),
            }
        };
        let queued = enqueue_model_callback(&mut app, &requests, edit);
        let callback = framed_model_publication_callback(queued, incoming.clone());
        let drafts = app.durable.chat_drafts.clone();
        let cards = app.durable.cards.clone();
        let focused = app.workspace.focused();
        drop(app.update(Message::WorkspaceModelObserved(callback)));
        assert_eq!(app.workspace_model.identity, incoming.identity);
        assert_eq!(
            app.durable.workspace_model_identity,
            Some(incoming.identity),
            "bootstrap reopened {reopened}: join the real incoming registry before projecting its silence"
        );
        assert!(!app.channel_ui.is_deleting(channel) && !app.channel_ui.was_deleted(channel));
        assert!(
            app.durable
                .channels
                .iter()
                .any(|local| local.id == channel && !local.shared && !local.cards.is_empty())
        );
        assert_eq!(app.durable.cards, cards);
        assert_eq!(app.persistent_workspace().chat_drafts, drafts);
        assert_eq!(app.workspace.focused(), focused);
        let migration = take_creation_publications(&requests);
        assert_eq!(
            migration.len(),
            2,
            "bootstrap migration has one project/channel pair"
        );
        drop(app.update(Message::WorkspaceModelObserved(ModelUpdate::Read(incoming))));
        assert!(take_creation_publications(&requests).is_empty());
    }
}

#[test]
fn bootstrap_channel_ack_binds_registry_before_saved_registry_reconnect_and_keeps_terminal_choice()
{
    use crate::app::{CardRuntime, Message};
    let (_directory, mut app, _project, channel, repository, requests) = creation_fixture();
    assert_eq!(
        app.workspace_model.identity,
        crate::workspace_model::NO_REGISTRY
    );
    let saved_registry = WorkspaceModel {
        identity: uuid::Uuid::new_v4(),
        sequence: 1,
        ..WorkspaceModel::default()
    };
    let durable = std::sync::Arc::make_mut(&mut app.durable);
    durable.workspace_model_identity = Some(saved_registry.identity);
    let local = durable
        .channels
        .iter_mut()
        .find(|local| local.id == channel)
        .unwrap();
    local.shared = true;
    let card = local.cards[0];
    write_creation_draft(&mut app, card, "Keep the saved-registry bootstrap draft");
    let queued = take_creation_publications(&requests);
    let create = queued
        .into_iter()
        .find(|edit| matches!(edit, ModelEdit::CreateChannel { .. }))
        .unwrap();
    let acknowledged = channel_publication_model(
        WorkspaceModel {
            identity: uuid::Uuid::new_v4(),
            sequence: 7,
            ..WorkspaceModel::default()
        },
        &create,
    );
    assert_ne!(acknowledged.identity, saved_registry.identity);
    let ack = framed_channel_publication_ack(create, acknowledged.clone());
    let deferred_terminal = repository.join("pre-registry-terminal-choice");
    assert_ne!(app.active_workspace.as_ref(), Some(&deferred_terminal));
    app.shared_views.deferred_default = Some(deferred_terminal.clone());
    let cards = app.durable.cards.clone();
    let drafts = app.durable.chat_drafts.clone();
    let focused = app.workspace.focused();
    let runtime_before = match &app.cards[&card] {
        CardRuntime::Chat(runtime) => (
            std::ptr::from_ref(&**runtime) as usize,
            runtime.session.clone(),
            runtime.target,
        ),
        _ => panic!("saved conversation runtime missing"),
    };
    // This ACK precedes the observer's first connection. It must replace saved
    // registry custody, so a later Connected(saved B) really performs a join
    // rather than retaining the sharing claim just established by A.
    drop(app.update(Message::WorkspaceModelObserved(ack)));
    let registry_after_ack = app.durable.workspace_model_identity;
    let shared_after_ack = app
        .durable
        .channels
        .iter()
        .any(|local| local.id == channel && local.shared);
    // Capture this before Connected/reconciliation can legitimately consume it.
    let terminal_choice_after_ack = app.shared_views.deferred_default.clone();
    drop(
        app.update(Message::WorkspaceModelObserved(ModelUpdate::Connected(
            saved_registry.clone(),
        ))),
    );
    drop(
        app.update(Message::WorkspaceModelObserved(ModelUpdate::Read(
            saved_registry.clone(),
        ))),
    );
    assert_eq!(registry_after_ack, Some(acknowledged.identity));
    assert!(shared_after_ack);
    assert_eq!(
        terminal_choice_after_ack,
        Some(deferred_terminal),
        "bootstrap admission must preserve the real deferred terminal choice until ordinary reconciliation"
    );
    assert_eq!(app.workspace_model, saved_registry);
    assert_eq!(
        app.durable.workspace_model_identity,
        Some(app.workspace_model.identity)
    );
    assert!(!app.channel_ui.is_deleting(channel) && !app.channel_ui.was_deleted(channel));
    let local = app
        .durable
        .channels
        .iter()
        .find(|local| local.id == channel)
        .expect("the current registry has never admitted this local channel");
    assert!(!local.shared && local.cards.contains(&card));
    assert_eq!(app.durable.active_channel, Some(channel));
    assert_eq!(app.workspace.focused(), focused);
    assert_eq!(app.durable.cards, cards);
    assert!(app.card_state(card).unwrap().existence.is_open());
    let runtime_after = match &app.cards[&card] {
        CardRuntime::Chat(runtime) => (
            std::ptr::from_ref(&**runtime) as usize,
            runtime.session.clone(),
            runtime.target,
        ),
        _ => panic!("the original saved conversation runtime must survive"),
    };
    assert_eq!(runtime_after, runtime_before);
    assert_eq!(app.persistent_workspace().chat_drafts, drafts);
}

/// The explicit inventory admission and public publication capture the origin;
/// this helper never constructs or changes the proof after receiving a reply.
fn enqueue_explicit_reopen(
    app: &mut crate::app::Cagents,
    requests: &async_channel::Receiver<QueuedEdit>,
    project: crate::model::ProjectId,
) -> QueuedEdit {
    let repository = app
        .sidebar_model
        .project(project)
        .unwrap()
        .repository
        .clone();
    drop(app.project_load_task(
        repository.clone(),
        Some(project),
        true,
        Some(ProjectKind::Folder),
    ));
    assert!(app.project_ui.is_reopening(&repository));
    app.publish_project_opened(project);
    loop {
        let queued = requests
            .try_recv()
            .expect("explicit reopen must enqueue its captured request");
        if matches!(&queued.edit, ModelEdit::ReopenProject { repository: path, .. } if path == &repository)
        {
            assert!(queued.reopen.is_some());
            return queued;
        }
    }
}

fn framed_reopen_failure(queued: QueuedEdit) -> ModelUpdate {
    let expected = queued.edit.clone();
    let original = queued
        .reopen
        .clone()
        .expect("an actual queued origin is required");
    let peer = Peer::new(vec![connected()], move |_, peer| {
        let request = expect_edit(peer, &expected);
        response(peer, &request, refused());
        true
    });
    let context = PublicationContext {
        authority: Authority::default(),
        remote: false,
        no_start: true,
    };
    let (tx, requests) = async_channel::unbounded();
    tx.send_blocking(queued).unwrap();
    drop(tx);
    let (updates, received) = async_channel::unbounded();
    let mut pending = None;
    let error = publish_with(&updates, &requests, &mut pending, || {
        existing(&context, &peer.socket)
    })
    .unwrap_err();
    assert!(error.contains("PermissionDenied"));
    assert!(pending.is_none() && requests.is_empty());
    peer.finished();
    let update = received.try_recv().unwrap();
    assert!(
        matches!(&update, ModelUpdate::ReopenFailed { origin: Some(origin), .. } if origin == &original)
    );
    assert!(received.is_empty());
    update
}

fn reopened_project_model(mut model: WorkspaceModel, repository: &Path) -> WorkspaceModel {
    model.sequence += 1;
    model.projects.push(crate::workspace_model::ModelProject {
        repository: repository.to_path_buf(),
        name: "Publisher reopen receipt".into(),
        kind: ProjectKind::Folder,
        ordinal: 1,
    });
    model.closed_projects.retain(|path| path != repository);
    model
}

#[test]
fn queued_old_reopen_success_and_failure_cannot_settle_new_registry_or_aba_intent() {
    use crate::app::Message;
    for replacement_registry in [false, true] {
        for success in [false, true] {
            let (_directory, mut app, project, channel, repository, requests, canonical) =
                publication_reconciliation_fixture();
            take_creation_publications(&requests);
            let old = enqueue_explicit_reopen(&mut app, &requests, project);
            let old_origin = old.reopen.clone().unwrap();
            let old_callback = if success {
                framed_model_publication_callback(
                    old,
                    reopened_project_model(canonical.clone(), &repository),
                )
            } else {
                framed_reopen_failure(old)
            };
            let old_generation = app.repository_generations[&project];
            // Its own failed inventory attempt ends the old local intent. The
            // next explicit retry must receive fresh nonce/generation proof.
            drop(app.update(Message::RepositoryLoaded {
                open_cards: true,
                project_id: project,
                generation: old_generation,
                result: Err("Original explicit inventory failed".into()),
            }));
            assert!(!app.project_ui.is_reopening(&repository));
            let mut current = canonical;
            if replacement_registry {
                current.identity = uuid::Uuid::new_v4();
                current.sequence = 1;
                drop(
                    app.update(Message::WorkspaceModelObserved(ModelUpdate::Connected(
                        current.clone(),
                    ))),
                );
            } else {
                current.sequence += 2;
                drop(
                    app.update(Message::WorkspaceModelObserved(ModelUpdate::Read(
                        current.clone(),
                    ))),
                );
            }
            let new = enqueue_explicit_reopen(&mut app, &requests, project);
            let new_origin = new.reopen.clone().unwrap();
            assert_ne!(
                old_origin, new_origin,
                "same project/path is not the same explicit attempt"
            );
            assert!(app.repository_generations[&project] > old_generation);
            let model_before = app.workspace_model.clone();
            let channels = app.durable.channels.clone();
            let cards = app.durable.cards.clone();
            let drafts = app.durable.chat_drafts.clone();
            let focused = app.workspace.focused();
            let pending = app.project_ui.reopening.clone();
            let notices = app.notices.entries.len();
            drop(app.update(Message::WorkspaceModelObserved(old_callback)));
            assert_eq!(
                app.workspace_model, model_before,
                "old or foreign receipt cannot replace newer authority"
            );
            assert_eq!(app.project_ui.reopening, pending);
            assert!(app.current_reopen(&new_origin));
            assert!(app.workspace_model_reopens.deferred.is_empty());
            assert_eq!(
                app.notices.entries.len(),
                notices,
                "old refusal cannot report against the new open"
            );
            assert_eq!(app.durable.channels, channels);
            assert_eq!(app.durable.cards, cards);
            assert_eq!(app.persistent_workspace().chat_drafts, drafts);
            assert_eq!(app.workspace.focused(), focused);
            assert!(!app.channel_ui.is_deleting(channel) && !app.channel_ui.was_deleted(channel));
            let completion = framed_model_publication_callback(
                new,
                reopened_project_model(current, &repository),
            );
            drop(app.update(Message::WorkspaceModelObserved(completion)));
            assert!(!app.project_ui.is_reopening(&repository));
            assert!(!app.current_reopen(&new_origin));
            assert_eq!(app.persistent_workspace().chat_drafts, drafts);
        }
    }
}

#[test]
fn exact_reopen_reply_before_connected_defers_only_proof_until_fresh_observer_confirmation() {
    use crate::app::Message;
    let (_directory, mut app, project, channel, repository, requests, canonical) =
        publication_reconciliation_fixture();
    take_creation_publications(&requests);
    let queued = enqueue_explicit_reopen(&mut app, &requests, project);
    let origin = queued.reopen.clone().unwrap();
    let receipt_model = reopened_project_model(
        WorkspaceModel {
            identity: uuid::Uuid::new_v4(),
            sequence: 19,
            ..WorkspaceModel::default()
        },
        &repository,
    );
    let reply = framed_model_publication_callback(queued, receipt_model.clone());
    let drafts = app.durable.chat_drafts.clone();
    let cards = app.durable.cards.clone();
    let focus = app.workspace.focused();
    drop(app.update(Message::WorkspaceModelObserved(reply)));
    assert_eq!(
        app.workspace_model, canonical,
        "a reply from B is not an observer connection to B"
    );
    assert!(app.current_reopen(&origin) && app.project_ui.is_reopening(&repository));
    assert!(app.workspace_model_reopens.deferred.contains_key(&project));
    let mut earlier = receipt_model.clone();
    earlier.sequence -= 1;
    drop(
        app.update(Message::WorkspaceModelObserved(ModelUpdate::Connected(
            earlier,
        ))),
    );
    assert!(
        app.project_ui.is_reopening(&repository),
        "wait for the actual receipt sequence"
    );
    let mut fresh = receipt_model;
    fresh.projects[0].name = "Fresh observer metadata wins".into();
    drop(
        app.update(Message::WorkspaceModelObserved(ModelUpdate::Read(
            fresh.clone(),
        ))),
    );
    assert_eq!(
        app.workspace_model, fresh,
        "settlement cannot install the held publisher snapshot"
    );
    assert!(!app.project_ui.is_reopening(&repository));
    assert!(!app.project_ui.closed.contains(&repository));
    assert!(
        app.workspace_model_reopens.active.is_empty()
            && app.workspace_model_reopens.deferred.is_empty()
    );
    assert_eq!(app.durable.cards, cards);
    assert_eq!(app.persistent_workspace().chat_drafts, drafts);
    assert_eq!(app.workspace.focused(), focus);
    assert!(!app.channel_ui.is_deleting(channel) && !app.channel_ui.was_deleted(channel));
}

#[test]
fn fresh_project_close_outranks_a_queued_reopen_acknowledgement() {
    use crate::app::Message;
    let (_directory, mut app, project, _channel, repository, requests, canonical) =
        publication_reconciliation_fixture();
    take_creation_publications(&requests);
    let queued = enqueue_explicit_reopen(&mut app, &requests, project);
    let origin = queued.reopen.clone().unwrap();
    let reply = framed_model_publication_callback(
        queued,
        reopened_project_model(
            WorkspaceModel {
                identity: uuid::Uuid::new_v4(),
                sequence: 19,
                ..WorkspaceModel::default()
            },
            &repository,
        ),
    );
    app.project_ui.closed.insert(repository.clone());
    app.project_ui.reopening.remove(&project);
    let drafts = app.durable.chat_drafts.clone();
    drop(app.update(Message::WorkspaceModelObserved(reply)));
    assert_eq!(app.workspace_model, canonical);
    assert!(app.project_ui.closed.contains(&repository));
    assert!(!app.current_reopen(&origin));
    assert!(app.workspace_model_reopens.deferred.is_empty());
    assert_eq!(app.persistent_workspace().chat_drafts, drafts);
}

#[test]
fn background_inventory_failure_cannot_retire_the_explicit_reopen_origin_or_its_close_shield() {
    use crate::app::Message;
    let (_directory, mut app, project, channel, repository, requests, canonical) =
        publication_reconciliation_fixture();
    take_creation_publications(&requests);
    let queued = enqueue_explicit_reopen(&mut app, &requests, project);
    let origin = queued.reopen.clone().unwrap();
    // A passive refresh advances the inventory generation while the explicit
    // queued reopen retains its original intent nonce and close protection.
    drop(app.project_load_task(
        repository.clone(),
        Some(project),
        false,
        Some(ProjectKind::Folder),
    ));
    let passive_generation = app.repository_generations[&project];
    let mut closed_snapshot = canonical.clone();
    closed_snapshot.sequence += 1;
    closed_snapshot.closed_projects.push(repository.clone());
    drop(
        app.update(Message::WorkspaceModelObserved(ModelUpdate::Read(
            closed_snapshot,
        ))),
    );
    assert!(app.project_ui.is_reopening(&repository));
    let drafts = app.durable.chat_drafts.clone();
    let cards = app.durable.cards.clone();
    let focus = app.workspace.focused();
    drop(app.update(Message::RepositoryLoaded {
        open_cards: false,
        project_id: project,
        generation: passive_generation,
        result: Err("Passive inventory refresh failed".into()),
    }));
    assert!(app.project_ui.is_reopening(&repository) && app.current_reopen(&origin));
    assert!(!app.project_ui.closed.contains(&repository));
    assert!(!app.channel_ui.is_deleting(channel) && !app.channel_ui.was_deleted(channel));
    assert_eq!(app.durable.cards, cards);
    assert_eq!(app.persistent_workspace().chat_drafts, drafts);
    assert_eq!(app.workspace.focused(), focus);
    let mut admitted = reopened_project_model(canonical, &repository);
    admitted.sequence += 1;
    let ack = framed_model_publication_callback(queued, admitted);
    drop(app.update(Message::WorkspaceModelObserved(ack)));
    assert!(!app.project_ui.is_reopening(&repository));
    assert_eq!(app.persistent_workspace().chat_drafts, drafts);
}

#[test]
fn contradicted_deferred_reopen_keeps_work_protected_and_allows_a_new_explicit_retry() {
    use crate::app::Message;
    let (_directory, mut app, project, channel, repository, requests, _canonical) =
        publication_reconciliation_fixture();
    take_creation_publications(&requests);
    let queued = enqueue_explicit_reopen(&mut app, &requests, project);
    let old_origin = queued.reopen.clone().unwrap();
    let reply = framed_model_publication_callback(
        queued,
        reopened_project_model(
            WorkspaceModel {
                identity: uuid::Uuid::new_v4(),
                sequence: 19,
                ..WorkspaceModel::default()
            },
            &repository,
        ),
    );
    drop(app.update(Message::WorkspaceModelObserved(reply)));
    let other = WorkspaceModel {
        identity: uuid::Uuid::new_v4(),
        sequence: 1,
        closed_projects: vec![repository.clone()],
        ..WorkspaceModel::default()
    };
    let drafts = app.durable.chat_drafts.clone();
    drop(
        app.update(Message::WorkspaceModelObserved(ModelUpdate::Connected(
            other.clone(),
        ))),
    );
    assert!(
        app.project_ui.is_reopening(&repository),
        "an unrelated connection must not remove the close shield"
    );
    assert!(app.workspace_model_reopens.can_retry(&repository));
    assert!(!app.project_ui.closed.contains(&repository));
    assert!(!app.channel_ui.is_deleting(channel) && !app.channel_ui.was_deleted(channel));
    while requests.try_recv().is_ok() {}
    drop(app.project_load_task(
        repository.clone(),
        Some(project),
        false,
        Some(ProjectKind::Folder),
    ));
    app.publish_project_opened(project);
    assert!(
        requests.is_empty(),
        "a passive refresh cannot replay a completed or contradicted reopen"
    );
    let retry = enqueue_explicit_reopen(&mut app, &requests, project);
    let fresh_origin = retry.reopen.clone().unwrap();
    assert_ne!(old_origin, fresh_origin);
    let ack = framed_model_publication_callback(retry, reopened_project_model(other, &repository));
    drop(app.update(Message::WorkspaceModelObserved(ack)));
    assert!(!app.project_ui.is_reopening(&repository));
    assert!(!app.project_ui.closed.contains(&repository));
    assert_eq!(app.persistent_workspace().chat_drafts, drafts);
}

#[test]
fn an_exact_completed_reopen_releases_its_shield_for_a_newer_current_project_close() {
    use crate::app::Message;
    for reply_before_connection in [false, true] {
        let (_directory, mut app, project, channel, repository, requests, canonical) =
            publication_reconciliation_fixture();
        let creation = take_creation_publications(&requests)
            .into_iter()
            .find(|edit| matches!(edit, ModelEdit::CreateChannel { id, .. } if *id == channel))
            .unwrap();
        let admitted = channel_publication_model(canonical, &creation);
        let shared = framed_channel_publication_ack(creation, admitted.clone());
        drop(app.update(Message::WorkspaceModelObserved(shared)));
        drop(
            app.update(Message::WorkspaceModelObserved(ModelUpdate::Read(
                admitted.clone(),
            ))),
        );
        assert!(
            app.durable
                .channels
                .iter()
                .any(|local| local.id == channel && local.shared)
        );
        let queued = enqueue_explicit_reopen(&mut app, &requests, project);
        let origin = queued.reopen.clone().unwrap();
        let mut opened = admitted;
        opened.sequence += 1;
        if reply_before_connection {
            opened.identity = uuid::Uuid::new_v4();
        }
        let ack = framed_model_publication_callback(queued, opened.clone());
        let close = ModelEdit::CloseProject {
            repository: repository.clone(),
        };
        let close_sequence = opened.sequence + 1;
        let cards = app.durable.cards.clone();
        let drafts = app.persistent_workspace().chat_drafts;
        let focus = app.workspace.focused();
        if reply_before_connection {
            // The reply from B cannot itself replace A. A later observer
            // connection to B already includes the ordered peer close.
            drop(app.update(Message::WorkspaceModelObserved(ack)));
            assert!(app.workspace_model_reopens.deferred.contains_key(&project));
            let mut closed = opened;
            closed.fold(close_sequence, &close);
            drop(
                app.update(Message::WorkspaceModelObserved(ModelUpdate::Connected(
                    closed,
                ))),
            );
        } else {
            // The current registry's close can outrun our own physical edit
            // reply. Before that exact ACK, pending work is still protected.
            drop(
                app.update(Message::WorkspaceModelObserved(ModelUpdate::Edited {
                    sequence: close_sequence,
                    edit: close,
                })),
            );
            assert!(app.project_ui.is_reopening(&repository));
            assert!(!app.project_ui.closed.contains(&repository));
            assert!(!app.channel_ui.is_deleting(channel) && !app.channel_ui.was_deleted(channel));
            assert!(app.sidebar_model.project(project).is_some());
            assert!(
                app.durable
                    .channels
                    .iter()
                    .any(|local| local.id == channel && local.shared)
            );
            assert_eq!(app.durable.cards, cards);
            assert_eq!(app.persistent_workspace().chat_drafts, drafts);
            assert_eq!(app.workspace.focused(), focus);
            drop(app.update(Message::WorkspaceModelObserved(ack)));
        }
        assert_eq!(app.workspace_model.sequence, close_sequence);
        assert!(app.workspace_model.project(&repository).is_none());
        assert!(app.workspace_model.closed_projects.contains(&repository));
        assert!(!app.project_ui.is_reopening(&repository));
        assert!(!app.current_reopen(&origin));
        assert!(app.workspace_model_reopens.active.is_empty());
        assert!(app.workspace_model_reopens.deferred.is_empty());
        assert!(!app.workspace_model_reopens.can_retry(&repository));
        assert!(
            app.project_ui.closed.contains(&repository),
            "completion of this exact nonce must permit the newer canonical close"
        );
        assert!(
            app.project_ui.closing.contains_key(&project)
                || app.sidebar_model.project(project).is_none(),
            "the normal close flow must start, rather than remain shielded"
        );
    }
}

#[test]
fn a_provisional_picker_reopen_shields_existing_shared_channels_before_inventory_remaps_it() {
    use crate::app::Message;
    let (_directory, mut app, project, channel, repository, requests, canonical) =
        publication_reconciliation_fixture();
    let creation = take_creation_publications(&requests)
        .into_iter()
        .find(|edit| matches!(edit, ModelEdit::CreateChannel { id, .. } if *id == channel))
        .unwrap();
    let admitted = channel_publication_model(canonical, &creation);
    let shared = framed_channel_publication_ack(creation, admitted.clone());
    drop(app.update(Message::WorkspaceModelObserved(shared)));
    drop(
        app.update(Message::WorkspaceModelObserved(ModelUpdate::Read(
            admitted.clone(),
        ))),
    );
    assert!(
        app.durable
            .channels
            .iter()
            .any(|local| local.id == channel && local.shared)
    );
    // The actual picker admits a fresh provisional ID even when this path
    // already has a row. Its protection must apply before inventory dedups it.
    drop(app.project_load_task(repository.clone(), None, true, Some(ProjectKind::Folder)));
    let provisional = *app
        .project_ui
        .reopening
        .iter()
        .find(|(_, path)| **path == repository)
        .unwrap()
        .0;
    assert_ne!(provisional, project);
    let generation = app.repository_generations[&provisional];
    let original = app.workspace_model_reopens.active[&provisional].clone();
    let cards = app.durable.cards.clone();
    let drafts = app.persistent_workspace().chat_drafts;
    let focus = app.workspace.focused();
    drop(
        app.update(Message::WorkspaceModelObserved(ModelUpdate::Edited {
            sequence: admitted.sequence + 1,
            edit: ModelEdit::CloseProject {
                repository: repository.clone(),
            },
        })),
    );
    assert!(app.project_ui.is_reopening(&repository));
    assert!(!app.project_ui.closed.contains(&repository));
    assert!(!app.channel_ui.is_deleting(channel) && !app.channel_ui.was_deleted(channel));
    assert!(
        app.durable
            .channels
            .iter()
            .any(|local| local.id == channel && local.shared)
    );
    assert_eq!(app.durable.cards, cards);
    assert_eq!(app.persistent_workspace().chat_drafts, drafts);
    assert_eq!(app.workspace.focused(), focus);
    let loaded = crate::app::load_project(
        &repository,
        Some(provisional),
        tokio_util::sync::CancellationToken::default(),
        Some(ProjectKind::Folder),
    )
    .unwrap();
    drop(app.update(Message::RepositoryLoaded {
        open_cards: true,
        project_id: provisional,
        generation,
        result: Ok(Box::new(loaded)),
    }));
    assert!(
        !app.workspace_model_reopens
            .active
            .contains_key(&provisional)
    );
    assert_eq!(
        app.workspace_model_reopens.active[&project].nonce,
        original.nonce
    );
    let queued = std::iter::from_fn(|| requests.try_recv().ok())
        .find(|queued| matches!(&queued.edit, ModelEdit::ReopenProject { repository: path, .. } if path == &repository))
        .expect("inventory remapping must publish the original picker intent");
    let origin = queued.reopen.unwrap();
    assert_eq!(origin.nonce, original.nonce);
    assert_eq!(origin.project, project);
    assert_eq!(origin.repository, repository);
    assert!(app.project_ui.is_reopening(&repository));
    assert!(!app.channel_ui.is_deleting(channel) && !app.channel_ui.was_deleted(channel));
    assert_eq!(app.persistent_workspace().chat_drafts, drafts);
}

#[test]
fn contradicted_reopen_proof_cannot_repeat_a_dismissed_warning_or_settle_on_later_presence() {
    use crate::app::Message;
    let (_directory, mut app, project, channel, repository, requests, _canonical) =
        publication_reconciliation_fixture();
    take_creation_publications(&requests);
    let queued = enqueue_explicit_reopen(&mut app, &requests, project);
    let original = queued.reopen.clone().unwrap();
    let receipt = reopened_project_model(
        WorkspaceModel {
            identity: uuid::Uuid::new_v4(),
            sequence: 19,
            ..WorkspaceModel::default()
        },
        &repository,
    );
    let callback = framed_model_publication_callback(queued, receipt.clone());
    drop(app.update(Message::WorkspaceModelObserved(callback.clone())));
    assert!(app.workspace_model_reopens.deferred.contains_key(&project));
    let mut closed = receipt.clone();
    closed.sequence += 1;
    closed.projects.clear();
    closed.closed_projects.push(repository.clone());
    closed.identity = uuid::Uuid::new_v4();
    let drafts = app.persistent_workspace().chat_drafts;
    let cards = app.durable.cards.clone();
    let focus = app.workspace.focused();
    drop(
        app.update(Message::WorkspaceModelObserved(ModelUpdate::Connected(
            closed.clone(),
        ))),
    );
    assert!(app.workspace_model_reopens.can_retry(&repository));
    assert!(app.workspace_model_reopens.deferred.is_empty());
    let warning = app
        .notices
        .entries
        .iter()
        .find(|entry| {
            entry
                .notice
                .message
                .contains("reopening could not be confirmed")
        })
        .expect("a contradicted receipt must explain the explicit retry")
        .id;
    drop(app.update(Message::DismissNotice(warning)));
    let notices = app.notices.entries.clone();
    // Neither ordinary activity nor a duplicate physical callback can
    // rearm completed proof after the user has dismissed its failure.
    drop(app.update(Message::WorkspaceModelObserved(ModelUpdate::Read(closed))));
    drop(app.update(Message::WorkspaceModelObserved(callback)));
    assert_eq!(app.notices.entries, notices);
    assert!(app.workspace_model_reopens.deferred.is_empty());
    let mut later = receipt;
    later.sequence += 2;
    drop(
        app.update(Message::WorkspaceModelObserved(ModelUpdate::Connected(
            later.clone(),
        ))),
    );
    assert!(app.current_reopen(&original));
    assert!(app.project_ui.is_reopening(&repository));
    assert!(app.workspace_model_reopens.can_retry(&repository));
    assert_eq!(app.notices.entries, notices);
    assert_eq!(app.durable.cards, cards);
    assert_eq!(app.persistent_workspace().chat_drafts, drafts);
    assert_eq!(app.workspace.focused(), focus);
    assert!(!app.channel_ui.is_deleting(channel) && !app.channel_ui.was_deleted(channel));
    let retry = enqueue_explicit_reopen(&mut app, &requests, project);
    assert_ne!(retry.reopen.as_ref().unwrap(), &original);
    let ack = framed_model_publication_callback(retry, reopened_project_model(later, &repository));
    drop(app.update(Message::WorkspaceModelObserved(ack)));
    assert!(!app.project_ui.is_reopening(&repository));
    assert_eq!(app.persistent_workspace().chat_drafts, drafts);
}

#[test]
fn model_updates_before_first_inventory_reply_keep_the_explicit_origin_and_first_publication() {
    use crate::app::Message;
    use crate::model::ProjectId;
    let (_directory, mut app, _project, _channel, repository, requests, canonical) =
        publication_reconciliation_fixture();
    take_creation_publications(&requests);
    let path = repository.join("unlisted-picker-folder");
    std::fs::create_dir(&path).unwrap();
    let provisional = ProjectId::new();
    drop(app.project_load_task(
        path.clone(),
        Some(provisional),
        true,
        Some(ProjectKind::Folder),
    ));
    let generation = app.repository_generations[&provisional];
    let original = app
        .workspace_model_reopens
        .active
        .get(&provisional)
        .unwrap()
        .clone();
    assert!(app.sidebar_model.project(provisional).is_none());
    let drafts = app.durable.chat_drafts.clone();
    drop(
        app.update(Message::WorkspaceModelObserved(ModelUpdate::Connected(
            canonical.clone(),
        ))),
    );
    let mut observed = canonical;
    observed.sequence += 1;
    observed.closed_projects.push(path.clone());
    drop(app.update(Message::WorkspaceModelObserved(ModelUpdate::Read(observed))));
    assert!(app.project_ui.is_reopening(&path));
    assert_eq!(
        app.workspace_model_reopens
            .active
            .get(&provisional)
            .unwrap()
            .nonce,
        original.nonce
    );
    assert!(
        !app.workspace_model_reopens.can_retry(&path),
        "no completed/refused request exists before first inventory"
    );
    // Use the actual bounded folder loader; no provider, daemon, Git helper or
    // returned UI task is executed by this regression.
    let loaded = crate::app::load_project(
        &path,
        Some(provisional),
        tokio_util::sync::CancellationToken::default(),
        Some(ProjectKind::Folder),
    )
    .unwrap();
    drop(app.update(Message::RepositoryLoaded {
        open_cards: true,
        project_id: provisional,
        generation,
        result: Ok(Box::new(loaded)),
    }));
    let mut reopens = Vec::new();
    while let Ok(queued) = requests.try_recv() {
        if matches!(&queued.edit, ModelEdit::ReopenProject { repository, .. } if repository == &path)
        {
            reopens.push(queued);
        }
    }
    assert_eq!(
        reopens.len(),
        1,
        "a listed picker must publish its first origin-backed reopen exactly once"
    );
    assert_eq!(reopens[0].reopen.as_ref().unwrap().nonce, original.nonce);
    assert_eq!(app.persistent_workspace().chat_drafts, drafts);
}

#[test]
fn a_closed_publication_queue_keeps_reopen_protection_and_allows_an_explicit_new_attempt() {
    let (_directory, mut app, project, channel, repository, requests, _canonical) =
        publication_reconciliation_fixture();
    take_creation_publications(&requests);
    drop(app.project_load_task(
        repository.clone(),
        Some(project),
        true,
        Some(ProjectKind::Folder),
    ));
    let original = app
        .workspace_model_reopens
        .active
        .get(&project)
        .unwrap()
        .clone();
    let cards = app.durable.cards.clone();
    let drafts = app.durable.chat_drafts.clone();
    let (tx, closed) = async_channel::unbounded();
    drop(closed);
    app.workspace_model_tx = tx;
    app.publish_project_opened(project);
    assert!(app.current_reopen(&original) && app.project_ui.is_reopening(&repository));
    assert!(app.workspace_model_reopens.can_retry(&repository));
    assert!(!app.workspace_model_reopens.queued.contains(&project));
    assert!(app.status.as_ref().is_some_and(|notice| {
        notice
            .message
            .contains("Could not queue reopening this project")
    }));
    assert!(!app.project_ui.closed.contains(&repository));
    assert!(!app.channel_ui.is_deleting(channel) && !app.channel_ui.was_deleted(channel));
    assert_eq!(app.durable.cards, cards);
    assert_eq!(app.persistent_workspace().chat_drafts, drafts);
    drop(app.project_load_task(
        repository.clone(),
        Some(project),
        true,
        Some(ProjectKind::Folder),
    ));
    let retry = app.workspace_model_reopens.active.get(&project).unwrap();
    assert_ne!(
        retry.nonce, original.nonce,
        "a fresh explicit picker can recover despite the stopped publisher"
    );
    assert!(app.current_reopen(retry));
    assert!(!app.workspace_model_reopens.queued.contains(&project));
    assert_eq!(app.durable.cards, cards);
    assert_eq!(app.persistent_workspace().chat_drafts, drafts);
}

/// Admit through the same command and confirmation as the project menu. Dropping
/// the returned task deliberately leaves the destructive helper unexecuted.
fn admit_project_removal(
    app: &mut crate::app::Cagents,
    project: crate::model::ProjectId,
    repository: &Path,
) -> crate::app::project_ui::RemovalOrigin {
    use crate::app::project_ui::Action;
    drop(app.handle_project_action(Action::Command {
        project_id: project,
        worktree_id: None,
        command: crate::sidebar::ProjectCommand::Remove,
    }));
    drop(app.handle_project_action(Action::ConfirmRemove));
    app.project_ui.removal_attempts[repository].origin.clone()
}

fn removed_project_model(mut model: WorkspaceModel, repository: &Path) -> WorkspaceModel {
    model.sequence += 1;
    model
        .projects
        .retain(|project| project.repository != repository);
    model
        .channels
        .retain(|channel| channel.repository != repository);
    model.closed_projects.retain(|path| path != repository);
    model.closed_projects.push(repository.to_path_buf());
    model
}

#[test]
fn confirmed_removal_supersedes_retryable_reopen_in_either_observer_close_order() {
    use crate::app::{Message, project_ui::Action};
    for observer_first in [false, true] {
        let (_directory, mut app, project, channel, repository, requests, canonical) =
            publication_reconciliation_fixture();
        take_creation_publications(&requests);
        let queued = enqueue_explicit_reopen(&mut app, &requests, project);
        let reopen = queued.reopen.clone().unwrap();
        drop(
            app.update(Message::WorkspaceModelObserved(framed_reopen_failure(
                queued,
            ))),
        );
        assert!(app.current_reopen(&reopen) && app.workspace_model_reopens.can_retry(&repository));
        let origin = admit_project_removal(&mut app, project, &repository);
        let closed = removed_project_model(canonical, &repository);
        let cards = app.durable.cards.clone();
        let drafts = app.durable.chat_drafts.clone();
        if observer_first {
            drop(
                app.update(Message::WorkspaceModelObserved(ModelUpdate::Read(
                    closed.clone(),
                ))),
            );
            assert!(app.current_reopen(&reopen));
            assert!(!app.project_ui.closed.contains(&repository));
            assert!(!app.channel_ui.is_deleting(channel) && !app.channel_ui.was_deleted(channel));
            assert_eq!(app.durable.cards, cards);
            assert_eq!(app.persistent_workspace().chat_drafts, drafts);
        }
        drop(app.handle_project_action(Action::RemovedProject {
            origin,
            result: Ok(closed.clone()),
        }));
        if !observer_first {
            drop(app.update(Message::WorkspaceModelObserved(ModelUpdate::Read(closed))));
        }
        assert!(!app.project_ui.removal_attempts.contains_key(&repository));
        assert!(!app.project_ui.removing.contains_key(&repository));
        assert!(!app.project_ui.is_reopening(&repository) && !app.current_reopen(&reopen));
        assert!(!app.workspace_model_reopens.can_retry(&repository));
        assert!(app.project_ui.closed.contains(&repository));
        assert!(app.channel_ui.is_deleting(channel) || app.channel_ui.was_deleted(channel));
        while let Ok(queued) = requests.try_recv() {
            assert!(!matches!(queued.edit, ModelEdit::ReopenProject { .. }));
        }
    }
}

#[test]
fn failed_removal_keeps_retryable_reopen_work_and_releases_the_matching_spinner() {
    use crate::app::{Message, project_ui::Action};
    let (_directory, mut app, project, channel, repository, requests, canonical) =
        publication_reconciliation_fixture();
    take_creation_publications(&requests);
    let queued = enqueue_explicit_reopen(&mut app, &requests, project);
    let reopen = queued.reopen.clone().unwrap();
    drop(
        app.update(Message::WorkspaceModelObserved(framed_reopen_failure(
            queued,
        ))),
    );
    let origin = admit_project_removal(&mut app, project, &repository);
    assert!(app.project_ui.removing.contains_key(&repository));
    let generation = app.repository_generations[&project];
    let cards = app.durable.cards.clone();
    let drafts = app.durable.chat_drafts.clone();
    let focused = app.workspace.focused();
    drop(app.project_load_task(
        repository.clone(),
        Some(project),
        true,
        Some(ProjectKind::Folder),
    ));
    assert_eq!(app.repository_generations[&project], generation);
    assert!(app.current_reopen(&reopen));
    assert_eq!(app.project_ui.removal_attempts[&repository].origin, origin);
    assert!(
        requests.is_empty(),
        "pending removal must not enqueue a new reopen"
    );
    drop(app.handle_project_action(Action::RemovedProject {
        origin: origin.clone(),
        result: Err("Private removal fixture refused the operation".into()),
    }));
    assert!(!app.project_ui.removing.contains_key(&repository));
    assert!(!app.project_ui.removal_attempts.contains_key(&repository));
    assert!(!app.project_ui.close_started_for(project));
    assert!(app.current_reopen(&reopen) && app.workspace_model_reopens.can_retry(&repository));
    assert!(
        app.status
            .as_ref()
            .is_some_and(|notice| notice.message.contains("Could not remove project"))
    );
    drop(
        app.update(Message::WorkspaceModelObserved(ModelUpdate::Read(
            removed_project_model(canonical, &repository),
        ))),
    );
    assert!(!app.project_ui.closed.contains(&repository));
    assert!(!app.channel_ui.is_deleting(channel) && !app.channel_ui.was_deleted(channel));
    assert_eq!(app.durable.cards, cards);
    assert_eq!(app.persistent_workspace().chat_drafts, drafts);
    assert_eq!(app.workspace.focused(), focused);
    let retry = admit_project_removal(&mut app, project, &repository);
    assert_ne!(retry, origin, "a user retry is a new removal confirmation");
}

#[test]
fn an_unsettled_reopen_refuses_project_removal_without_losing_its_close_shield() {
    use crate::app::{Message, project_ui::Action};
    let (_directory, mut app, project, channel, repository, requests, canonical) =
        publication_reconciliation_fixture();
    take_creation_publications(&requests);
    let queued = enqueue_explicit_reopen(&mut app, &requests, project);
    let reopen = queued.reopen.unwrap();
    let cards = app.durable.cards.clone();
    let drafts = app.durable.chat_drafts.clone();
    drop(app.handle_project_action(Action::Command {
        project_id: project,
        worktree_id: None,
        command: crate::sidebar::ProjectCommand::Remove,
    }));
    drop(app.handle_project_action(Action::ConfirmRemove));
    assert!(app.project_ui.removal_attempts.is_empty());
    assert!(!app.project_ui.removing.contains_key(&repository));
    assert!(app.current_reopen(&reopen));
    assert!(
        app.status
            .as_ref()
            .is_some_and(|notice| notice.message.contains("finish opening"))
    );
    drop(
        app.update(Message::WorkspaceModelObserved(ModelUpdate::Read(
            removed_project_model(canonical, &repository),
        ))),
    );
    assert!(!app.project_ui.closed.contains(&repository));
    assert!(!app.channel_ui.is_deleting(channel) && !app.channel_ui.was_deleted(channel));
    assert_eq!(app.durable.cards, cards);
    assert_eq!(app.persistent_workspace().chat_drafts, drafts);
    assert!(requests.is_empty());
}

#[test]
fn old_removal_replies_cannot_cross_a_registry_or_settle_an_aba_successor_confirmation() {
    use crate::app::{Message, project_ui::Action};
    for old_sequence in [Some(1), Some(10_000), None] {
        let (_directory, mut app, project, channel, repository, requests, canonical) =
            publication_reconciliation_fixture();
        take_creation_publications(&requests);
        let queued = enqueue_explicit_reopen(&mut app, &requests, project);
        drop(
            app.update(Message::WorkspaceModelObserved(framed_reopen_failure(
                queued,
            ))),
        );
        let old = admit_project_removal(&mut app, project, &repository);
        let old_result = if let Some(sequence) = old_sequence {
            let mut closed = removed_project_model(canonical.clone(), &repository);
            closed.sequence = sequence;
            Ok(closed)
        } else {
            Err("Old A removal failed".to_owned())
        };
        let foreign = WorkspaceModel {
            identity: uuid::Uuid::new_v4(),
            sequence: 2,
            ..WorkspaceModel::default()
        };
        drop(
            app.update(Message::WorkspaceModelObserved(ModelUpdate::Connected(
                foreign.clone(),
            ))),
        );
        let cards = app.durable.cards.clone();
        let drafts = app.durable.chat_drafts.clone();
        let focused = app.workspace.focused();
        drop(app.handle_project_action(Action::RemovedProject {
            origin: old.clone(),
            result: old_result.clone(),
        }));
        assert_eq!(
            app.workspace_model, foreign,
            "neither high nor low foreign sequence is current authority"
        );
        assert_eq!(app.durable.workspace_model_identity, Some(foreign.identity));
        assert!(app.project_ui.removal_attempts.is_empty());
        assert!(!app.project_ui.removing.contains_key(&repository));
        assert!(app.project_ui.is_reopening(&repository));
        assert!(app.sidebar_model.project(project).is_some());
        assert!(!app.channel_ui.is_deleting(channel) && !app.channel_ui.was_deleted(channel));
        let mut fresh_foreign = foreign;
        fresh_foreign.sequence += 1;
        drop(
            app.update(Message::WorkspaceModelObserved(ModelUpdate::Read(
                fresh_foreign.clone(),
            ))),
        );
        assert_eq!(
            app.workspace_model, fresh_foreign,
            "old A cannot poison B's sequence watermark"
        );
        drop(
            app.update(Message::WorkspaceModelObserved(ModelUpdate::Connected(
                canonical.clone(),
            ))),
        );
        let queued = enqueue_explicit_reopen(&mut app, &requests, project);
        let successor_reopen = queued.reopen.clone().unwrap();
        drop(
            app.update(Message::WorkspaceModelObserved(framed_reopen_failure(
                queued,
            ))),
        );
        let successor = admit_project_removal(&mut app, project, &repository);
        assert_ne!(successor, old);
        let current = app.workspace_model.clone();
        drop(app.handle_project_action(Action::RemovedProject {
            origin: old,
            result: old_result,
        }));
        assert_eq!(app.workspace_model, current);
        assert_eq!(
            app.project_ui.removal_attempts[&repository].origin,
            successor
        );
        assert!(app.project_ui.removing.contains_key(&repository));
        assert!(app.current_reopen(&successor_reopen));
        assert!(!app.project_ui.closed.contains(&repository));
        assert!(!app.channel_ui.is_deleting(channel) && !app.channel_ui.was_deleted(channel));
        assert_eq!(app.durable.cards, cards);
        assert_eq!(app.persistent_workspace().chat_drafts, drafts);
        assert_eq!(app.workspace.focused(), focused);
        drop(app.handle_project_action(Action::RemovedProject {
            origin: successor,
            result: Err("Current confirmation failed".into()),
        }));
        assert!(!app.project_ui.removing.contains_key(&repository));
        assert!(app.current_reopen(&successor_reopen));
    }
}

#[test]
fn a_newer_canonical_open_defeats_stale_successful_removal_without_sticking_its_spinner() {
    use crate::app::{Message, project_ui::Action};
    let (_directory, mut app, project, channel, repository, requests, canonical) =
        publication_reconciliation_fixture();
    take_creation_publications(&requests);
    let queued = enqueue_explicit_reopen(&mut app, &requests, project);
    drop(
        app.update(Message::WorkspaceModelObserved(framed_reopen_failure(
            queued,
        ))),
    );
    let origin = admit_project_removal(&mut app, project, &repository);
    let closed = removed_project_model(canonical.clone(), &repository);
    let mut opened = reopened_project_model(canonical, &repository);
    opened.sequence = closed.sequence + 1;
    drop(
        app.update(Message::WorkspaceModelObserved(ModelUpdate::Read(
            opened.clone(),
        ))),
    );
    let cards = app.durable.cards.clone();
    let drafts = app.durable.chat_drafts.clone();
    let focused = app.workspace.focused();
    drop(app.handle_project_action(Action::RemovedProject {
        origin,
        result: Ok(closed),
    }));
    assert_eq!(app.workspace_model, opened);
    assert!(!app.project_ui.removal_attempts.contains_key(&repository));
    assert!(!app.project_ui.removing.contains_key(&repository));
    assert!(!app.project_ui.close_started_for(project));
    assert!(!app.project_ui.closed.contains(&repository));
    assert!(!app.project_ui.closing.contains_key(&project));
    assert!(!app.channel_ui.is_deleting(channel) && !app.channel_ui.was_deleted(channel));
    assert!(app.sidebar_model.project(project).is_some());
    assert_eq!(app.durable.cards, cards);
    assert_eq!(app.persistent_workspace().chat_drafts, drafts);
    assert_eq!(app.workspace.focused(), focused);
}

#[test]
fn successful_removal_still_retires_the_same_reopen_nonce_after_passive_inventory_changes() {
    use crate::app::project_ui::Action;
    let (_directory, mut app, project, channel, repository, requests, canonical) =
        publication_reconciliation_fixture();
    take_creation_publications(&requests);
    drop(app.project_load_task(
        repository.clone(),
        Some(project),
        true,
        Some(ProjectKind::Folder),
    ));
    let (tx, stopped) = async_channel::unbounded();
    drop(stopped);
    app.workspace_model_tx = tx;
    app.publish_project_opened(project);
    assert!(app.workspace_model_reopens.can_retry(&repository));
    let before = app.workspace_model_reopens.active[&project].clone();
    let origin = admit_project_removal(&mut app, project, &repository);
    drop(app.project_load_task(
        repository.clone(),
        Some(project),
        false,
        Some(ProjectKind::Folder),
    ));
    app.publish_project_opened(project);
    let refreshed = app.workspace_model_reopens.active[&project].clone();
    assert_eq!(refreshed.nonce, before.nonce);
    assert_ne!(refreshed.inventory_generation, before.inventory_generation);
    assert!(app.current_reopen(&refreshed));
    drop(app.handle_project_action(Action::RemovedProject {
        origin,
        result: Ok(removed_project_model(canonical, &repository)),
    }));
    assert!(!app.project_ui.removal_attempts.contains_key(&repository));
    assert!(!app.project_ui.is_reopening(&repository));
    assert!(!app.workspace_model_reopens.active.contains_key(&project));
    assert!(!app.workspace_model_reopens.can_retry(&repository));
    assert!(app.project_ui.closed.contains(&repository));
    assert!(app.channel_ui.is_deleting(channel) || app.channel_ui.was_deleted(channel));
}

#[test]
fn an_observed_close_does_not_release_pending_removal_admission_when_its_spinner_stops() {
    use crate::app::{Message, project_ui::Action};
    let (_directory, mut app, project, _channel, repository, requests, canonical) =
        publication_reconciliation_fixture();
    take_creation_publications(&requests);
    let origin = admit_project_removal(&mut app, project, &repository);
    let closed = removed_project_model(canonical, &repository);
    drop(app.update(Message::WorkspaceModelObserved(ModelUpdate::Read(closed))));
    assert_eq!(app.project_ui.removal_attempts[&repository].origin, origin);
    assert!(!app.project_ui.removing.contains_key(&repository));
    let generation = app.repository_load_sequence;
    drop(app.project_load_task(
        repository.clone(),
        Some(project),
        true,
        Some(ProjectKind::Folder),
    ));
    assert_eq!(
        app.repository_load_sequence, generation,
        "a pending helper still owns removal admission after observer close"
    );
    assert!(!app.project_ui.is_reopening(&repository));
    drop(app.handle_project_action(Action::Command {
        project_id: project,
        worktree_id: None,
        command: crate::sidebar::ProjectCommand::Remove,
    }));
    drop(app.handle_project_action(Action::ConfirmRemove));
    assert_eq!(app.project_ui.removal_attempts.len(), 1);
    assert_eq!(app.project_ui.removal_attempts[&repository].origin, origin);
    while let Ok(queued) = requests.try_recv() {
        assert!(!matches!(queued.edit, ModelEdit::ReopenProject { .. }));
    }
    drop(app.handle_project_action(Action::RemovedProject {
        origin,
        result: Err("Removal helper finished after the observed close".into()),
    }));
    assert!(!app.project_ui.removal_attempts.contains_key(&repository));
}

#[test]
fn observer_close_callbacks_lose_their_generation_across_registry_replacement_and_aba() {
    use crate::{
        app::{
            Message,
            project_ui::{Action, ProjectSessionClosure},
        },
        model::{ProjectId, RepositoryId},
        sidebar::SidebarProject,
    };
    for successor in ["open", "closed", "aba-closed"] {
        let directory = tempfile::tempdir().unwrap();
        let repository = directory.path().canonicalize().unwrap();
        let project = ProjectId::new();
        let mut app = crate::app::tests::context_inspector_test_app(true);
        app.sidebar_model.projects.push(
            SidebarProject::new(
                project,
                RepositoryId::new(),
                "Private observer-close fixture".into(),
                repository.clone(),
                Vec::new(),
                Vec::new(),
            )
            .with_kind(ProjectKind::Folder),
        );
        // No project channels need draining, so this real Connected schedules
        // the scoped session helper immediately. Its returned task is held.
        let a = removed_project_model(
            WorkspaceModel {
                identity: uuid::Uuid::new_v4(),
                sequence: 4,
                ..WorkspaceModel::default()
            },
            &repository,
        );
        drop(
            app.update(Message::WorkspaceModelObserved(ModelUpdate::Connected(
                a.clone(),
            ))),
        );
        let old_generation = app.project_ui.close_generation[&project];
        assert!(app.project_ui.close_in_flight_for(project));
        let old_cancellation = app.project_ui.close_cancellation_for(project).unwrap();
        let b = WorkspaceModel {
            identity: uuid::Uuid::new_v4(),
            sequence: 1,
            ..WorkspaceModel::default()
        };
        let mut current = if successor == "closed" {
            removed_project_model(b, &repository)
        } else {
            reopened_project_model(b, &repository)
        };
        drop(
            app.update(Message::WorkspaceModelObserved(ModelUpdate::Connected(
                current.clone(),
            ))),
        );
        assert!(old_cancellation.is_cancelled());
        if successor == "aba-closed" {
            current = a;
            current.sequence += 1;
            drop(
                app.update(Message::WorkspaceModelObserved(ModelUpdate::Connected(
                    current.clone(),
                ))),
            );
        }
        let expected_generation = app.project_ui.close_generation.get(&project).copied();
        if successor == "open" {
            assert_eq!(expected_generation, None);
            assert!(!app.project_ui.closing.contains_key(&project));
            assert!(!app.project_ui.closed.contains(&repository));
        } else {
            assert_ne!(expected_generation, Some(old_generation));
            assert!(expected_generation.is_some());
            assert!(app.project_ui.close_in_flight_for(project));
        }
        let cards = app.durable.cards.clone();
        let drafts = app.durable.chat_drafts.clone();
        let focused = app.workspace.focused();
        drop(app.handle_project_action(Action::ProjectSessionsClosed(
            project,
            old_generation,
            Ok(ProjectSessionClosure::for_roots(vec![repository.clone()])),
        )));
        assert_eq!(app.workspace_model, current);
        assert!(
            app.sidebar_model.project(project).is_some(),
            "{successor}: old registry's completed helper cannot retire the current row"
        );
        assert_eq!(
            app.project_ui.close_generation.get(&project).copied(),
            expected_generation
        );
        assert_eq!(app.durable.cards, cards);
        assert_eq!(app.persistent_workspace().chat_drafts, drafts);
        assert_eq!(app.workspace.focused(), focused);
        if let Some(generation) = expected_generation {
            // The fresh B (or fresh A after ABA) close remains actionable;
            // invalidating A's earlier callback must not strand this one.
            drop(app.handle_project_action(Action::ProjectSessionsClosed(
                project,
                generation,
                Ok(ProjectSessionClosure::for_roots(vec![repository.clone()])),
            )));
            assert!(app.sidebar_model.project(project).is_none());
        }
    }
}

/// A real observer tombstone schedules a close of an empty folder project.
/// Keep its task unexecuted so the resulting generation can arrive out of order.
fn queued_observer_close_fixture() -> (
    tempfile::TempDir,
    crate::app::Cagents,
    crate::model::ProjectId,
    PathBuf,
    WorkspaceModel,
    u64,
) {
    use crate::{
        app::Message,
        model::{ProjectId, RepositoryId},
        sidebar::SidebarProject,
    };
    let directory = tempfile::tempdir().unwrap();
    let repository = directory.path().canonicalize().unwrap();
    let project = ProjectId::new();
    let mut app = crate::app::tests::context_inspector_test_app(false);
    app.sidebar_model.projects.push(
        SidebarProject::new(
            project,
            RepositoryId::new(),
            "Private held observer close".into(),
            repository.clone(),
            Vec::new(),
            Vec::new(),
        )
        .with_kind(ProjectKind::Folder),
    );
    let closed = removed_project_model(
        WorkspaceModel {
            identity: uuid::Uuid::new_v4(),
            sequence: 18,
            ..WorkspaceModel::default()
        },
        &repository,
    );
    drop(
        app.update(Message::WorkspaceModelObserved(ModelUpdate::Connected(
            closed.clone(),
        ))),
    );
    let generation = app.project_ui.close_generation[&project];
    assert!(app.project_ui.close_in_flight_for(project));
    (directory, app, project, repository, closed, generation)
}

#[test]
fn same_registry_reconnect_after_a_missed_reopen_invalidates_the_old_close_callback() {
    use crate::app::{
        Message,
        project_ui::{Action, ProjectSessionClosure},
    };
    let (_directory, mut app, project, repository, closed, generation) =
        queued_observer_close_fixture();
    assert_eq!(closed.sequence, 19);
    let cancellation = app.project_ui.close_cancellation_for(project).unwrap();
    let opened = reopened_project_model(closed, &repository);
    assert_eq!(opened.sequence, 20);
    // The watcher missed the ReopenProject edge. Its reconnect is to the same
    // registry, but the fresh full model proves this repository is open again.
    drop(
        app.update(Message::WorkspaceModelObserved(ModelUpdate::Connected(
            opened.clone(),
        ))),
    );
    assert!(cancellation.is_cancelled());
    assert!(!app.project_ui.closed.contains(&repository));
    assert!(!app.project_ui.closing.contains_key(&project));
    assert!(!app.project_ui.close_generation.contains_key(&project));
    assert!(!app.project_ui.close_in_flight_for(project));
    drop(app.handle_project_action(Action::AwaitClosedProject(project, generation)));
    drop(app.handle_project_action(Action::ProjectSessionsClosed(
        project,
        generation,
        Ok(ProjectSessionClosure::for_roots(vec![repository.clone()])),
    )));
    assert_eq!(app.workspace_model, opened);
    assert_eq!(
        app.sidebar_model.project(project).unwrap().repository,
        repository
    );
    assert!(!app.project_ui.closed.contains(&repository));
    assert!(!app.project_ui.close_generation.contains_key(&project));
}

#[test]
fn a_current_inventory_replacing_the_project_repository_fences_old_close_launch_and_completion() {
    use crate::app::{
        Message,
        project_ui::{Action, ProjectSessionClosure},
    };
    let (directory, mut app, project, repository, closed, generation) =
        queued_observer_close_fixture();
    let successor = directory.path().join("replacement-folder");
    std::fs::create_dir(&successor).unwrap();
    let successor = successor.canonicalize().unwrap();
    let cancellation = app.project_ui.close_cancellation_for(project).unwrap();
    drop(app.project_load_task(
        successor.clone(),
        Some(project),
        false,
        Some(ProjectKind::Folder),
    ));
    let inventory_generation = app.repository_generations[&project];
    // The actual bounded folder loader returns the same requested project ID
    // with a new repository. No Git/provider/helper process is needed.
    let loaded = crate::app::load_project(
        &successor,
        Some(project),
        tokio_util::sync::CancellationToken::default(),
        Some(ProjectKind::Folder),
    )
    .unwrap();
    drop(app.update(Message::RepositoryLoaded {
        open_cards: false,
        project_id: project,
        generation: inventory_generation,
        result: Ok(Box::new(loaded)),
    }));
    assert_eq!(
        app.sidebar_model.project(project).unwrap().repository,
        successor
    );
    assert_eq!(app.workspace_model, closed);
    // Exercise both callback boundaries: a queued poll cannot launch its old
    // generation against S's roots, and a completed R helper cannot retire S.
    drop(app.handle_project_action(Action::AwaitClosedProject(project, generation)));
    assert!(cancellation.is_cancelled());
    assert!(!app.project_ui.close_in_flight_for(project));
    assert!(!app.project_ui.close_generation.contains_key(&project));
    drop(app.handle_project_action(Action::ProjectSessionsClosed(
        project,
        generation,
        Ok(ProjectSessionClosure::for_roots(vec![repository.clone()])),
    )));
    assert_eq!(
        app.sidebar_model.project(project).unwrap().repository,
        successor
    );
    assert!(!app.project_ui.closed.contains(&successor));
    assert!(!app.project_ui.closing.contains_key(&project));
    assert!(!app.project_ui.close_generation.contains_key(&project));
}

#[test]
fn retiring_an_unrelated_shared_channel_during_read_keeps_the_real_project_close_authority() {
    use crate::{
        app::{
            Message, channel_ui,
            project_ui::{Action, ProjectSessionClosure},
        },
        model::{ProjectId, RepositoryId},
        sidebar::SidebarProject,
    };
    let (directory, mut app, project, repository, closed, generation) =
        queued_observer_close_fixture();
    let cancellation = app.project_ui.close_cancellation_for(project).unwrap();
    let unrelated_repository = directory.path().join("unrelated-project");
    std::fs::create_dir(&unrelated_repository).unwrap();
    let unrelated_project = ProjectId::new();
    app.sidebar_model.projects.push(
        SidebarProject::new(
            unrelated_project,
            RepositoryId::new(),
            "Unrelated shared channel".into(),
            unrelated_repository.clone(),
            Vec::new(),
            Vec::new(),
        )
        .with_kind(ProjectKind::Folder),
    );
    let (tx, requests) = async_channel::unbounded();
    app.workspace_model_tx = tx;
    drop(app.handle_channel_ui(channel_ui::Action::Create(unrelated_project)));
    let unrelated_channel = app.durable.active_channel.unwrap();
    let create = take_creation_publications(&requests)
        .into_iter()
        .find(
            |edit| matches!(edit, ModelEdit::CreateChannel { id, .. } if *id == unrelated_channel),
        )
        .unwrap();
    let shared = channel_publication_model(closed, &create);
    drop(app.update(Message::WorkspaceModelObserved(
        framed_channel_publication_ack(create, shared.clone()),
    )));
    drop(
        app.update(Message::WorkspaceModelObserved(ModelUpdate::Read(
            shared.clone(),
        ))),
    );
    assert!(
        app.durable
            .channels
            .iter()
            .any(|channel| channel.id == unrelated_channel && channel.shared)
    );
    assert_eq!(
        app.project_ui.close_generation.get(&project),
        Some(&generation)
    );
    assert!(!cancellation.is_cancelled());
    // An ordinary current-registry read deletes S while R remains canonically
    // closed. Retiring S calls project completion during reconciliation's
    // temporary model extraction; that placeholder cannot revoke R's proof.
    let mut absent = shared;
    absent.sequence += 1;
    absent
        .channels
        .retain(|channel| channel.id != unrelated_channel);
    drop(
        app.update(Message::WorkspaceModelObserved(ModelUpdate::Read(
            absent.clone(),
        ))),
    );
    assert_eq!(app.workspace_model, absent);
    assert!(app.channel_ui.was_deleted(unrelated_channel));
    assert!(
        !app.durable
            .channels
            .iter()
            .any(|channel| channel.id == unrelated_channel)
    );
    assert_eq!(
        app.project_ui.close_generation.get(&project),
        Some(&generation)
    );
    assert_eq!(app.project_ui.closing.get(&project), Some(&repository));
    assert!(app.project_ui.closed.contains(&repository));
    assert!(app.project_ui.close_in_flight_for(project));
    assert!(!cancellation.is_cancelled());
    drop(app.handle_project_action(Action::ProjectSessionsClosed(
        project,
        generation,
        Ok(ProjectSessionClosure::for_roots(vec![repository.clone()])),
    )));
    assert!(
        app.sidebar_model.project(project).is_none(),
        "the original legitimate close must still finish"
    );
    assert!(app.sidebar_model.project(unrelated_project).is_some());
    assert!(app.project_ui.closed.contains(&repository));
}
