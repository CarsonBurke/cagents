use super::*;
use crate::{agent::Provider, app::chat_ui::Action};

fn chat_session(app: &Cagents, card: CardId) -> crate::chat::Session {
    match &app.card_state(card).unwrap().kind {
        CardKind::Chat(session) => session.clone(),
        other => panic!("the card holds a chat: {other:?}"),
    }
}

/// A launch option chosen on a draft is this window's own edit to the shared
/// pane, so it is published like any other. An unpublished choice was undone
/// by the next shared projection, which saved the earlier recipe over the one
/// already stored; storage refused that and every later save of the window.
#[test]
fn a_draft_account_choice_survives_the_next_shared_projection() {
    let mut app = super::super::super::tests::context_inspector_test_app(false);
    let directory = tempfile::tempdir().unwrap();
    let (writer, _) = crate::session::SessionRuntime::open(
        directory.path().join("state.sqlite3"),
        crate::model::BootId::new("boot-test"),
    )
    .unwrap();
    app.session = Some(writer);
    let managed = app.account_ui.add_managed_fixture(Provider::Claude, 24);
    let channel = ChannelId::new();
    let scope = Scope::Channel(channel);
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
    app.shared_views.initialized.insert(scope.clone());

    // The reported pane: an unused copy of a chat beside it.
    let _ = app.new_native_chat(Provider::Claude, Some("/workspace/chat-fixture".into()));
    let source = app.workspace.focused().unwrap();
    drop(app.duplicate_pane(app.card_state(source).unwrap().clone()));
    let copy = app.workspace.focused().unwrap();
    assert_ne!(copy, source);
    let original = chat_session(&app, copy);
    assert!(original.is_unstarted());
    let flushed = |app: &Cagents| {
        let writer = app.session.as_ref().unwrap();
        writer.flush(std::time::Duration::from_secs(10)).unwrap();
        assert_eq!(writer.last_error(), None);
        writer.operation_store()
    };
    let stored = |app: &Cagents, card| flushed(app).load_card(card, None).unwrap().unwrap();
    assert_eq!(stored(&app, copy).kind, CardKind::Chat(original.clone()));

    // The registry accepts both inserts.
    let mut view = View::empty(channel);
    view.revision = 2;
    view.panes = app.local_views().panes.remove(&scope).unwrap();
    assert_eq!(view.panes.len(), 2);
    app.workspace_model.views = vec![view.clone()];
    app.shared_views.pending.clear();

    let target = match app.cards.get(&copy) {
        Some(CardRuntime::Chat(runtime)) => runtime.target,
        _ => panic!("the copy opens as a draft"),
    };
    let _ = app.update(Message::ChatUi(Action::ChooseAccount(target, managed)));
    let chosen = chat_session(&app, copy);
    assert_eq!(chosen.id, original.id);
    assert_ne!(chosen.launch.recipe, original.launch.recipe);
    assert!(
        app.shared_views.pending.iter().any(|(_, pending, change)| {
            *pending == scope
                && matches!(change, Change::Update { pane, .. }
                    if pane.id == copy && pane.kind == CardKind::Chat(chosen.clone()))
        }),
        "the choice is published to the shared pane"
    );

    // Any other change to the view, here another client renaming the
    // source, projects the registry's view before it acknowledges the edit.
    view.revision += 1;
    view.panes[0].title = "Renamed elsewhere".into();
    app.workspace_model.views = vec![view];
    drop(app.reconcile_shared_views());
    assert_eq!(chat_session(&app, copy), chosen);
    assert_eq!(app.card_state(source).unwrap().title, "Renamed elsewhere");

    assert_eq!(stored(&app, copy).kind, CardKind::Chat(chosen));
    assert_eq!(stored(&app, source).title, "Renamed elsewhere");
}
