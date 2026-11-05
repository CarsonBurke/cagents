//! What one window does when a pane holding a session reaches it, given the
//! cards it already has for that session: closed, idle, or with work on it.
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
    // Its own publication is over: the daemon never took it, so it stays local.
    app.shared_views.pending.clear();
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

/// Closing a card a pane holds the session of lets go of it and its draft, and
/// the pane shows.
#[test]
fn closing_a_held_card_shows_the_attached_pane_without_its_draft() {
    let (mut app, mut view, attached, _directory) = local_only_chat();
    let card = app.workspace.focused().unwrap();
    app.chat_ui = chat_ui::State::with_drafts(HashMap::from([(card, "unsent".into())]));
    hold_in_view(&mut app, &mut view, &attached);
    assert!(app.another_pane_holds_session(card));

    drop(app.begin_chat_close(card));
    assert!(!app.card_state(card).unwrap().existence.is_open());
    assert_eq!(open_panes_of(&app, &attached.kind), [attached.id]);
    assert!(app.shared_views.blocked.is_empty());
    assert!(!app.shared_chat_has_local_work(card));
}

/// `pane` is in the daemon's view, whether or not this window has acted on it.
fn hold_in_view(app: &mut Cagents, view: &mut View, pane: &Pane) {
    view.revision += 1;
    view.panes = vec![pane.clone()];
    let scope = view.scope.clone();
    *app.workspace_model
        .views
        .iter_mut()
        .find(|candidate| candidate.scope == scope)
        .unwrap() = view.clone();
}

fn channel_holding(app: &Cagents, card: CardId) -> ChannelId {
    app.durable
        .channels
        .iter()
        .find(|channel| channel.cards.contains(&card))
        .unwrap()
        .id
}

/// A channel deletion waits on its sessions' close and removes the channel when
/// that answers; with nothing of its own to close it finishes at once.
#[test]
fn deleting_a_channel_only_closes_sessions_no_other_pane_holds() {
    for held_elsewhere in [true, false] {
        let (mut app, mut view, attached, _directory) = local_only_chat();
        let card = app.workspace.focused().unwrap();
        let old = channel_holding(&app, card);
        if held_elsewhere {
            hold_in_view(&mut app, &mut view, &attached);
        }
        // The confirmation has to say the same thing the deletion does.
        let channel = app
            .durable
            .channels
            .iter()
            .find(|channel| channel.id == old)
            .unwrap();
        assert_eq!(
            app.channel_conversation_count(channel),
            usize::from(!held_elsewhere)
        );
        let notes = app.channel_held_elsewhere_notes(channel);
        assert_eq!(notes.len(), usize::from(held_elsewhere), "{notes:?}");
        assert!(notes.iter().all(|note| note.contains("keeps running in")));
        drop(app.delete_channel(old));
        let remains = app.durable.channels.iter().any(|channel| channel.id == old);
        assert_eq!(
            remains, !held_elsewhere,
            "the close is awaited only if it was asked for"
        );
        if held_elsewhere {
            assert!(!app.card_state(card).unwrap().existence.is_open());
            assert_eq!(app.workspace_model.views[1].panes[0].id, attached.id);
        }
    }
}

#[test]
fn closing_a_terminal_card_only_ends_its_session_when_no_other_pane_holds_it() {
    for held_elsewhere in [true, false] {
        let mut app = crate::app::tests::context_inspector_test_app(true);
        let card = app.workspace.focused().unwrap();
        let state = app.card_state_mut(card).unwrap();
        state.residency = Residency::Hot;
        let CardKind::Terminal(session) = &mut state.kind else {
            unreachable!();
        };
        session.continuity = TerminalContinuity::Live {
            host_session_id: crate::model::HostSessionId::new("host"),
            boot_id: crate::model::BootId::new("boot"),
            host_incarnation: crate::model::HostIncarnation::new("incarnation"),
        };
        if held_elsewhere {
            let mut pane = Pane::from_card(app.card_state(card).unwrap(), None).unwrap();
            pane.id = CardId::new();
            let mut view = View::empty(ChannelId::new());
            view.panes.push(pane);
            app.workspace_model.views = vec![view];
        }
        drop(app.update(Message::CloseCardUnasked(card)));
        assert_eq!(app.terminal_close_pending(card), !held_elsewhere);
        assert_eq!(
            app.card_state(card).unwrap().existence.is_open(),
            !held_elsewhere
        );
    }
}

/// Letting go of a held card is an ordinary close of that card: it says so in
/// the saved record, and a channel it empties retires as usual.
#[test]
fn releasing_a_held_card_records_a_close_and_retires_the_channel_it_empties() {
    let (mut app, mut view, attached, _directory) = local_only_chat();
    let card = app.workspace.focused().unwrap();
    let old = channel_holding(&app, card);
    app.workspace_model
        .channels
        .push(crate::workspace_model::ModelChannel {
            id: old,
            repository: "/workspace/chat-fixture".into(),
            title: "view".into(),
            title_source: Default::default(),
            ordinal: 1,
        });
    hold_in_view(&mut app, &mut view, &attached);
    drop(app.begin_chat_close(card));
    let Existence::Closed { tombstone } = &app.card_state(card).unwrap().existence else {
        panic!("the card closed");
    };
    assert_eq!(tombstone.reason.as_deref(), Some("closed by user"));
    assert_eq!(app.channel_work.retire_candidates.len(), 1);
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
