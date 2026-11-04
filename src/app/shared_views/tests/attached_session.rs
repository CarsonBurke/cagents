//! What one window does when a pane holding a session reaches it, given the
//! cards it already has for that session: closed, idle, or holding unsent text.
//! These start from that card state rather than replaying how it arose, and they
//! drop the tasks a reconcile returns, so neither a provider nor the daemon's
//! `cagents channel move` runs here.
use super::published_card_edits::{accept_local_view, shared_channel_app, stored};
use super::*;
use crate::agent::Provider;

/// A started chat whose card this window has saved closed, the state a session
/// is left in when a shared projection removes its pane while it keeps running.
fn detached_live_chat() -> (Cagents, Scope, View, Pane, tempfile::TempDir) {
    let (mut app, scope, directory) = shared_channel_app();
    let _ = app.new_native_chat(Provider::Claude, Some("/workspace/chat-fixture".into()));
    let card = app.workspace.focused().unwrap();
    let Some(CardKind::Chat(chat)) = app.card_state_mut(card).map(|state| &mut state.kind) else {
        panic!("the card holds a chat");
    };
    chat.launch.session_id = Some(uuid::Uuid::new_v4().to_string());
    app.persist();
    let mut view = accept_local_view(&mut app, &scope);
    let pane = view.panes.remove(0);
    assert_eq!(pane.id, card);
    view.revision += 1;
    view.retired.push(card);
    app.workspace_model.views = vec![view.clone()];
    drop(app.reconcile_shared_views());
    // Recorded nowhere once it happened: the reported card was not retired.
    view.retired.clear();
    app.workspace_model.views = vec![view.clone()];
    assert!(!stored(&app, card).existence.is_open());
    assert!(app.workspace.card(card).is_none());
    (app, scope, view, pane, directory)
}

fn publish(app: &mut Cagents, view: &mut View, pane: Pane) {
    view.revision += 1;
    view.panes.push(pane);
    app.workspace_model.views = vec![view.clone()];
    drop(app.reconcile_shared_views());
}

fn open_panes_of(app: &Cagents, kind: &CardKind) -> Vec<CardId> {
    app.durable
        .cards
        .iter()
        .filter(|card| card.existence.is_open() && views::same_resource(&card.kind, kind))
        .map(|card| card.id)
        .collect()
}

/// Storage never reopens a closed card, so a pane returning under that card's
/// own ID shows here without its changes being saved. Pinned because a live
/// session stays reachable; #2093 tracks saving it.
#[test]
fn a_pane_returning_under_its_closed_cards_id_shows_while_storage_keeps_it_closed() {
    let (mut app, _scope, mut view, pane, _directory) = detached_live_chat();
    publish(&mut app, &mut view, pane.clone());
    assert!(app.workspace.card(pane.id).is_some());
    assert_eq!(open_panes_of(&app, &pane.kind), [pane.id]);
    let writer = app.session.as_ref().unwrap();
    let refusal = writer
        .flush(std::time::Duration::from_secs(10))
        .unwrap_err();
    assert!(
        refusal.contains("saved state is invalid"),
        "storage refuses the reopened card: {refusal}"
    );
    let saved = writer
        .operation_store()
        .load_card(pane.id, None)
        .unwrap()
        .unwrap();
    assert!(!saved.existence.is_open());
}

/// `cagents channel move` cannot see a window's saved cards, so it mints a pane
/// for the session. The closed card must neither block that pane nor come back
/// beside it, then or after a reload.
#[test]
fn an_attached_session_shows_once_beside_its_closed_card_and_after_a_reload() {
    let (mut app, scope, mut view, closed, directory) = detached_live_chat();
    let attached = Pane {
        id: CardId::new(),
        ..closed.clone()
    };
    publish(&mut app, &mut view, attached.clone());
    assert!(app.workspace.card(attached.id).is_some());
    assert!(app.workspace.card(closed.id).is_none());
    assert_eq!(open_panes_of(&app, &attached.kind), [attached.id]);
    let saved = stored(&app, attached.id);
    assert!(saved.existence.is_open());
    assert_eq!(saved.kind, attached.kind);
    assert!(!stored(&app, closed.id).existence.is_open());

    // A new window process over the same saved state and the same daemon view.
    let saved_closed = stored(&app, closed.id);
    let channels = app.durable.channels.clone();
    let active = app.durable.active_channel;
    let identity = app.workspace_model.identity;
    drop(app);
    let mut reloaded = super::super::super::tests::context_inspector_test_app(false);
    let (writer, _) = crate::session::SessionRuntime::open(
        directory.path().join("state.sqlite3"),
        crate::model::BootId::new("boot-test"),
    )
    .unwrap();
    reloaded.session = Some(writer);
    let durable = Arc::make_mut(&mut reloaded.durable);
    durable.channels = channels;
    durable.active_channel = active;
    durable.cards.push(saved_closed);
    durable.cards.push(saved);
    reloaded.workspace_model.identity = identity;
    reloaded.workspace_model.views = vec![view];
    reloaded.shared_views.initialized.insert(scope);
    drop(reloaded.reconcile_shared_views());
    assert!(reloaded.workspace.card(attached.id).is_some());
    assert!(reloaded.workspace.card(closed.id).is_none());
    assert_eq!(open_panes_of(&reloaded, &attached.kind), [attached.id]);
}

/// A live chat this window shows from a card no shared view holds, as a first
/// send leaves it, plus an empty destination channel.
fn local_only_chat() -> (Cagents, View, Pane, tempfile::TempDir) {
    let (mut app, scope, directory) = shared_channel_app();
    let _ = app.new_native_chat(Provider::Claude, Some("/workspace/chat-fixture".into()));
    let card = app.workspace.focused().unwrap();
    let Some(CardKind::Chat(chat)) = app.card_state_mut(card).map(|state| &mut state.kind) else {
        panic!("the card holds a chat");
    };
    chat.launch.session_id = Some(uuid::Uuid::new_v4().to_string());
    let kind = app.card_state(card).unwrap().kind.clone();
    app.retain_local_pane(card);
    app.persist();
    let destination = ChannelId::new();
    Arc::make_mut(&mut app.durable)
        .channels
        .push(ProjectChannel {
            id: destination,
            project_id: ProjectId::new(),
            title: "destination".into(),
            title_source: Default::default(),
            ordinal: 2,
            shared: true,
            cards: vec![],
            focused: None,
        });
    let destination = Scope::Channel(destination);
    app.shared_views.initialized.insert(destination.clone());
    let Scope::Channel(id) = destination else {
        unreachable!()
    };
    let mut view = View::empty(id);
    view.revision = 1;
    let mut own = View::empty(match scope {
        Scope::Channel(id) => id,
        _ => unreachable!(),
    });
    own.revision = 1;
    app.workspace_model.views = vec![own, view.clone()];
    let attached = Pane {
        id: CardId::new(),
        title: "Attached chat".into(),
        user_title: None,
        pending_terminal_start: false,
        width_fraction: 0.5,
        kind,
        scope: None,
    };
    (app, view, attached, directory)
}

fn publish_attached(app: &mut Cagents, view: &mut View, attached: &Pane) {
    view.revision += 1;
    view.panes = vec![attached.clone()];
    let scope = view.scope.clone();
    let slot = app
        .workspace_model
        .views
        .iter_mut()
        .find(|candidate| candidate.scope == scope)
        .unwrap();
    *slot = view.clone();
    drop(app.reconcile_shared_views());
}

#[test]
fn attaching_a_session_with_an_idle_local_card_replaces_that_card() {
    let (mut app, mut view, attached, _directory) = local_only_chat();
    let card = app.workspace.focused().unwrap();
    publish_attached(&mut app, &mut view, &attached);
    assert!(app.shared_views.blocked.is_empty());
    assert_eq!(open_panes_of(&app, &attached.kind), [attached.id]);
    assert!(!app.card_state(card).unwrap().existence.is_open());
}

#[test]
fn unsent_text_holds_an_attached_session_until_it_clears_then_moves_it() {
    let (mut app, mut view, attached, _directory) = local_only_chat();
    let card = app.workspace.focused().unwrap();
    app.chat_ui = chat_ui::State::with_drafts(HashMap::from([(card, "unsent".into())]));
    publish_attached(&mut app, &mut view, &attached);
    assert!(app.shared_views.blocked.contains(&view.scope));
    assert_eq!(open_panes_of(&app, &attached.kind), [card]);
    assert!(app.shared_views.blocked_aliases.contains(&card));

    // Nothing has changed yet, so a reconcile is not due.
    drop(app.reconcile_cleared_blocked_aliases());
    assert_eq!(open_panes_of(&app, &attached.kind), [card]);

    app.chat_ui = chat_ui::State::with_drafts(HashMap::new());
    drop(app.reconcile_cleared_blocked_aliases());
    assert!(app.shared_views.blocked.is_empty());
    assert_eq!(open_panes_of(&app, &attached.kind), [attached.id]);
}

/// Closing the held card lets go of it and its draft, so the pane can show.
#[test]
fn closing_a_blocked_card_shows_the_attached_pane() {
    let (mut app, mut view, attached, _directory) = local_only_chat();
    let card = app.workspace.focused().unwrap();
    app.chat_ui = chat_ui::State::with_drafts(HashMap::from([(card, "unsent".into())]));
    publish_attached(&mut app, &mut view, &attached);
    assert!(app.another_pane_holds_session(card));

    drop(app.begin_chat_close(card));
    assert!(!app.card_state(card).unwrap().existence.is_open());
    assert_eq!(open_panes_of(&app, &attached.kind), [attached.id]);
    assert!(app.shared_views.blocked.is_empty());
    assert!(!app.shared_chat_has_local_work(card));
}

#[test]
fn a_terminal_attached_beside_its_closed_card_shows_once() {
    let (mut app, scope, _directory) = shared_channel_app();
    let terminal = crate::app::fresh_terminal_session(LaunchRecipe {
        program: "/bin/sh".into(),
        arguments: vec![],
        working_directory: "/workspace/chat-fixture".into(),
        environment: Default::default(),
        unset_environment: Default::default(),
    });
    let kind = CardKind::Terminal(terminal);
    let closed = CardState {
        id: CardId::new(),
        generation: app.durable.generation,
        title: "Terminal".into(),
        width_fraction: 0.5,
        existence: crate::app::project_ui::removed_existence("pane closed in another client"),
        exposure: Exposure::Offscreen,
        residency: Residency::Cold,
        kind: kind.clone(),
    };
    let closed_id = closed.id;
    Arc::make_mut(&mut app.durable).cards.push(closed);
    let Scope::Channel(channel) = scope else {
        unreachable!()
    };
    let mut view = View::empty(channel);
    let attached = Pane {
        id: CardId::new(),
        title: "Terminal".into(),
        user_title: None,
        pending_terminal_start: false,
        width_fraction: 0.5,
        kind: kind.clone(),
        scope: None,
    };
    publish(&mut app, &mut view, attached.clone());
    assert_eq!(open_panes_of(&app, &kind), [attached.id]);
    assert!(!app.card_state(closed_id).unwrap().existence.is_open());
}
