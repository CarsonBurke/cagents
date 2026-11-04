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
