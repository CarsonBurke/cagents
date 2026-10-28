//! Framed publisher replies and owning App reducers. Peers prove callback
//! custody, not Host authorization. Loader tasks run only in the remap and
//! final Git fixtures; returned pane/provider/close tasks are never executed.
use super::*;
use crate::{
    model::{BootId, CardId, HostIncarnation, HostSessionId, ProjectKind},
    terminal::ipc::{
        ClientEnvelope, ClientRequest, PROTOCOL_VERSION, ProtocolError, ProtocolErrorCode,
        ServerEnvelope, ServerMessage, ServerResponse, read_frame, write_frame,
    },
};
use std::{os::unix::net::UnixStream, thread};

fn reply(peer: &mut UnixStream, request: &ClientEnvelope, message: ServerMessage) {
    let proof = crate::model::HostSessionProof {
        host_session_id: HostSessionId::new("registry-custody-fixture"),
        boot_id: BootId::new("registry-custody-fixture"),
        host_incarnation: HostIncarnation::new("registry-custody-fixture"),
    };
    write_frame(
        peer,
        &ServerEnvelope {
            version: PROTOCOL_VERSION,
            request_id: Some(request.request_id),
            proof,
            message,
        },
    )
    .unwrap();
}

fn framed(queued: QueuedEdit, model: WorkspaceModel, refused: bool) -> ModelUpdate {
    let (client, mut peer) = UnixStream::pair().unwrap();
    for stream in [&client, &peer] {
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(3)))
            .unwrap();
    }
    let expected = queued.edit.clone();
    let worker = thread::spawn(move || {
        let hello: ClientEnvelope = read_frame(&mut peer).unwrap().unwrap();
        assert!(matches!(hello.request, ClientRequest::Connect { .. }));
        reply(
            &mut peer,
            &hello,
            ServerMessage::Response {
                response: ServerResponse::Connected {
                    proof: crate::model::HostSessionProof {
                        host_session_id: HostSessionId::new("registry-custody-fixture"),
                        boot_id: BootId::new("registry-custody-fixture"),
                        host_incarnation: HostIncarnation::new("registry-custody-fixture"),
                    },
                    resumed: vec![],
                },
            },
        );
        let request: ClientEnvelope = read_frame(&mut peer).unwrap().unwrap();
        assert!(
            matches!(&request.request, ClientRequest::EditWorkspaceModel { edit } if edit == &expected)
        );
        reply(
            &mut peer,
            &request,
            if refused {
                ServerMessage::Error(ProtocolError::new(
                    ProtocolErrorCode::PermissionDenied,
                    "Fixture refused the exact reopen",
                    false,
                ))
            } else {
                ServerMessage::Response {
                    response: ServerResponse::WorkspaceModel { model },
                }
            },
        );
        assert!(read_frame::<ClientEnvelope>(&mut peer).unwrap().is_none());
    });
    let (tx, requests) = async_channel::unbounded();
    tx.send_blocking(queued).unwrap();
    drop(tx);
    let (updates, received) = async_channel::unbounded();
    let mut pending = None;
    assert!(
        publish_with(&updates, &requests, &mut pending, || {
            ControlClient::from_stream(client).map_err(|error| error.to_string())
        })
        .is_err()
    );
    assert!(pending.is_none());
    worker.join().unwrap();
    let result = received
        .try_recv()
        .expect("the actual publisher must produce its callback");
    assert!(received.is_empty());
    result
}

fn fixture() -> (
    tempfile::TempDir,
    Cagents,
    ProjectId,
    ChannelId,
    PathBuf,
    async_channel::Receiver<QueuedEdit>,
    WorkspaceModel,
) {
    let directory = tempfile::tempdir().unwrap();
    let repository = directory.path().canonicalize().unwrap();
    let runtime = repository.join("runtime");
    std::fs::create_dir(&runtime).unwrap();
    let mut app = crate::app::tests::context_inspector_test_app(true);
    app.worktree_protections.use_fixture_runtime(runtime);
    let project = ProjectId::new();
    app.sidebar_model.projects.push(
        SidebarProject::new(
            project,
            RepositoryId::new(),
            "Retained local project".into(),
            repository.clone(),
            vec![],
            vec![],
        )
        .with_kind(ProjectKind::Folder),
    );
    let channel = ChannelId::new();
    let mut local = local(channel, project, "Retained conversation", 3);
    local.cards.push(app.durable.cards[0].id);
    local.focused = Some(local.cards[0]);
    let durable = Arc::make_mut(&mut app.durable);
    durable.channels.push(local);
    durable.active_channel = Some(channel);
    let requests = intercept_publishes(&mut app);
    let mut canonical = registry(vec![]);
    canonical.projects.push(ModelProject {
        repository: repository.clone(),
        name: "Retained local project".into(),
        kind: ProjectKind::Folder,
        ordinal: 1,
    });
    drop(
        app.update(Message::WorkspaceModelObserved(ModelUpdate::Connected(
            canonical.clone(),
        ))),
    );
    drain(&requests);
    (
        directory, app, project, channel, repository, requests, canonical,
    )
}

fn admit(
    app: &mut Cagents,
    requests: &async_channel::Receiver<QueuedEdit>,
    project: ProjectId,
) -> QueuedEdit {
    let repository = app
        .sidebar_model
        .project(project)
        .unwrap()
        .repository
        .clone();
    // Real admission mints the nonce before the inventory future. Existing
    // folder-row inventory is irrelevant to these reducer tests and unpolled.
    drop(app.project_load_task(
        repository.clone(),
        Some(project),
        true,
        Some(ProjectKind::Folder),
    ));
    app.publish_project_opened(project);
    std::iter::from_fn(|| requests.try_recv().ok()).find(|queued| {
        matches!(&queued.edit, ModelEdit::ReopenProject { repository: path, .. } if path == &repository)
    }).expect("explicit admission must enqueue an origin-backed reopen")
}

fn opened(mut model: WorkspaceModel, repository: &Path, sequence: u64) -> WorkspaceModel {
    model.sequence = sequence;
    model.closed_projects.retain(|path| path != repository);
    if model.project(repository).is_none() {
        model.projects.push(ModelProject {
            repository: repository.into(),
            name: "Reopened project".into(),
            kind: ProjectKind::Folder,
            ordinal: 1,
        });
    }
    model
}

#[test]
fn foreign_read_reopen_and_rank_callbacks_cannot_authorize_local_retirement() {
    for kind in ["read", "reopen", "rank"] {
        let (_directory, mut app, project, channel, repository, requests, canonical) = fixture();
        let old = opened(
            registry(vec![shared(channel, &repository, "Old authority", 27)]),
            &repository,
            30,
        );
        assert_ne!(old.identity, canonical.identity);
        let queued = match kind {
            "read" => ModelEdit::ResetNavigation {
                repository: repository.clone(),
            }
            .into(),
            "reopen" => admit(&mut app, &requests, project),
            _ => ModelEdit::CreateChannel {
                id: channel,
                repository: repository.clone(),
                title: "Retained conversation".into(),
                title_source: Default::default(),
            }
            .into(),
        };
        let cards = app.durable.cards.clone();
        let focus = app.workspace.focused();
        let ordinal = app
            .durable
            .channels
            .iter()
            .find(|local| local.id == channel)
            .unwrap()
            .ordinal;
        let callback = framed(queued, old.clone(), false);
        drop(app.update(Message::WorkspaceModelObserved(callback)));
        drop(
            app.update(Message::WorkspaceModelObserved(ModelUpdate::Read(
                canonical.clone(),
            ))),
        );
        assert_eq!(app.workspace_model, canonical);
        assert_eq!(
            app.durable.workspace_model_identity,
            Some(canonical.identity)
        );
        let local = app
            .durable
            .channels
            .iter()
            .find(|local| local.id == channel)
            .unwrap();
        assert!(!local.shared && local.ordinal == ordinal);
        assert!(!app.channel_ui.is_deleting(channel) && !app.channel_ui.was_deleted(channel));
        assert_eq!(app.durable.cards, cards);
        assert_eq!(app.workspace.focused(), focus);
    }
}

#[test]
fn bootstrap_rank_binds_its_registry_before_saved_registry_absence_is_interpreted() {
    let (_directory, mut app, _project, channel, repository, _requests, canonical) = fixture();
    app.workspace_model = WorkspaceModel::default();
    Arc::make_mut(&mut app.durable).workspace_model_identity = Some(canonical.identity);
    let choice = repository.join("pre-registry-default-choice");
    app.shared_views.deferred_default = Some(choice.clone());
    let other = registry(vec![shared(
        channel,
        &repository,
        "Acknowledged elsewhere",
        21,
    )]);
    let ack = framed(
        ModelEdit::CreateChannel {
            id: channel,
            repository,
            title: "Retained conversation".into(),
            title_source: Default::default(),
        }
        .into(),
        other.clone(),
        false,
    );
    drop(app.update(Message::WorkspaceModelObserved(ack)));
    assert_eq!(app.durable.workspace_model_identity, Some(other.identity));
    assert_eq!(app.shared_views.deferred_default, Some(choice));
    let cards = app.durable.cards.clone();
    drop(
        app.update(Message::WorkspaceModelObserved(ModelUpdate::Connected(
            canonical,
        ))),
    );
    assert!(
        !app.durable
            .channels
            .iter()
            .find(|local| local.id == channel)
            .unwrap()
            .shared
    );
    assert!(!app.channel_ui.is_deleting(channel));
    assert_eq!(app.durable.cards, cards);
}

#[test]
fn bootstrap_read_joins_before_absence_and_a_saved_registry_reconnect_preserves_local_work() {
    let (_directory, mut app, _project, channel, repository, _requests, saved) = fixture();
    Arc::make_mut(&mut app.durable)
        .channels
        .iter_mut()
        .find(|local| local.id == channel)
        .unwrap()
        .shared = true;
    app.workspace_model = WorkspaceModel::default();
    assert_eq!(app.durable.workspace_model_identity, Some(saved.identity));
    let cards = app.durable.cards.clone();
    let focus = app.workspace.focused();
    let incoming = opened(registry(vec![]), &repository, 19);
    assert_ne!(incoming.identity, saved.identity);
    let callback = framed(
        ModelEdit::ResetNavigation { repository }.into(),
        incoming.clone(),
        false,
    );
    assert!(matches!(&callback, ModelUpdate::Read(model) if model == &incoming));
    drop(app.update(Message::WorkspaceModelObserved(callback)));
    assert_eq!(app.workspace_model, incoming);
    assert_eq!(
        app.durable.workspace_model_identity,
        Some(incoming.identity)
    );
    assert!(
        app.durable
            .channels
            .iter()
            .any(|local| local.id == channel && !local.shared)
    );
    assert!(!app.channel_ui.is_deleting(channel) && !app.channel_ui.was_deleted(channel));
    assert_eq!(app.durable.cards, cards);
    assert_eq!(app.workspace.focused(), focus);

    drop(
        app.update(Message::WorkspaceModelObserved(ModelUpdate::Connected(
            saved.clone(),
        ))),
    );
    assert_eq!(app.durable.workspace_model_identity, Some(saved.identity));
    assert!(
        app.durable
            .channels
            .iter()
            .any(|local| local.id == channel && !local.shared)
    );
    assert!(!app.channel_ui.is_deleting(channel) && !app.channel_ui.was_deleted(channel));
    assert_eq!(app.durable.cards, cards);
    assert_eq!(app.workspace.focused(), focus);
}

#[test]
fn exact_reopen_waits_for_observer_but_a_newer_same_registry_close_consumes_it() {
    for late_close in [false, true] {
        let (_directory, mut app, project, channel, repository, requests, canonical) = fixture();
        let queued = admit(&mut app, &requests, project);
        let origin = queued.reopen.clone().unwrap();
        let receipt = opened(
            WorkspaceModel {
                identity: uuid::Uuid::new_v4(),
                ..WorkspaceModel::default()
            },
            &repository,
            19,
        );
        let callback = framed(queued, receipt.clone(), false);
        if late_close {
            let mut earlier = receipt.clone();
            earlier.sequence = 18;
            drop(
                app.update(Message::WorkspaceModelObserved(ModelUpdate::Connected(
                    earlier,
                ))),
            );
            drop(
                app.update(Message::WorkspaceModelObserved(ModelUpdate::Edited {
                    sequence: 20,
                    edit: ModelEdit::CloseProject {
                        repository: repository.clone(),
                    },
                })),
            );
            assert!(
                app.project_ui.is_reopening(&repository)
                    && !app.project_ui.closed.contains(&repository)
            );
            drop(app.update(Message::WorkspaceModelObserved(callback)));
            assert_eq!(app.workspace_model.sequence, 20);
            assert!(app.project_ui.closed.contains(&repository));
            assert!(app.project_ui.closing.contains_key(&project));
        } else {
            let cards = app.durable.cards.clone();
            drop(app.update(Message::WorkspaceModelObserved(callback)));
            assert_eq!(app.workspace_model, canonical);
            assert!(app.current_reopen(&origin));
            drop(
                app.update(Message::WorkspaceModelObserved(ModelUpdate::Connected(
                    receipt,
                ))),
            );
            assert_eq!(app.durable.cards, cards);
            assert!(!app.channel_ui.is_deleting(channel));
        }
        assert!(!app.project_ui.is_reopening(&repository));
        assert!(!app.workspace_model_reopens.active.contains_key(&project));
        assert!(!app.workspace_model_reopens.deferred.contains_key(&project));
    }
}

#[test]
fn old_success_failure_and_passive_inventory_cannot_settle_a_successor_reopen_nonce() {
    for refused in [false, true] {
        let (_directory, mut app, project, channel, repository, requests, canonical) = fixture();
        let old = admit(&mut app, &requests, project);
        let failure = framed(old.clone(), canonical.clone(), true);
        drop(app.update(Message::WorkspaceModelObserved(failure)));
        assert!(app.workspace_model_reopens.can_retry(&repository));
        let new = admit(&mut app, &requests, project);
        let origin = new.reopen.clone().unwrap();
        assert_ne!(old.reopen.as_ref().unwrap().nonce, origin.nonce);
        drop(app.project_load_task(
            repository.clone(),
            Some(project),
            false,
            Some(ProjectKind::Folder),
        ));
        let generation = app.repository_generations[&project];
        drop(app.update(Message::RepositoryLoaded {
            open_cards: false,
            project_id: project,
            generation,
            result: Err("Passive inventory failure preserves explicit intent".into()),
        }));
        assert!(app.current_reopen(&origin));
        let callback = framed(old, opened(canonical.clone(), &repository, 19), refused);
        let cards = app.durable.cards.clone();
        drop(app.update(Message::WorkspaceModelObserved(callback)));
        assert!(app.current_reopen(&origin) && app.project_ui.is_reopening(&repository));
        assert!(!app.channel_ui.is_deleting(channel));
        assert_eq!(app.durable.cards, cards);
        drop(app.update(Message::WorkspaceModelObserved(framed(
            new,
            opened(canonical, &repository, 20),
            false,
        ))));
        assert!(!app.project_ui.is_reopening(&repository));
    }
}

#[test]
fn picker_inventory_remaps_its_nonce_to_the_existing_project_before_publication() {
    let (_directory, mut app, project, _channel, repository, requests, canonical) = fixture();
    let inventory = first_message(app.project_load_task(
        repository.clone(),
        None,
        true,
        Some(ProjectKind::Folder),
    ))
    .expect("the actual folder inventory must complete");
    let Message::RepositoryLoaded {
        project_id: provisional,
        result: Ok(_),
        ..
    } = &inventory
    else {
        panic!("actual picker inventory")
    };
    let provisional = *provisional;
    assert_ne!(provisional, project);
    let origin = app.workspace_model_reopens.active[&provisional].clone();
    drop(app.update(inventory));
    assert!(
        !app.workspace_model_reopens
            .active
            .contains_key(&provisional)
    );
    assert!(!app.project_ui.reopening.contains_key(&provisional));
    assert_eq!(app.sidebar_model.projects.len(), 1);
    let queued = std::iter::from_fn(|| requests.try_recv().ok())
        .find(|queued| {
            queued
                .reopen
                .as_ref()
                .is_some_and(|origin| origin.project == project)
        })
        .expect("the remapped inventory must queue its exact existing-row origin");
    let remapped = queued.reopen.as_ref().unwrap();
    assert_eq!(remapped.nonce, origin.nonce);
    assert_eq!(remapped.project, project);
    assert!(app.current_reopen(remapped));
    drop(app.update(Message::WorkspaceModelObserved(framed(
        queued,
        opened(canonical, &repository, 19),
        false,
    ))));
    assert!(!app.project_ui.is_reopening(&repository));
    assert!(app.workspace_model_reopens.active.is_empty());
}

#[test]
fn retired_passive_result_cannot_release_a_new_explicit_reopen_origin() {
    for failed in [false, true] {
        let checkout = git_checkout().expect("the custody integration fixture requires Git");
        let repository = canonical_repository(&checkout.path().join(".git")).unwrap();
        let runtime = checkout.path().canonicalize().unwrap().join("runtime");
        std::fs::create_dir(&runtime).unwrap();
        let mut app = crate::app::tests::context_inspector_test_app(false);
        app.worktree_protections.use_fixture_runtime(runtime);
        let requests = intercept_publishes(&mut app);
        let mut model = registry(vec![]);
        model.projects.push(ModelProject {
            repository: repository.clone(),
            name: "Shared Git project".into(),
            kind: ProjectKind::Git,
            ordinal: 1,
        });
        app.workspace_model = model.clone();
        let Message::RepositoryLoaded {
            project_id: passive,
            generation: old_generation,
            result: Ok(old),
            ..
        } = first_message(app.reconcile_workspace_model()).unwrap()
        else {
            panic!("actual passive Git read")
        };
        let explicit =
            first_message(app.repository_load_task(repository.clone(), None, false)).unwrap();
        let Message::RepositoryLoaded {
            project_id: installed,
            ..
        } = &explicit
        else {
            panic!("actual independent read")
        };
        let installed = *installed;
        drop(app.update(explicit));
        let sequence = app.workspace_model.sequence + 1;
        drop(
            app.update(Message::WorkspaceModelObserved(ModelUpdate::Edited {
                sequence,
                edit: ModelEdit::CloseProject {
                    repository: repository.clone(),
                },
            })),
        );
        assert!(!app.workspace_model_loads.contains_key(&passive));
        let close_generation = app.project_ui.close_generation[&installed];
        drop(app.update(Message::ProjectUi(
            crate::app::project_ui::Action::ProjectSessionsClosed(
                installed,
                close_generation,
                Ok(crate::app::project_ui::ProjectSessionClosure::for_roots(
                    vec![repository.clone()],
                )),
            ),
        )));
        drain(&requests);
        let current = ProjectId::new();
        let inventory = first_message(app.project_load_task(
            repository.clone(),
            Some(current),
            true,
            Some(ProjectKind::Git),
        ))
        .unwrap();
        let origin = app.workspace_model_reopens.active[&current].clone();
        drop(app.update(Message::RepositoryLoaded {
            open_cards: false,
            project_id: passive,
            generation: old_generation,
            result: if failed {
                Err("retired passive failure".into())
            } else {
                Ok(old)
            },
        }));
        assert!(app.live_reopen_intent(&origin));
        assert!(app.sidebar_model.projects.is_empty());
        drop(app.update(inventory));
        let queued = std::iter::from_fn(|| requests.try_recv().ok()).find(|queued|
            matches!(&queued.edit, ModelEdit::ReopenProject { repository: path, .. } if path == &repository)).unwrap();
        assert_eq!(queued.reopen.as_ref().unwrap().nonce, origin.nonce);
        assert!(app.workspace_model_loads.is_empty());
        assert_eq!(app.project_for_repository(&repository), Some(current));
        model.sequence = sequence + 1;
        drop(app.update(Message::WorkspaceModelObserved(framed(
            queued, model, false,
        ))));
        assert!(!app.project_ui.is_reopening(&repository));
        assert!(app.workspace_model_reopens.active.is_empty());
        assert_eq!(app.sidebar_model.projects.len(), 1);
    }
}

// After-only removal custody cases ported from owner4784. The actual App
// command, framed publisher, and typed completion run; destructive helper tasks
// are dropped. No provider/native/Host authorization proof is claimed.
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

fn enqueue_explicit_reopen(
    app: &mut Cagents,
    requests: &async_channel::Receiver<QueuedEdit>,
    project: ProjectId,
) -> QueuedEdit {
    admit(app, requests, project)
}

fn framed_reopen_failure(queued: QueuedEdit) -> ModelUpdate {
    let original = queued
        .reopen
        .clone()
        .expect("an actual queued origin is required");
    let update = framed(queued, WorkspaceModel::default(), true);
    assert!(
        matches!(&update, ModelUpdate::ReopenFailed { origin: Some(origin), .. } if origin == &original)
    );
    update
}

fn framed_channel_publication_ack(edit: ModelEdit, model: WorkspaceModel) -> ModelUpdate {
    assert!(matches!(edit, ModelEdit::CreateChannel { .. }));
    framed(edit.into(), model, false)
}

fn framed_model_publication_callback(queued: QueuedEdit, model: WorkspaceModel) -> ModelUpdate {
    framed(queued, model, false)
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
