//! After-only port of the #1803 registry-custody regressions at 2195169e7.
//! The owning App and physical framed callbacks run without a daemon/provider.
//! Local queue/origin APIs do not exist at the #1806 predecessor.
use super::*;
use crate::{
    model::{BootId, CardId, HostIncarnation, HostSessionId, HostSessionProof, ProjectKind},
    terminal::ipc::{
        ClientEnvelope, ClientRequest, PROTOCOL_VERSION, ProtocolError, ProtocolErrorCode,
        ServerEnvelope, ServerMessage, ServerResponse, read_frame, write_frame,
    },
};
use std::{
    cell::Cell,
    io,
    os::unix::net::{UnixListener, UnixStream},
    thread::{self, JoinHandle},
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

fn refused() -> ServerMessage {
    ServerMessage::Error(ProtocolError::new(
        ProtocolErrorCode::PermissionDenied,
        "The private fixture refused this publication",
        false,
    ))
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
    let mut pending = None;
    let connections = Cell::new(0);
    let error = publish_with(&updates, &requests, &mut pending, || {
        connections.set(connections.get() + 1);
        ControlClient::connect(&peer.socket)
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
    let (tx, requests) = async_channel::unbounded();
    tx.send_blocking(edit).unwrap();
    drop(tx);
    let (updates, received) = async_channel::unbounded();
    let mut pending = None;
    let connections = Cell::new(0);
    let error = publish_with(&updates, &requests, &mut pending, || {
        connections.set(connections.get() + 1);
        ControlClient::connect(&peer.socket)
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
        assert!(migration.iter().any(|edit| matches!(edit,
            ModelEdit::OpenProject { repository: path, .. } if path == &repository)));
        assert!(migration.iter().any(|edit| matches!(edit,
            ModelEdit::CreateChannel { id, .. } if *id == channel)));
        // #1806 retains its original publication retry policy; this regression
        // establishes migration custody without importing #1803 coalescing.
        drop(app.update(Message::WorkspaceModelObserved(ModelUpdate::Read(incoming))));
        // Target publication retries remain its existing policy.
        take_creation_publications(&requests);
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
    let (tx, requests) = async_channel::unbounded();
    tx.send_blocking(queued).unwrap();
    drop(tx);
    let (updates, received) = async_channel::unbounded();
    let mut pending = None;
    let error = publish_with(&updates, &requests, &mut pending, || {
        ControlClient::connect(&peer.socket)
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
            // Settle the physical request before admitting its replacement:
            // success completes opening; an actual refusal permits a retry.
            drop(app.update(Message::WorkspaceModelObserved(old_callback.clone())));
            if success {
                assert!(!app.project_ui.is_reopening(&repository));
            } else {
                assert!(app.project_ui.is_reopening(&repository));
                assert!(app.workspace_model_reopens.can_retry(&repository));
            }
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
            // Replay the old reducer outcome to exercise stale-origin defense.
            // This does not assert the publisher duplicates callbacks in normal
            // operation or promises exactly-once delivery.
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
        assert_eq!(
            requests.len(),
            queued,
            "old callback itself cannot replay or compensate"
        );
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
        assert_eq!(app.workspace_model, replacement);
        assert_eq!(
            app.durable.workspace_model_identity,
            Some(app.workspace_model.identity)
        );
        assert_eq!(app.project_ui.closed, closed);
        assert_eq!(app.project_ui.reopening, reopening);

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
        let runtime = directory.path().join("runtime");
        std::fs::create_dir(&runtime).unwrap();
        app.worktree_protections.use_fixture_runtime(runtime);
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
    let runtime = directory.path().join("runtime");
    std::fs::create_dir(&runtime).unwrap();
    app.worktree_protections.use_fixture_runtime(runtime);
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

#[test]
fn prepublication_inventory_failure_keeps_loaded_and_provisional_retry_work_protected() {
    use crate::app::Message;
    for provisional in [false, true] {
        let (_directory, mut app, project, channel, repository, requests, canonical) =
            publication_reconciliation_fixture();
        let create = take_creation_publications(&requests)
            .into_iter()
            .find(|edit| matches!(edit, ModelEdit::CreateChannel { id, .. } if *id == channel))
            .unwrap();
        let admitted = channel_publication_model(canonical, &create);
        drop(app.update(Message::WorkspaceModelObserved(
            framed_channel_publication_ack(create, admitted.clone()),
        )));
        drop(
            app.update(Message::WorkspaceModelObserved(ModelUpdate::Read(
                admitted.clone(),
            ))),
        );
        let old = enqueue_explicit_reopen(&mut app, &requests, project);
        let old_origin = old.reopen.clone().unwrap();
        drop(app.update(Message::WorkspaceModelObserved(framed_reopen_failure(old))));
        assert!(app.workspace_model_reopens.can_retry(&repository));
        let closed = removed_project_model(admitted.clone(), &repository);
        drop(
            app.update(Message::WorkspaceModelObserved(ModelUpdate::Read(
                closed.clone(),
            ))),
        );
        let channels = app.durable.channels.clone();
        let cards = app.durable.cards.clone();
        let drafts = app.persistent_workspace().chat_drafts;
        let focused = app.workspace.focused();
        // A retry from the picker can use either the loaded project P or a
        // provisional Q that has no sidebar row until inventory completes.
        drop(app.project_load_task(
            repository.clone(),
            (!provisional).then_some(project),
            true,
            Some(ProjectKind::Folder),
        ));
        let pending = *app
            .project_ui
            .reopening
            .iter()
            .find(|(_, path)| **path == repository)
            .unwrap()
            .0;
        if provisional {
            assert_ne!(pending, project);
            assert!(app.sidebar_model.project(pending).is_none());
        } else {
            assert_eq!(pending, project);
        }
        let original = app.workspace_model_reopens.active[&pending].clone();
        assert_ne!(original.nonce, old_origin.nonce);
        let generation = app.repository_generations[&pending];
        assert!(!app.workspace_model_reopens.queued.contains(&pending));
        assert!(
            requests.is_empty(),
            "the fresh inventory has not published a reopen"
        );
        drop(app.update(Message::RepositoryLoaded {
            open_cards: true,
            project_id: pending,
            generation,
            result: Err("Private inventory IO failure before publication".into()),
        }));
        assert_eq!(
            app.workspace_model_reopens.active.get(&pending),
            Some(&original)
        );
        assert_eq!(app.project_ui.reopening.get(&pending), Some(&repository));
        assert!(app.workspace_model_reopens.can_retry(&repository));
        assert!(!app.workspace_model_reopens.queued.contains(&pending));
        assert!(!app.project_ui.closed.contains(&repository));
        assert!(!app.project_ui.closing.contains_key(&project));
        assert!(!app.channel_ui.is_deleting(channel) && !app.channel_ui.was_deleted(channel));
        assert_eq!(app.durable.channels, channels);
        assert_eq!(app.durable.cards, cards);
        assert_eq!(app.persistent_workspace().chat_drafts, drafts);
        assert_eq!(app.workspace.focused(), focused);
        assert!(requests.is_empty());
        // An equal cached close must remain harmless while the user decides
        // to retry. Recovery is finite and requires a fresh explicit pick.
        drop(
            app.update(Message::WorkspaceModelObserved(ModelUpdate::Read(
                closed.clone(),
            ))),
        );
        assert_eq!(app.durable.channels, channels);
        assert!(!app.channel_ui.is_deleting(channel) && !app.channel_ui.was_deleted(channel));
        assert!(
            requests.is_empty(),
            "a cached close cannot automatically retry the failed inventory"
        );
        drop(app.project_load_task(
            repository.clone(),
            Some(project),
            true,
            Some(ProjectKind::Folder),
        ));
        let retry = app.workspace_model_reopens.active[&project].clone();
        assert_ne!(retry.nonce, original.nonce);
        if provisional {
            assert!(!app.workspace_model_reopens.active.contains_key(&pending));
        }
        let generation = app.repository_generations[&project];
        let loaded = crate::app::load_project(
            &repository,
            Some(project),
            tokio_util::sync::CancellationToken::default(),
            Some(ProjectKind::Folder),
        )
        .unwrap();
        drop(app.update(Message::RepositoryLoaded {
            open_cards: true,
            project_id: project,
            generation,
            result: Ok(Box::new(loaded)),
        }));
        let queued = std::iter::from_fn(|| requests.try_recv().ok()).find(|queued| {
            matches!(&queued.edit, ModelEdit::ReopenProject { repository: path, .. } if path == &repository)
        }).expect("successful explicit retry publishes its captured origin");
        assert_eq!(queued.reopen.as_ref().unwrap().nonce, retry.nonce);
        // The successful explicit pick can navigate into the worktree and
        // create its default pane. It must retain the protected conversation;
        // its later publisher receipt must not change that new selection.
        for card in &cards {
            assert_eq!(
                app.durable
                    .cards
                    .iter()
                    .find(|current| current.id == card.id),
                Some(card)
            );
        }
        let recovered_cards = app.durable.cards.clone();
        let recovered_drafts = app.persistent_workspace().chat_drafts;
        for (target, draft) in &drafts {
            assert_eq!(recovered_drafts.get(target), Some(draft));
        }
        let recovered_focus = app.workspace.focused();
        let mut opened = admitted;
        opened.sequence = closed.sequence + 1;
        drop(app.update(Message::WorkspaceModelObserved(
            framed_model_publication_callback(queued, opened),
        )));
        assert!(!app.project_ui.is_reopening(&repository));
        assert!(!app.workspace_model_reopens.can_retry(&repository));
        assert!(!app.channel_ui.is_deleting(channel) && !app.channel_ui.was_deleted(channel));
        assert_eq!(app.durable.channels, channels);
        assert_eq!(app.durable.cards, recovered_cards);
        assert_eq!(app.persistent_workspace().chat_drafts, recovered_drafts);
        assert_eq!(app.workspace.focused(), recovered_focus);
    }
}

#[test]
fn inventory_failure_after_publication_keeps_the_inflight_shield_until_its_real_callback() {
    use crate::app::Message;
    let (_directory, mut app, project, channel, repository, requests, canonical) =
        publication_reconciliation_fixture();
    let create = take_creation_publications(&requests)
        .into_iter()
        .find(|edit| matches!(edit, ModelEdit::CreateChannel { id, .. } if *id == channel))
        .unwrap();
    let admitted = channel_publication_model(canonical, &create);
    drop(app.update(Message::WorkspaceModelObserved(
        framed_channel_publication_ack(create, admitted.clone()),
    )));
    drop(
        app.update(Message::WorkspaceModelObserved(ModelUpdate::Read(
            admitted.clone(),
        ))),
    );
    // The ordinary UI publishes after inventory succeeds. This adversarial
    // ordering invokes its public publication hook first, then delivers an IO
    // failure for that generation; it makes no claim that normal inventory
    // emits both outcomes or that production duplicates producer callbacks.
    let queued = enqueue_explicit_reopen(&mut app, &requests, project);
    let original = queued.reopen.clone().unwrap();
    let generation = app.repository_generations[&project];
    let closed = removed_project_model(admitted.clone(), &repository);
    drop(
        app.update(Message::WorkspaceModelObserved(ModelUpdate::Read(
            closed.clone(),
        ))),
    );
    let channels = app.durable.channels.clone();
    let cards = app.durable.cards.clone();
    let drafts = app.persistent_workspace().chat_drafts;
    let focused = app.workspace.focused();
    drop(app.update(Message::RepositoryLoaded {
        open_cards: true,
        project_id: project,
        generation,
        result: Err("Private inventory IO failure while publication is queued".into()),
    }));
    assert!(app.current_reopen(&original));
    assert!(app.workspace_model_reopens.queued.contains(&project));
    assert!(!app.workspace_model_reopens.can_retry(&repository));
    drop(app.project_load_task(
        repository.clone(),
        Some(project),
        true,
        Some(ProjectKind::Folder),
    ));
    assert_eq!(app.repository_generations[&project], generation);
    assert!(app.current_reopen(&original));
    assert!(
        requests.is_empty(),
        "inventory failure cannot admit overlapping publication"
    );
    assert!(!app.project_ui.closed.contains(&repository));
    assert!(!app.channel_ui.is_deleting(channel) && !app.channel_ui.was_deleted(channel));
    assert_eq!(app.durable.channels, channels);
    assert_eq!(app.durable.cards, cards);
    assert_eq!(app.persistent_workspace().chat_drafts, drafts);
    assert_eq!(app.workspace.focused(), focused);
    // Only the actual publisher outcome drains this in-flight attempt and
    // permits a new explicit nonce. An IO failure alone cannot do so.
    drop(
        app.update(Message::WorkspaceModelObserved(framed_reopen_failure(
            queued,
        ))),
    );
    assert!(app.current_reopen(&original));
    assert!(app.workspace_model_reopens.can_retry(&repository));
    let retry = enqueue_explicit_reopen(&mut app, &requests, project);
    assert_ne!(retry.reopen.as_ref().unwrap().nonce, original.nonce);
    let mut opened = admitted;
    opened.sequence = closed.sequence + 1;
    drop(app.update(Message::WorkspaceModelObserved(
        framed_model_publication_callback(retry, opened),
    )));
    assert!(!app.project_ui.is_reopening(&repository));
    assert_eq!(app.durable.channels, channels);
    assert_eq!(app.durable.cards, cards);
    assert_eq!(app.persistent_workspace().chat_drafts, drafts);
    assert_eq!(app.workspace.focused(), focused);
}
