//! UI-side shared-view regressions. Tasks are deliberately dropped: no provider,
//! daemon, native window or host registry runs in this fixture. A public peer
//! Close produces the typed Remove; applying it to the model below is an
//! explicitly synthetic authoritative host result, delivered through Read.
use super::*;
use crate::app::workspace_model_sync::ModelUpdate;
use crate::model::{BootId, ChannelId};
use crate::session::SessionRuntime;
use crate::workspace_model::{
    ModelChannel, ModelEdit, ModelProject, WorkspaceModel,
    views::{Change, Pane, Scope, View},
};

struct Fixture {
    app: Cagents,
    model: WorkspaceModel,
    project: ProjectId,
    channel: ChannelId,
    card: CardId,
    sibling: CardId,
    directory: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let repository = directory.path().canonicalize().unwrap();
        let project = ProjectId::new();
        let channel = ChannelId::new();
        let mut app = crate::app::tests::context_inspector_test_app(false);
        install_project(&mut app, project, &repository);
        let durable = Arc::make_mut(&mut app.durable);
        durable.channels.push(crate::model::ProjectChannel {
            id: channel,
            project_id: project,
            title: "Image draft".into(),
            title_source: Default::default(),
            ordinal: 1,
            shared: true,
            cards: Vec::new(),
            focused: None,
        });
        durable.active_channel = Some(channel);
        app.active_workspace = Some(repository.clone());
        let mut cards = Vec::new();
        for _ in 0..2 {
            drop(app.new_native_chat(Provider::Codex, Some(repository.clone())));
            let card = app.workspace.focused().unwrap();
            assert!(runtime(&app, card).is_draft());
            assert!(!app.chat_start_pending(card));
            cards.push(card);
        }
        let model = WorkspaceModel {
            identity: uuid::Uuid::new_v4(),
            sequence: 10,
            projects: vec![ModelProject {
                repository: repository.clone(),
                name: "Image draft".into(),
                kind: ProjectKind::Folder,
                ordinal: 1,
            }],
            channels: vec![ModelChannel {
                id: channel,
                repository,
                title: "Image draft".into(),
                title_source: Default::default(),
                ordinal: 1,
            }],
            views: vec![View {
                scope: Scope::Channel(channel),
                revision: 7,
                panes: cards
                    .iter()
                    .map(|id| Pane::from_card(app.card_state(*id).unwrap(), None).unwrap())
                    .collect(),
                retired: Vec::new(),
            }],
            ..WorkspaceModel::default()
        };
        acknowledge_initial_view(&mut app, channel, &model);
        Self {
            app,
            model,
            project,
            channel,
            card: cards[0],
            sibling: cards[1],
            directory,
        }
    }

    fn image_only(&mut self, image: &Attachment) {
        let target = runtime(&self.app, self.card).target;
        let ticket = self
            .app
            .chat_runtime_mut(target)
            .unwrap()
            .begin_attachment_insert();
        drop(self.app.update(Message::ChatUi(Action::ImageSaved(
            target,
            ticket,
            Ok(image.clone()),
        ))));
        assert!(
            runtime(&self.app, self.card)
                .composer
                .text()
                .contains(&image.reference.as_ref().unwrap().token())
        );
        // Ordinary select-all/delete removes the inline marker, not its chip.
        replace_text(&mut self.app, self.card, "");
        self.assert_image_only(image);
    }

    fn assert_image_only(&self, image: &Attachment) {
        let runtime = runtime(&self.app, self.card);
        assert_eq!(runtime.unsent_text(), "");
        assert_eq!(runtime.unsent_attachments(), [image.clone()]);
        assert!(!runtime.closed_elsewhere);
        assert!(!runtime.has_unsent_text());
        assert_eq!(runtime.transition_blocker(), None);
        assert!(!self.app.chat_ui.phases.contains_key(&self.card));
        assert!(!self.app.chat_ui.drafts.contains_key(&self.card));
        assert!(!self.app.chat_ui.draft_attachments.contains_key(&self.card));
    }

    fn park(&mut self) {
        let CardRuntime::Chat(runtime) = self.app.cards.remove(&self.card).unwrap() else {
            panic!("chat runtime");
        };
        self.app.chat_ui.parked.insert(self.card, runtime);
    }

    fn peer_remove(&self) -> WorkspaceModel {
        // A second UI instance has the same acknowledged two-pane view and no
        // local draft. Its actual public Close owns the outgoing operation.
        let mut peer = crate::app::tests::context_inspector_test_app(false);
        peer.durable = self.app.durable.clone();
        let durable = Arc::make_mut(&mut peer.durable);
        durable.chat_drafts.clear();
        durable.chat_draft_attachments.clear();
        peer.workspace = Cagents::rebuild_workspace(&peer.durable);
        peer.active_workspace = self.app.active_workspace.clone();
        install_project(&mut peer, self.project, self.directory.path());
        for id in [self.card, self.sibling] {
            let CardKind::Chat(session) = &peer.card_state(id).unwrap().kind else {
                panic!("unstarted chat");
            };
            let session = session.clone();
            assert!(session.is_unstarted());
            drop(peer.open_chat_draft(id, session));
            assert!(!peer.shared_chat_has_local_work(id));
        }
        let (publish, published) = async_channel::unbounded();
        peer.workspace_model_tx = publish;
        acknowledge_initial_view(&mut peer, self.channel, &self.model);
        while published.try_recv().is_ok() {}
        drop(peer.update(Message::CloseCard(self.card)));
        assert!(!peer.card_state(self.card).unwrap().existence.is_open());
        assert!(peer.card_state(self.sibling).unwrap().existence.is_open());
        let mut edits = Vec::new();
        while let Ok(edit) = published.try_recv() {
            edits.push(edit);
        }
        let removes = edits
            .iter()
            .filter_map(|edit| match edit {
                ModelEdit::ChangeView {
                    scope,
                    expected_revision,
                    change: change @ Change::Remove { id },
                    ..
                } if *id == self.card => Some((scope, expected_revision, change)),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(removes.len(), 1, "typed peer Remove: {edits:?}");
        let (scope, expected_revision, change) = removes[0];
        assert_eq!(*scope, Scope::Channel(self.channel));
        assert_eq!(*expected_revision, 7);
        let mut model = self.model.clone();
        model.sequence += 1;
        let view = model
            .views
            .iter_mut()
            .find(|view| &view.scope == scope)
            .unwrap();
        assert!(view.apply(change).unwrap());
        view.revision += 1;
        assert_eq!(view.panes.len(), 1);
        assert_eq!(view.panes[0].id, self.sibling);
        assert!(view.retired.contains(&self.card));
        model
    }

    fn observe(&mut self, model: WorkspaceModel) {
        drop(
            self.app
                .update(Message::WorkspaceModelObserved(ModelUpdate::Read(model))),
        );
    }

    fn assert_retained_receipt(&mut self, image: &Attachment) {
        assert!(self.app.card_state(self.card).unwrap().existence.is_open());
        assert!(self.app.workspace.order().any(|id| id == self.card));
        assert!(self.app.durable.local_only_cards.contains(&self.card));
        assert_eq!(
            runtime(&self.app, self.card).unsent_attachments(),
            [image.clone()]
        );
        let saved = self.app.persistent_workspace();
        assert!(!saved.chat_drafts.contains_key(&self.card));
        assert_eq!(saved.chat_draft_attachments[&self.card], [image.clone()]);
    }
}

fn install_project(app: &mut Cagents, project: ProjectId, repository: &Path) {
    app.sidebar_model.projects.push(SidebarProject::new(
        project,
        RepositoryId::new(),
        "Image draft".into(),
        repository.into(),
        Vec::new(),
        Vec::new(),
    ));
}

fn acknowledge_initial_view(app: &mut Cagents, channel: ChannelId, model: &WorkspaceModel) {
    app.workspace_model = model.clone();
    drop(
        app.update(Message::WorkspaceModelObserved(ModelUpdate::ViewApplied {
            scope: Scope::Channel(channel),
            operation: None,
            model: model.clone(),
        })),
    );
}

fn runtime(app: &Cagents, card: CardId) -> &Runtime {
    match app.cards.get(&card) {
        Some(CardRuntime::Chat(runtime)) => runtime,
        _ => app
            .chat_ui
            .parked
            .get(&card)
            .expect("mounted or parked chat"),
    }
}

fn replace_text(app: &mut Cagents, card: CardId, text: &str) {
    let target = runtime(app, card).target;
    for edit in [
        text_editor::Action::Move(text_editor::Motion::DocumentStart),
        text_editor::Action::Select(text_editor::Motion::DocumentEnd),
        text_editor::Action::Edit(text_editor::Edit::Paste(Arc::new(text.into()))),
    ] {
        drop(app.update(Message::ChatUi(Action::Composer(target, edit))));
    }
}

#[test]
fn peer_remove_retains_ready_image_only_mounted_and_parked_drafts_after_writer_reopen() {
    let _admission = attachment_draft::test_admission();
    chat::attachment_assets::with_test_image(|image| {
        for parked in [false, true] {
            let mut fixture = Fixture::new();
            fixture.image_only(&image);
            let allocation = runtime(&fixture.app, fixture.card) as *const Runtime;
            if parked {
                fixture.park();
            }
            fixture.assert_image_only(&image);
            let database = fixture.directory.path().join("state.sqlite3");
            let boot = BootId::new("image-only-draft-test");
            let (writer, _) = SessionRuntime::open(&database, boot.clone()).unwrap();
            writer.save_project(ProjectState {
                id: fixture.project,
                generation: 0,
                name: "Image draft".into(),
                repository: fixture.directory.path().into(),
                kind: ProjectKind::Folder,
                existence: Existence::Open,
            });
            fixture.app.session = Some(writer);
            let removed = fixture.peer_remove();
            fixture.observe(removed);
            assert_eq!(
                runtime(&fixture.app, fixture.card) as *const Runtime,
                allocation
            );
            assert_eq!(
                fixture.app.chat_ui.parked.contains_key(&fixture.card),
                parked
            );
            fixture.assert_retained_receipt(&image);
            fixture.app.persist();
            let mut writer = fixture.app.session.take().unwrap();
            writer.flush(Duration::from_secs(10)).unwrap();
            assert_eq!(writer.last_error(), None);
            writer.shutdown();
            drop(writer);
            let (mut reopened, restored) = SessionRuntime::open(&database, boot).unwrap();
            let restored = restored.expect("saved workspace");
            assert!(
                restored
                    .cards
                    .iter()
                    .any(|card| card.id == fixture.card && card.existence.is_open())
            );
            assert!(restored.local_only_cards.contains(&fixture.card));
            assert!(!restored.chat_drafts.contains_key(&fixture.card));
            assert_eq!(
                restored.chat_draft_attachments[&fixture.card],
                [image.clone()]
            );
            chat::attachment_assets::validate(&restored.chat_draft_attachments[&fixture.card][0])
                .unwrap();
            reopened.shutdown();
            assert_eq!(reopened.last_error(), None);
        }
    });
}

#[test]
fn ready_image_only_alias_blocks_canonical_adoption() {
    let _admission = attachment_draft::test_admission();
    chat::attachment_assets::with_test_image(|image| {
        for parked in [false, true] {
            let mut fixture = Fixture::new();
            fixture.image_only(&image);
            if parked {
                fixture.park();
            }
            let mut model = fixture.model.clone();
            model.sequence += 1;
            let view = &mut model.views[0];
            view.revision += 1;
            let pane = view
                .panes
                .iter_mut()
                .find(|pane| pane.id == fixture.card)
                .unwrap();
            let canonical = CardId::new();
            pane.id = canonical;
            view.retired.push(fixture.card);
            fixture.observe(model);
            assert!(
                fixture
                    .app
                    .card_state(fixture.card)
                    .unwrap()
                    .existence
                    .is_open()
            );
            assert_eq!(
                runtime(&fixture.app, fixture.card).unsent_attachments(),
                [image.clone()]
            );
            assert!(
                !fixture
                    .app
                    .card_state(canonical)
                    .is_some_and(|card| card.existence.is_open())
            );
        }
    });
}

#[test]
fn ready_image_only_canonical_destination_refuses_draft_rekey() {
    let _admission = attachment_draft::test_admission();
    chat::attachment_assets::with_test_image(|image| {
        for parked in [false, true] {
            let mut fixture = Fixture::new();
            fixture.image_only(&image);
            if parked {
                fixture.park();
            }
            let alias = CardId::new();
            let incoming = chat::attachment_assets::test_image("alias image.png");
            fixture
                .app
                .chat_ui
                .drafts
                .insert(alias, "alias draft".into());
            fixture
                .app
                .chat_ui
                .draft_attachments
                .insert(alias, vec![incoming.clone()]);
            assert!(
                !fixture
                    .app
                    .can_rekey_shared_terminal_draft(alias, fixture.card)
            );
            assert!(!fixture.app.rekey_shared_terminal_draft(alias, fixture.card));
            assert_eq!(fixture.app.chat_ui.drafts[&alias], "alias draft");
            assert_eq!(fixture.app.chat_ui.draft_attachments[&alias], [incoming]);
            assert!(!fixture.app.chat_ui.drafts.contains_key(&fixture.card));
            assert!(
                !fixture
                    .app
                    .chat_ui
                    .draft_attachments
                    .contains_key(&fixture.card)
            );
            assert_eq!(
                runtime(&fixture.app, fixture.card).unsent_attachments(),
                [image.clone()]
            );
        }
    });
}

#[test]
fn history_browsing_preserves_unsent_image_stash_without_treating_sent_images_as_local_work() {
    let _admission = attachment_draft::test_admission();
    chat::attachment_assets::with_test_image(|image| {
        let mut fixture = Fixture::new();
        fixture.image_only(&image);
        let target = runtime(&fixture.app, fixture.card).target;
        fixture
            .app
            .chat_runtime_mut(target)
            .unwrap()
            .remember("previous prompt".into());
        drop(
            fixture
                .app
                .update(Message::ChatUi(Action::HistoryOlder(target))),
        );
        assert_eq!(
            runtime(&fixture.app, fixture.card).composer.text(),
            "previous prompt"
        );
        assert!(runtime(&fixture.app, fixture.card).attachments.is_empty());
        fixture.assert_image_only(&image);
        let removed = fixture.peer_remove();
        fixture.observe(removed);
        fixture.assert_retained_receipt(&image);
        drop(
            fixture
                .app
                .update(Message::ChatUi(Action::HistoryNewer(target))),
        );
        assert_eq!(runtime(&fixture.app, fixture.card).composer.text(), "");
        assert_eq!(
            runtime(&fixture.app, fixture.card).attachments,
            [image.clone()]
        );

        let mut sent_only = Fixture::new();
        let target = runtime(&sent_only.app, sent_only.card).target;
        sent_only
            .app
            .chat_runtime_mut(target)
            .unwrap()
            .remember_with_attachments("previous image prompt".into(), vec![image.clone()]);
        drop(
            sent_only
                .app
                .update(Message::ChatUi(Action::HistoryOlder(target))),
        );
        assert_eq!(runtime(&sent_only.app, sent_only.card).attachments, [image]);
        assert!(
            runtime(&sent_only.app, sent_only.card)
                .unsent_attachments()
                .is_empty()
        );
        assert!(!sent_only.app.shared_chat_has_local_work(sent_only.card));
        let removed = sent_only.peer_remove();
        sent_only.observe(removed);
        assert!(
            !sent_only
                .app
                .card_state(sent_only.card)
                .unwrap()
                .existence
                .is_open()
        );
    });
}

#[test]
fn explicit_image_removal_and_local_close_keep_their_discard_semantics() {
    let _admission = attachment_draft::test_admission();
    chat::attachment_assets::with_test_image(|image| {
        let mut fixture = Fixture::new();
        fixture.image_only(&image);
        let target = runtime(&fixture.app, fixture.card).target;
        drop(
            fixture
                .app
                .update(Message::ChatUi(Action::RemoveAttachment(target, 0))),
        );
        assert!(
            runtime(&fixture.app, fixture.card)
                .unsent_attachments()
                .is_empty()
        );
        assert!(!fixture.app.shared_chat_has_local_work(fixture.card));
        let removed = fixture.peer_remove();
        fixture.observe(removed);
        assert!(
            !fixture
                .app
                .card_state(fixture.card)
                .unwrap()
                .existence
                .is_open()
        );
        assert!(
            !fixture
                .app
                .persistent_workspace()
                .chat_draft_attachments
                .contains_key(&fixture.card)
        );

        let mut local_close = Fixture::new();
        local_close.image_only(&image);
        drop(local_close.app.update(Message::CloseCard(local_close.card)));
        assert!(
            !local_close
                .app
                .card_state(local_close.card)
                .unwrap()
                .existence
                .is_open()
        );
        assert!(!local_close.app.cards.contains_key(&local_close.card));
        assert!(
            !local_close
                .app
                .chat_ui
                .parked
                .contains_key(&local_close.card)
        );
        assert!(
            !local_close
                .app
                .persistent_workspace()
                .chat_draft_attachments
                .contains_key(&local_close.card)
        );
        // Discarding a pane association is not an assertion of blob deletion.
        chat::attachment_assets::validate(&image).unwrap();
    });
}

#[test]
fn pending_images_and_text_remain_local_work_while_an_empty_draft_does_not() {
    let _admission = attachment_draft::test_admission();
    let mut fixture = Fixture::new();
    assert!(!fixture.app.shared_chat_has_local_work(fixture.card));
    let target = runtime(&fixture.app, fixture.card).target;
    let ticket = fixture
        .app
        .chat_runtime_mut(target)
        .unwrap()
        .begin_attachment_insert();
    assert!(
        runtime(&fixture.app, fixture.card)
            .unsent_attachments()
            .is_empty()
    );
    assert!(fixture.app.shared_chat_has_local_work(fixture.card));
    let removed = fixture.peer_remove();
    fixture.observe(removed);
    assert!(
        fixture
            .app
            .card_state(fixture.card)
            .unwrap()
            .existence
            .is_open()
    );
    drop(fixture.app.update(Message::ChatUi(Action::ImageSaved(
        target,
        ticket,
        Err("cancelled fixture image".into()),
    ))));
    replace_text(&mut fixture.app, fixture.card, "keep text");
    assert!(fixture.app.shared_chat_has_local_work(fixture.card));
    assert_eq!(
        fixture.app.persistent_workspace().chat_drafts[&fixture.card],
        "keep text"
    );
    replace_text(&mut fixture.app, fixture.card, "");
    assert!(!fixture.app.shared_chat_has_local_work(fixture.card));
}
