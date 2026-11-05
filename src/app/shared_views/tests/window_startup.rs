use super::*;

fn loading_channel() -> (Cagents, ChannelId, ChannelId) {
    let (mut app, channel, other, _) = fixture();
    app.active_workspace = Some(PathBuf::from("/repo"));
    Arc::make_mut(&mut app.durable).active_channel = Some(channel);
    app.shared_views
        .initialized
        .remove(&Scope::Channel(channel));
    app.workspace_model.identity = crate::workspace_model::NO_REGISTRY;
    app.settings.default_channel_pane = settings::DefaultPane::NativeChat;
    (app, channel, other)
}

fn peer_draft() -> Pane {
    let mut app = super::super::super::tests::context_inspector_test_app(false);
    app.active_workspace = Some(PathBuf::from("/repo"));
    drop(app.new_native_chat(crate::agent::Provider::Claude, Some("/repo".into())));
    Pane::from_card(
        app.card_state(app.workspace.focused().unwrap()).unwrap(),
        None,
    )
    .unwrap()
}

#[test]
fn a_copied_empty_channel_waits_for_its_shared_panes_before_opening_a_default() {
    let (mut app, channel, _) = loading_channel();
    drop(app.ensure_default_pane());
    assert!(
        app.workspace.cards().is_empty(),
        "startup cannot offer a different composer before its channel loads"
    );
    assert_eq!(
        app.shared_views.deferred_default,
        Some((Scope::Channel(channel), "/repo".into()))
    );

    let incoming = peer_draft();
    app.workspace_model.identity = uuid::Uuid::new_v4();
    app.workspace_model.views = vec![View {
        panes: vec![incoming.clone()],
        ..View::empty(channel)
    }];
    // A registry read is not yet the scope's seed acknowledgement.
    drop(app.reconcile_shared_views());
    assert!(app.workspace.cards().is_empty());
    app.acknowledge_view_edit(Scope::Channel(channel), None);
    drop(app.reconcile_shared_views());
    assert_eq!(app.workspace.order().collect::<Vec<_>>(), [incoming.id]);
    assert_eq!(app.workspace.focused(), Some(incoming.id));
    assert!(!app.chat_is_unused_default(incoming.id));
    assert!(app.shared_views.deferred_default.is_none());
}

#[test]
fn a_proven_empty_channel_still_gets_exactly_one_default_composer() {
    let (mut app, channel, _) = loading_channel();
    drop(app.ensure_default_pane());
    app.workspace_model.identity = uuid::Uuid::new_v4();
    app.workspace_model.views = vec![View::empty(channel)];
    app.acknowledge_view_edit(Scope::Channel(channel), None);
    drop(app.reconcile_shared_views());
    let card = app
        .workspace
        .focused()
        .expect("the initialized empty channel opens its composer");
    assert!(app.chat_is_unused_default(card));
    for _ in 0..3 {
        drop(app.reconcile_shared_views());
    }
    assert_eq!(app.workspace.order().collect::<Vec<_>>(), [card]);
    assert!(app.shared_views.deferred_default.is_none());
}

#[test]
fn deferred_default_cannot_follow_navigation_to_another_channel_in_the_same_directory() {
    let (mut app, channel, other) = loading_channel();
    drop(app.ensure_default_pane());
    // Both scopes use /repo; matching the path alone would create the old
    // scope's automatic composer in this newly selected channel.
    Arc::make_mut(&mut app.durable).active_channel = Some(other);
    app.workspace_model.identity = uuid::Uuid::new_v4();
    app.workspace_model.views = vec![View::empty(channel), View::empty(other)];
    app.acknowledge_view_edit(Scope::Channel(channel), None);
    drop(app.reconcile_shared_views());
    assert!(app.workspace.cards().is_empty());
    assert!(app.shared_views.deferred_default.is_none());
}

#[test]
fn joining_the_first_registry_keeps_the_selected_scope_default_intent() {
    let (mut app, channel, _) = loading_channel();
    drop(app.ensure_default_pane());
    let identity = uuid::Uuid::new_v4();
    let model = crate::workspace_model::WorkspaceModel {
        identity,
        channels: app
            .durable
            .channels
            .iter()
            .map(|channel| crate::workspace_model::ModelChannel {
                id: channel.id,
                repository: "/repo/.git".into(),
                title: channel.title.clone(),
                title_source: channel.title_source,
                ordinal: channel.ordinal,
            })
            .collect(),
        views: vec![View::empty(channel)],
        ..Default::default()
    };
    drop(app.apply_workspace_model_update(
        super::super::super::workspace_model_sync::ModelUpdate::Connected(model),
    ));
    assert_eq!(
        app.shared_views.deferred_default,
        Some((Scope::Channel(channel), "/repo".into()))
    );
    assert!(app.workspace.cards().is_empty());
    app.acknowledge_view_edit(Scope::Channel(channel), None);
    drop(app.reconcile_shared_views());
    assert!(app.chat_is_unused_default(app.workspace.focused().unwrap()));
}
