//! Framed publisher replies and owning App reducers. Peers prove callback
//! custody, not Host authorization. Loader tasks run only in the remap and
//! final Git fixtures; returned pane/provider/close tasks are never executed.
use super::*;
use crate::{
    model::{BootId, HostIncarnation, HostSessionId},
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
