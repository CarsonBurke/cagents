//! A session that lost its pane comes back when a pane holding it reappears,
//! whichever card identity that pane uses, and stays back across a reload.
use super::published_card_edits::{accept_local_view, shared_channel_app, stored};
use super::*;
use crate::agent::Provider;

/// A window showing one started chat whose pane a shared projection then removed:
/// the card is saved closed although the daemon still hosts its process.
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
