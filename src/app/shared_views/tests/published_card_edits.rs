use super::*;
use crate::{agent::Provider, app::chat_ui::Action};

fn chat_session(app: &Cagents, card: CardId) -> crate::chat::Session {
    match &app.card_state(card).unwrap().kind {
        CardKind::Chat(session) => session.clone(),
        other => panic!("the card holds a chat: {other:?}"),
    }
}

/// A window with a session writer showing one shared channel.
fn shared_channel_app() -> (Cagents, Scope, tempfile::TempDir) {
    let mut app = super::super::super::tests::context_inspector_test_app(false);
    let directory = tempfile::tempdir().unwrap();
    let (writer, _) = crate::session::SessionRuntime::open(
        directory.path().join("state.sqlite3"),
        crate::model::BootId::new("boot-test"),
    )
    .unwrap();
    app.session = Some(writer);
    let channel = ChannelId::new();
    let durable = Arc::make_mut(&mut app.durable);
    durable.channels.push(ProjectChannel {
        id: channel,
        project_id: ProjectId::new(),
        title: "view".into(),
        title_source: Default::default(),
        ordinal: 1,
        shared: true,
        cards: vec![],
        focused: None,
    });
    durable.active_channel = Some(channel);
    app.workspace_model.identity = uuid::Uuid::new_v4();
    let scope = Scope::Channel(channel);
    app.shared_views.initialized.insert(scope.clone());
    (app, scope, directory)
}

/// Saves everything queued, asserting storage accepted it.
fn flushed(app: &Cagents) -> Arc<crate::persistence::StateStore> {
    let writer = app.session.as_ref().unwrap();
    writer.flush(std::time::Duration::from_secs(10)).unwrap();
    assert_eq!(writer.last_error(), None);
    writer.operation_store()
}

fn stored(app: &Cagents, card: CardId) -> CardState {
    flushed(app).load_card(card, None).unwrap().unwrap()
}

/// The registry accepts the window's panes as they are, and acknowledges
/// every edit published so far.
fn accept_local_view(app: &mut Cagents, scope: &Scope) -> View {
    let Scope::Channel(channel) = scope else {
        unreachable!()
    };
    let mut view = View::empty(*channel);
    view.revision = 2;
    view.panes = app.local_views().panes.remove(scope).unwrap();
    app.workspace_model.views = vec![view.clone()];
    app.shared_views.pending.clear();
    view
}

/// Any other change to the view, here another client renaming its first
/// pane, projects the registry's view before it acknowledges this window's
/// edit.
fn rename_elsewhere(app: &mut Cagents, mut view: View) -> CardId {
    view.revision += 1;
    view.panes[0].title = "Renamed elsewhere".into();
    let renamed = view.panes[0].id;
    app.workspace_model.views = vec![view];
    drop(app.reconcile_shared_views());
    assert_eq!(app.card_state(renamed).unwrap().title, "Renamed elsewhere");
    renamed
}

fn publishes(app: &Cagents, scope: &Scope, card: CardId, kind: &CardKind) -> bool {
    app.shared_views.pending.iter().any(|(_, pending, change)| {
        pending == scope
            && matches!(change, Change::Update { pane, .. } if pane.id == card && pane.kind == *kind)
    })
}

/// A launch option chosen on a draft is this window's own edit to the shared
/// pane, so it is published like any other. An unpublished choice was undone
/// by the next shared projection, which saved the earlier recipe over the one
/// already stored; storage refused that and every later save of the window.
#[test]
fn a_draft_account_choice_survives_the_next_shared_projection() {
    let (mut app, scope, _directory) = shared_channel_app();
    let managed = app.account_ui.add_managed_fixture(Provider::Claude, 24);

    // The reported pane: an unused copy of a chat beside it.
    let _ = app.new_native_chat(Provider::Claude, Some("/workspace/chat-fixture".into()));
    let source = app.workspace.focused().unwrap();
    drop(app.duplicate_pane(app.card_state(source).unwrap().clone()));
    let copy = app.workspace.focused().unwrap();
    assert_ne!(copy, source);
    let original = chat_session(&app, copy);
    assert!(original.is_unstarted());
    assert_eq!(stored(&app, copy).kind, CardKind::Chat(original.clone()));
    let view = accept_local_view(&mut app, &scope);
    assert_eq!(view.panes.len(), 2);

    let target = match app.cards.get(&copy) {
        Some(CardRuntime::Chat(runtime)) => runtime.target,
        _ => panic!("the copy opens as a draft"),
    };
    let _ = app.update(Message::ChatUi(Action::ChooseAccount(target, managed)));
    let chosen = chat_session(&app, copy);
    assert_eq!(chosen.id, original.id);
    assert_ne!(chosen.launch.recipe, original.launch.recipe);
    assert!(
        publishes(&app, &scope, copy, &CardKind::Chat(chosen.clone())),
        "the choice is published to the shared pane"
    );

    let renamed = rename_elsewhere(&mut app, view);
    assert_eq!(renamed, source);
    assert_eq!(chat_session(&app, copy), chosen);
    assert_eq!(stored(&app, copy).kind, CardKind::Chat(chosen));
    assert_eq!(stored(&app, source).title, "Renamed elsewhere");
}

/// A chat handed to its terminal CLI becomes a terminal pane in the shared
/// view too, rather than turning back into the chat on the next projection
/// while storage holds the terminal.
#[test]
fn a_chat_handed_to_its_terminal_stays_a_terminal_after_the_next_shared_projection() {
    let (mut app, scope, _directory) = shared_channel_app();
    let _ = app.new_native_chat(Provider::Claude, Some("/workspace/chat-fixture".into()));
    let card = app.workspace.focused().unwrap();
    // The provider reports the conversation, which the CLI then resumes.
    let Some(CardKind::Chat(chat)) = app.card_state_mut(card).map(|state| &mut state.kind) else {
        panic!("the card holds a chat");
    };
    chat.launch.session_id = Some(uuid::Uuid::new_v4().to_string());
    let resume = chat.launch.terminal_recipe().unwrap();
    app.persist();
    assert!(matches!(stored(&app, card).kind, CardKind::Chat(_)));
    let view = accept_local_view(&mut app, &scope);

    let terminal = crate::app::fresh_terminal_session(resume);
    drop(app.finish_chat_terminal_handoff(card, terminal.clone()));
    let handed = CardKind::Terminal(terminal);
    assert!(
        publishes(&app, &scope, card, &handed),
        "the handoff is published to the shared pane"
    );

    rename_elsewhere(&mut app, view);
    assert_eq!(app.card_state(card).unwrap().kind, handed);
    let saved = stored(&app, card);
    assert_eq!(saved.kind, handed);
    assert_eq!(saved.title, "Renamed elsewhere");
}

/// Launch options another window chose for a draft both windows show reach
/// this window's draft too: it shows them, starts on them, and builds its own
/// next choice on them rather than on the options it had.
#[test]
fn a_draft_shows_and_starts_on_options_chosen_in_another_window() {
    let (mut app, scope, _directory) = shared_channel_app();
    let _ = app.new_native_chat(Provider::Claude, Some("/workspace/chat-fixture".into()));
    let card = app.workspace.focused().unwrap();
    let original = chat_session(&app, card);
    assert!(original.is_unstarted());
    let (shown, _) = crate::app::chat_ui::draft_launch_for_test(&app, card);
    assert_ne!(shown.as_deref(), Some("plan"));
    let mut view = accept_local_view(&mut app, &scope);

    // The other window picks another account and the plan mode.
    let mut chosen = original.clone();
    chosen
        .launch
        .recipe
        .environment
        .insert("CLAUDE_CONFIG_DIR".into(), "/accounts/claude/other".into());
    crate::chat::apply_claude_permission_mode(&mut chosen.launch.recipe, "plan");
    view.revision += 1;
    view.panes[0].kind = CardKind::Chat(chosen.clone());
    app.workspace_model.views = vec![view];
    drop(app.reconcile_shared_views());

    assert_eq!(chat_session(&app, card), chosen);
    let (shown, session) = crate::app::chat_ui::draft_launch_for_test(&app, card);
    assert_eq!(shown.as_deref(), Some("plan"));
    assert_eq!(session, chosen);
    assert!(
        app.shared_views.pending.is_empty(),
        "nothing is published back"
    );

    // This window's own next choice keeps the other window's account.
    let target = match app.cards.get(&card) {
        Some(CardRuntime::Chat(runtime)) => runtime.target,
        _ => panic!("the card opens as a draft"),
    };
    let _ = app.update(Message::ChatUi(Action::PermissionMode(
        target,
        "acceptEdits".into(),
    )));
    let (shown, session) = crate::app::chat_ui::draft_launch_for_test(&app, card);
    assert_eq!(shown.as_deref(), Some("acceptEdits"));
    assert_eq!(
        session.launch.recipe.environment.get("CLAUDE_CONFIG_DIR"),
        chosen.launch.recipe.environment.get("CLAUDE_CONFIG_DIR")
    );
    assert_eq!(stored(&app, card).kind, CardKind::Chat(session.clone()));

    // Its first message starts on them.
    let started = crate::app::chat_ui::start_draft_for_test(&mut app, card);
    assert_eq!(started.launch.recipe, session.launch.recipe);
}

fn listed_model(id: &str) -> crate::chat::Model {
    crate::chat::Model {
        id: id.into(),
        label: id.into(),
        reasoning: vec!["low".into(), "high".into()],
    }
}

struct StoredDraftWindow {
    app: Cagents,
    directory: tempfile::TempDir,
    published: async_channel::Receiver<ModelEdit>,
    draft: &'static str,
}

impl StoredDraftWindow {
    fn assert_draft(&mut self, card: CardId, expected: &crate::chat::Session) {
        assert!(expected.is_unstarted());
        assert_eq!(chat_session(&self.app, card), *expected);
        // This helper also requires a draft connection with no queued send.
        assert_eq!(
            crate::app::chat_ui::draft_launch_for_test(&self.app, card).1,
            *expected
        );
        assert_eq!(
            crate::app::chat_ui::draft_model_for_test(&self.app, card),
            expected.launch.draft_model
        );
        assert!(!self.app.chat_start_pending(card));
        assert!(!self.app.chat_ui.transition_pending());
        assert!(self.app.durable.pending_chat_admissions.is_empty());
        assert!(self.app.shared_views.closed_chats.is_empty());
        assert_eq!(
            self.app.persistent_workspace().chat_drafts.get(&card),
            Some(&self.draft.to_owned())
        );
    }

    fn assert_quiet(&self) {
        assert!(self.app.shared_views.pending.is_empty());
        assert!(
            self.published.try_recv().is_err(),
            "adopting or acknowledging a shared choice must not publish another edit"
        );
    }

    fn reopen(&mut self, card: CardId, expected: &crate::chat::Session) {
        // Finish the real writer before reopening its database. In particular,
        // the original source must have landed before testing a provider move;
        // coalescing it with the replacement would conceal storage's refusal.
        assert_eq!(
            stored(&self.app, card).kind,
            CardKind::Chat(expected.clone())
        );
        let mut writer = self.app.session.take().unwrap();
        writer.shutdown();
        drop(writer);
        let (writer, restored) = crate::session::SessionRuntime::open(
            self.directory.path().join("state.sqlite3"),
            crate::model::BootId::new("boot-test"),
        )
        .unwrap();
        assert!(writer.restore_warnings().is_empty());
        assert_eq!(writer.last_error(), None);
        let restored = restored.expect("the shared draft workspace was saved");
        assert_eq!(restored.id, self.app.durable.id);
        assert_eq!(restored.cards.len(), 1);
        assert_eq!(restored.cards[0].id, card);
        assert_eq!(restored.cards[0].kind, CardKind::Chat(expected.clone()));
        assert!(restored.cards[0].existence.is_open());
        assert_eq!(
            restored.chat_drafts.get(&card),
            Some(&self.draft.to_owned())
        );
        assert!(restored.pending_chat_admissions.is_empty());
        self.app.session = Some(writer);
        self.assert_draft(card, expected);
    }
}

/// Two windows start from the same valid descriptor but own independent
/// databases, writers and unsent text. No provider or daemon task is polled.
fn stored_draft_windows() -> ([StoredDraftWindow; 2], Scope, View, CardId) {
    let (mut author, scope, author_directory) = shared_channel_app();
    let (mut peer, _, peer_directory) = shared_channel_app();
    let project = author_directory.path().join("project");
    std::fs::create_dir_all(project.join(".git")).unwrap();
    drop(author.new_native_chat(Provider::Claude, Some(project)));
    let card = author.workspace.focused().unwrap();
    let original = chat_session(&author, card);
    let view = accept_local_view(&mut author, &scope);
    assert_eq!(view.panes.len(), 1);

    peer.durable = author.persistent_workspace();
    peer.workspace = Cagents::rebuild_workspace(&peer.durable);
    peer.workspace_model = author.workspace_model.clone();
    peer.shared_views = State::for_workspace(&peer.durable);
    peer.shared_views.initialized.insert(scope.clone());
    drop(peer.reconcile_shared_views());

    let mut windows = [
        (
            author,
            author_directory,
            "Window A keeps this unsent draft.",
        ),
        (
            peer,
            peer_directory,
            "Window B keeps a different unsent draft.",
        ),
    ]
    .map(|(mut app, directory, draft)| {
        for provider in [Provider::Claude, Provider::Omp] {
            drop(app.observe_listed_models(
                provider,
                vec![listed_model("model-a"), listed_model("model-b")],
            ));
        }
        let Some(CardRuntime::Chat(runtime)) = app.cards.get(&card) else {
            panic!("the shared pane opens as a draft");
        };
        let target = runtime.target;
        drop(app.handle_chat_ui(Action::Composer(
            target,
            cosmic::widget::text_editor::Action::Edit(cosmic::widget::text_editor::Edit::Paste(
                Arc::new(draft.into()),
            )),
        )));
        app.persist();
        let (sender, published) = async_channel::unbounded();
        app.workspace_model_tx = sender;
        StoredDraftWindow {
            app,
            directory,
            published,
            draft,
        }
    });
    for window in &mut windows {
        window.reopen(card, &original);
        window.assert_quiet();
    }
    (windows, scope, view, card)
}

/// Accept the actual typed edits the choosing window published, rather than
/// constructing a descriptor that bypasses its model/effort reducer.
fn accept_draft_choice(window: &mut StoredDraftWindow, view: &mut View, card: CardId) {
    let changes = window.app.shared_views.pending.clone();
    assert!(!changes.is_empty(), "the local choice must be published");
    let mut published = Vec::new();
    while let Ok(edit) = window.published.try_recv() {
        let ModelEdit::ChangeView {
            operation,
            scope,
            change,
            ..
        } = edit
        else {
            panic!("a draft choice publishes only its pane update");
        };
        published.push((operation, scope, change));
    }
    assert_eq!(published, changes);
    for (operation, scope, change) in published {
        assert_eq!(scope, view.scope);
        assert!(matches!(&change, Change::Update { pane, .. } if pane.id == card));
        assert!(view.apply(&change).unwrap());
        view.revision += 1;
        window.app.acknowledge_view_edit(scope, Some(operation));
    }
    assert!(window.app.shared_views.pending.is_empty());
}

#[test]
fn shared_draft_choices_survive_independent_writers_and_reopen_without_republishing() {
    let (mut windows, _scope, mut view, card) = stored_draft_windows();
    let original = chat_session(&windows[0].app, card);
    // Alternate the choosing window: both writers must accept a passive
    // provider move, as well as model, effort and provider-default updates.
    for (owner, provider, model, reasoning) in [
        (0, Provider::Claude, Some("model-a"), Some("high")),
        (1, Provider::Claude, Some("model-b"), Some("high")),
        (0, Provider::Claude, Some("model-b"), Some("low")),
        (1, Provider::Omp, Some("model-b"), Some("high")),
        (0, Provider::Claude, Some("model-a"), Some("high")),
        (1, Provider::Claude, None, None),
    ] {
        let app = &mut windows[owner].app;
        let Some(CardRuntime::Chat(runtime)) = app.cards.get(&card) else {
            panic!("the pane remains a draft");
        };
        let target = runtime.target;
        drop(app.handle_chat_ui(Action::DraftModel(
            target,
            provider,
            model.map(str::to_owned),
        )));
        if let (Some(model), Some(reasoning)) = (model, reasoning) {
            drop(app.handle_chat_ui(Action::Reasoning(target, model.into(), reasoning.into())));
        }
        let chosen = chat_session(app, card);
        assert_eq!(chosen.id, original.id);
        assert_eq!(chosen.launch.provider, provider);
        assert_eq!(
            chosen.launch.draft_model,
            model.map(|model| crate::chat::DraftModel {
                model: model.into(),
                reasoning: reasoning.map(str::to_owned),
            })
        );
        accept_draft_choice(&mut windows[owner], &mut view, card);
        assert_eq!(view.panes[0].kind, CardKind::Chat(chosen.clone()));

        for window in &mut windows {
            window.app.workspace_model.views = vec![view.clone()];
            drop(window.app.reconcile_shared_views());
            window.assert_draft(card, &chosen);
            window.assert_quiet();
            window.reopen(card, &chosen);
            // Repeating the same watcher snapshot and a later ordinary save
            // neither quarantines the writer nor echoes the accepted choice.
            drop(window.app.reconcile_shared_views());
            window.app.persist();
            assert_eq!(
                stored(&window.app, card).kind,
                CardKind::Chat(chosen.clone())
            );
            window.assert_draft(card, &chosen);
            window.assert_quiet();
        }
    }
}

/// Poll only the real writer's receipt, never a provider-start task. Keep its
/// reducer message undelivered so a peer projection can supersede the save.
fn saved_draft_receipt(task: Task<Message>) -> Message {
    use cosmic::iced::futures::StreamExt;

    std::thread::spawn(move || {
        let executor = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        executor.block_on(async {
            let mut stream = cosmic::iced::runtime::task::into_stream(task).expect("save task");
            while let Some(action) =
                tokio::time::timeout(std::time::Duration::from_secs(5), stream.next())
                    .await
                    .expect("the real writer must settle")
            {
                if let cosmic::iced::runtime::Action::Output(cosmic::Action::App(
                    message @ Message::ChatUi(Action::DraftStartCheckpointed(..)),
                )) = action
                {
                    return message;
                }
            }
            panic!("save task did not return its checkpoint receipt")
        })
    })
    .join()
    .expect("the save-only executor must finish")
}

#[test]
fn a_peer_provider_choice_supersedes_a_saved_first_send_without_replaying_its_receipt() {
    let ([mut saving, mut peer], _scope, mut view, card) = stored_draft_windows();
    let original = chat_session(&saving.app, card);
    let Some(CardRuntime::Chat(runtime)) = saving.app.cards.get(&card) else {
        panic!("the pane opens as a draft");
    };
    let target = runtime.target;
    let save = saving
        .app
        .handle_chat_ui(Action::Submit(target, crate::chat::PromptMode::Send));
    assert!(saving.app.chat_start_pending(card));
    let receipt = saved_draft_receipt(save);
    assert!(matches!(
        &receipt,
        Message::ChatUi(Action::DraftStartCheckpointed(_, _, Ok(())))
    ));
    let CardKind::Chat(candidate) = stored(&saving.app, card).kind else {
        panic!("the first-send checkpoint saved a chat");
    };
    assert_ne!(candidate.id, original.id);
    assert_eq!(candidate.launch, original.launch);
    assert!(candidate.is_unstarted());
    assert_eq!(chat_session(&saving.app, card), original);
    saving.assert_quiet();

    // The peer still sees source A. Its ordinary provider selection publishes
    // C while the first window has saved B but not accepted B's receipt.
    let Some(CardRuntime::Chat(runtime)) = peer.app.cards.get(&card) else {
        panic!("the peer still shows the original draft");
    };
    let peer_target = runtime.target;
    drop(peer.app.handle_chat_ui(Action::DraftModel(
        peer_target,
        Provider::Omp,
        Some("model-b".into()),
    )));
    drop(peer.app.handle_chat_ui(Action::Reasoning(
        peer_target,
        "model-b".into(),
        "high".into(),
    )));
    let chosen = chat_session(&peer.app, card);
    assert_eq!(chosen.id, original.id);
    assert_eq!(chosen.launch.provider, Provider::Omp);
    accept_draft_choice(&mut peer, &mut view, card);
    for window in [&mut peer, &mut saving] {
        window.app.workspace_model.views = vec![view.clone()];
        drop(window.app.reconcile_shared_views());
        window.assert_draft(card, &chosen);
        window.assert_quiet();
        assert_eq!(
            stored(&window.app, card).kind,
            CardKind::Chat(chosen.clone())
        );
    }

    // The superseded success carries no remaining permission to start B or
    // publish it over C, even though its real save had already succeeded.
    drop(saving.app.update(receipt));
    for window in [&mut saving, &mut peer] {
        window.app.persist();
        window.assert_draft(card, &chosen);
        window.assert_quiet();
        window.reopen(card, &chosen);
    }
}

#[test]
fn a_draft_model_and_effort_choice_survive_the_next_shared_projection() {
    let (mut app, scope, _directory) = shared_channel_app();
    drop(app.observe_listed_models(Provider::Codex, vec![listed_model("shared-model")]));
    drop(app.new_native_chat(Provider::Codex, Some("/workspace/chat-fixture".into())));
    let card = app.workspace.focused().unwrap();
    let original = chat_session(&app, card);
    let view = accept_local_view(&mut app, &scope);
    let Some(CardRuntime::Chat(runtime)) = app.cards.get(&card) else {
        panic!("the card opens as a draft");
    };
    let target = runtime.target;
    drop(app.update(Message::ChatUi(Action::DraftModel(
        target,
        Provider::Codex,
        Some("shared-model".into()),
    ))));
    let model = chat_session(&app, card);
    assert!(publishes(&app, &scope, card, &CardKind::Chat(model)));
    drop(app.update(Message::ChatUi(Action::Reasoning(
        target,
        "shared-model".into(),
        "high".into(),
    ))));
    let chosen = chat_session(&app, card);
    assert_eq!(chosen.id, original.id);
    assert_eq!(chosen.launch.recipe, original.launch.recipe);
    let expected = Some(crate::chat::DraftModel {
        model: "shared-model".into(),
        reasoning: Some("high".into()),
    });
    assert_eq!(chosen.launch.draft_model, expected);
    assert!(publishes(
        &app,
        &scope,
        card,
        &CardKind::Chat(chosen.clone())
    ));
    rename_elsewhere(&mut app, view);
    assert_eq!(chat_session(&app, card), chosen);
    assert_eq!(
        crate::app::chat_ui::draft_model_for_test(&app, card),
        expected
    );
    assert_eq!(stored(&app, card).kind, CardKind::Chat(chosen));
}

#[test]
fn a_draft_shares_model_effort_and_default_across_provider_moves_and_starts_on_them() {
    for provider in [Provider::Claude, Provider::Codex, Provider::Omp] {
        for choice in [
            Some(crate::chat::DraftModel {
                model: "shared-model".into(),
                reasoning: Some("high".into()),
            }),
            None,
        ] {
            let (mut app, scope, _directory) = shared_channel_app();
            drop(app.observe_listed_models(
                provider,
                vec![listed_model("window-model"), listed_model("shared-model")],
            ));
            // This window remembers a different model for the destination
            // provider. Neither that preference nor its current draft wins.
            drop(app.new_native_chat(provider, Some("/workspace/chat-fixture".into())));
            let remembered = app.workspace.focused().unwrap();
            let Some(CardRuntime::Chat(runtime)) = app.cards.get(&remembered) else {
                panic!("the card opens as a draft");
            };
            let target = runtime.target;
            drop(app.update(Message::ChatUi(Action::DraftModel(
                target,
                provider,
                Some("window-model".into()),
            ))));
            drop(app.new_native_chat(Provider::Claude, Some("/workspace/chat-fixture".into())));
            let card = app.workspace.focused().unwrap();
            let original = chat_session(&app, card);
            let mut view = accept_local_view(&mut app, &scope);
            let mut chosen = crate::chat::fresh_session(
                provider,
                original.id,
                original.launch.recipe.working_directory.clone(),
                &crate::account::Account::system(provider),
            )
            .unwrap();
            chosen.launch.draft_model = choice.clone();
            view.revision += 1;
            view.panes
                .iter_mut()
                .find(|pane| pane.id == card)
                .unwrap()
                .kind = CardKind::Chat(chosen.clone());
            app.workspace_model.views = vec![view];
            drop(app.reconcile_shared_views());
            assert_eq!(
                crate::app::chat_ui::draft_model_for_test(&app, card),
                choice
            );
            assert_eq!(
                crate::app::chat_ui::draft_launch_for_test(&app, card).1,
                chosen
            );
            assert!(
                app.shared_views.pending.is_empty(),
                "adoption does not republish"
            );
            assert_eq!(stored(&app, card).kind, CardKind::Chat(chosen.clone()));
            let started = crate::app::chat_ui::start_draft_for_test(&mut app, card);
            assert_eq!(started.launch, chosen.launch);
        }
    }
}

#[test]
fn an_adopted_draft_keeps_its_failure_reason_until_its_account_changes() {
    let (mut app, scope, _directory) = shared_channel_app();
    let _ = app.new_native_chat(Provider::Claude, Some("/workspace/chat-fixture".into()));
    let card = app.workspace.focused().unwrap();
    let mut chosen = chat_session(&app, card);
    let mut view = accept_local_view(&mut app, &scope);
    let reason = Some("Claude is not signed in.".to_owned());
    *crate::app::chat_ui::draft_error_for_test(&mut app, card) = reason.clone();
    let mut publish = |app: &mut crate::app::Cagents, session: &crate::chat::Session| {
        view.revision += 1;
        view.panes[0].kind = CardKind::Chat(session.clone());
        app.workspace_model.views = vec![view.clone()];
        drop(app.reconcile_shared_views());
        assert_eq!(chat_session(app, card), *session);
    };

    // Another permission mode is still the account that failed.
    crate::chat::apply_claude_permission_mode(&mut chosen.launch.recipe, "plan");
    publish(&mut app, &chosen);
    assert_eq!(
        *crate::app::chat_ui::draft_error_for_test(&mut app, card),
        reason
    );

    // Another account may not fail the same way.
    chosen
        .launch
        .recipe
        .environment
        .insert("CLAUDE_CONFIG_DIR".into(), "/accounts/claude/other".into());
    publish(&mut app, &chosen);
    assert_eq!(
        *crate::app::chat_ui::draft_error_for_test(&mut app, card),
        None
    );
}

#[test]
fn a_draft_choosing_a_file_takes_up_options_chosen_meanwhile_in_another_window() {
    let (mut app, scope, _directory) = shared_channel_app();
    let _ = app.new_native_chat(Provider::Claude, Some("/workspace/chat-fixture".into()));
    let card = app.workspace.focused().unwrap();
    let original = chat_session(&app, card);
    let mut view = accept_local_view(&mut app, &scope);
    let mut chosen = original.clone();
    chosen
        .launch
        .recipe
        .environment
        .insert("CLAUDE_CONFIG_DIR".into(), "/accounts/claude/other".into());
    crate::chat::apply_claude_permission_mode(&mut chosen.launch.recipe, "plan");

    crate::app::chat_ui::with_attachment_chooser_open_for_test(&mut app, card, |app| {
        view.revision += 1;
        view.panes[0].kind = CardKind::Chat(chosen.clone());
        app.workspace_model.views = vec![view];
        drop(app.reconcile_shared_views());
        assert_eq!(stored(app, card).kind, CardKind::Chat(chosen.clone()));
    });

    // Once the chooser closes the draft shows, and would start on, the other
    // window's options, rather than publishing its own older ones over them.
    let (shown, session) = crate::app::chat_ui::draft_launch_for_test(&app, card);
    assert_eq!(shown.as_deref(), Some("plan"));
    assert_eq!(session, chosen);
    assert!(app.shared_views.pending.is_empty());
    let started = crate::app::chat_ui::start_draft_for_test(&mut app, card);
    assert_eq!(started.launch.recipe, chosen.launch.recipe);
}
