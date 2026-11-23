//! A pane another client moves out of the channel this window shows: the
//! window follows the pane it was showing, and otherwise says where it went.
use super::*;
use crate::app::pane_follow;
use crate::app::workspace_model_sync::{EdgeChanges, ModelUpdate};
use crate::workspace_model::delta::{EditEffects, ViewChanges, ViewRevision};

/// Two shared channels of one project, "Drafts" showing `moved` and `stays`
/// with `focus` focused, and an empty "Release notes".
fn two_panes(focus: usize) -> (Cagents, ChannelId, ChannelId, [Pane; 2]) {
    let (mut app, source, destination, moved) = fixture();
    let mut stays = moved.clone();
    stays.id = CardId::new();
    stays.title = "notes".into();
    if let CardKind::Browser(browser) = &mut stays.kind {
        browser.id = BrowserId::new();
    }
    let project = app.durable.channels[0].project_id;
    app.sidebar_model.projects.push(SidebarProject::new(
        project,
        RepositoryId::new(),
        "repo".into(),
        "/repo/.git".into(),
        vec![crate::sidebar::SidebarWorktree::from_durable(
            crate::model::WorktreeId::new(),
            "/repo".into(),
            None,
        )],
        vec![],
    ));
    for (id, title) in [(source, "Drafts"), (destination, "Release notes")] {
        Arc::make_mut(&mut app.durable)
            .channels
            .iter_mut()
            .find(|channel| channel.id == id)
            .unwrap()
            .title = title.into();
        app.workspace_model
            .channels
            .iter_mut()
            .find(|channel| channel.id == id)
            .unwrap()
            .title = title.into();
    }
    let mut source_view = View::empty(source);
    source_view.revision = 3;
    source_view.panes = vec![moved.clone(), stays.clone()];
    let mut destination_view = View::empty(destination);
    destination_view.revision = 5;
    app.workspace_model.views = vec![source_view, destination_view];
    drop(app.reconcile_shared_views());
    drop(app.navigate_to_channel(source));
    let panes = [moved, stays];
    drop(app.focus_workspace_card(panes[focus].id));
    assert_eq!(app.durable.active_channel, Some(source));
    assert_eq!(app.workspace.focused(), Some(panes[focus].id));
    (app, source, destination, panes)
}

/// The edge another client's transfer of `card` publishes.
fn transfer_edge(
    app: &Cagents,
    card: CardId,
    source: ChannelId,
    destination: ChannelId,
    operation: uuid::Uuid,
) -> ModelUpdate {
    transfer_over(
        app.workspace_model.identity,
        app.workspace_model.sequence + 1,
        app.workspace_model.views.clone(),
        card,
        source,
        destination,
        operation,
    )
    .0
}

/// The edge a transfer of `card` publishes at `sequence` over `views`, and
/// the views it leaves.
fn transfer_over(
    identity: uuid::Uuid,
    sequence: u64,
    mut views: Vec<View>,
    card: CardId,
    source: ChannelId,
    destination: ChannelId,
    operation: uuid::Uuid,
) -> (ModelUpdate, Vec<View>) {
    let (source, destination) = (Scope::Channel(source), Scope::Channel(destination));
    let before = views
        .iter()
        .map(|view| (view.scope.clone(), view.revision))
        .collect::<Vec<_>>();
    let expected = before
        .iter()
        .find(|(scope, _)| *scope == source)
        .map(|(_, revision)| *revision)
        .unwrap();
    views::transfer(&mut views, &source, card, &destination, None).unwrap();
    for view in &mut views {
        view.revision += 1;
    }
    let edge = ModelUpdate::Changed {
        sequence,
        edit: ModelEdit::ChangeView {
            scope: source.clone(),
            expected_revision: expected,
            operation,
            change: Change::Transfer {
                id: card,
                destination,
                before: None,
            },
        },
        changes: EdgeChanges {
            identity: Some(identity),
            effects: EditEffects {
                views: before
                    .into_iter()
                    .map(|(scope, revision)| ViewRevision {
                        scope,
                        before: Some(revision),
                        after: Some(revision + 1),
                    })
                    .collect(),
                ..Default::default()
            },
            views: ViewChanges {
                sequence,
                views: views.clone(),
                ..Default::default()
            },
        },
    };
    (edge, views)
}

/// Delivers `updates` as one run, as the observer stream does when they
/// queued together while the window was busy; otherwise one at a time.
fn deliver(app: &mut Cagents, updates: Vec<ModelUpdate>, run: bool) {
    if !run {
        for update in updates {
            drop(app.update(Message::WorkspaceModelObserved(update)));
        }
        return;
    }
    let (sender, receiver) = async_channel::unbounded();
    app.workspace_model_rx = receiver;
    let mut updates = updates.into_iter();
    let first = updates.next().expect("a run has a first update");
    for update in updates {
        sender.send_blocking(update).unwrap();
    }
    drop(app.update(Message::WorkspaceModelArrived(
        crate::app::ingress::Arrival::detached(first),
    )));
    assert!(
        app.workspace_model_rx.is_empty(),
        "the update drains its run"
    );
}

fn holds(app: &Cagents, channel: ChannelId, card: CardId) -> bool {
    channels_holding(&app.durable.channels, card).any(|held| held.id == channel)
}

fn notice(app: &Cagents) -> Option<&str> {
    app.pane_follow_notice().map(pane_follow::Notice::text)
}

#[test]
fn a_window_follows_the_pane_it_was_showing_and_can_go_back() {
    let (mut app, source, destination, [moved, stays]) = two_panes(0);
    let edge = transfer_edge(&app, moved.id, source, destination, uuid::Uuid::new_v4());
    drop(app.update(Message::WorkspaceModelObserved(edge)));

    assert!(holds(&app, destination, moved.id));
    assert_eq!(app.durable.active_channel, Some(destination));
    assert_eq!(app.workspace.focused(), Some(moved.id));
    assert_eq!(notice(&app), Some("Moved to Release notes"));

    drop(app.update(Message::MovedPane(pane_follow::Action::GoBack)));
    assert_eq!(app.durable.active_channel, Some(source));
    assert!(holds(&app, source, stays.id));
    let focused = app.workspace.focused().unwrap();
    assert!(holds(&app, source, focused), "focus is on what stayed");
    assert_eq!(notice(&app), None);
}

#[test]
fn a_window_reading_another_pane_stays_and_offers_the_moved_one() {
    let (mut app, source, destination, [moved, stays]) = two_panes(1);
    let edge = transfer_edge(&app, moved.id, source, destination, uuid::Uuid::new_v4());
    drop(app.update(Message::WorkspaceModelObserved(edge)));

    assert!(holds(&app, destination, moved.id));
    assert_eq!(app.durable.active_channel, Some(source));
    assert_eq!(app.workspace.focused(), Some(stays.id));
    // The pane by its own name, not by the channel it now leads.
    assert_eq!(notice(&app), Some("Browser moved to Release notes"));

    drop(app.update(Message::MovedPane(pane_follow::Action::Open)));
    assert_eq!(app.durable.active_channel, Some(destination));
    assert_eq!(app.workspace.focused(), Some(moved.id));
    assert_eq!(notice(&app), None);
}

#[test]
fn the_notice_belongs_to_the_channel_it_describes() {
    let (mut app, source, destination, [moved, _]) = two_panes(1);
    let edge = transfer_edge(&app, moved.id, source, destination, uuid::Uuid::new_v4());
    drop(app.update(Message::WorkspaceModelObserved(edge)));
    assert!(notice(&app).is_some());

    // Going to the pane some other way leaves nothing for the notice to do,
    // and coming back doesn't bring it back.
    let open = |channel_id| Message::Sidebar(SidebarAction::OpenChannel { channel_id });
    drop(app.update(open(destination)));
    assert_eq!(app.durable.active_channel, Some(destination));
    assert_eq!(notice(&app), None);
    drop(app.update(open(source)));
    assert_eq!(app.durable.active_channel, Some(source));
    assert_eq!(notice(&app), None);
}

#[test]
fn the_notice_goes_once_the_pane_it_offers_moves_on() {
    let (mut app, source, destination, [moved, _]) = two_panes(1);
    let edge = transfer_edge(&app, moved.id, source, destination, uuid::Uuid::new_v4());
    drop(app.update(Message::WorkspaceModelObserved(edge)));
    assert!(notice(&app).is_some());

    // Moved back here, the pane is in sight and Open has nowhere to go.
    let edge = transfer_edge(&app, moved.id, destination, source, uuid::Uuid::new_v4());
    drop(app.update(Message::WorkspaceModelObserved(edge)));
    assert!(holds(&app, source, moved.id));
    assert_eq!(app.durable.active_channel, Some(source));
    assert_eq!(notice(&app), None);
}

#[test]
fn coming_back_to_the_window_restarts_the_notice() {
    let (mut app, source, destination, [moved, _]) = two_panes(0);
    let main = cosmic::iced::window::Id::RESERVED;
    app.core.set_main_window_id(Some(main));
    drop(
        app.update(Message::RefreshUi(crate::app::refresh_ui::Action::Focused(
            main, false,
        ))),
    );
    let edge = transfer_edge(&app, moved.id, source, destination, uuid::Uuid::new_v4());
    drop(app.update(Message::WorkspaceModelObserved(edge)));
    let shown = app.pane_follow.as_ref().unwrap().deadline();

    std::thread::sleep(std::time::Duration::from_millis(5));
    drop(
        app.update(Message::RefreshUi(crate::app::refresh_ui::Action::Focused(
            main, true,
        ))),
    );
    assert!(app.pane_follow.as_ref().unwrap().deadline() > shown);
}

#[test]
fn a_window_does_not_chase_its_own_move() {
    let (mut app, source, destination, [moved, _]) = two_panes(0);
    let (tx, rx) = test_model_channel();
    app.workspace_model_tx = tx;
    drop(app.move_pane_to_channel(moved.id, destination));
    let ModelEdit::ChangeView { operation, .. } = rx.try_recv().unwrap() else {
        panic!("a transfer request");
    };
    let active = app.durable.active_channel;
    let edge = transfer_edge(&app, moved.id, source, destination, operation);
    drop(app.update(Message::WorkspaceModelObserved(edge)));

    assert!(holds(&app, destination, moved.id));
    assert_eq!(app.durable.active_channel, active);
    assert_eq!(notice(&app), None);
}

#[test]
fn a_window_on_another_channel_is_left_alone() {
    let (mut app, source, destination, [moved, _]) = two_panes(0);
    drop(app.navigate_to_channel(destination));
    let edge = transfer_edge(&app, moved.id, source, destination, uuid::Uuid::new_v4());
    drop(app.update(Message::WorkspaceModelObserved(edge)));

    assert!(holds(&app, destination, moved.id));
    assert_eq!(app.durable.active_channel, Some(destination));
    assert_eq!(notice(&app), None);
}

/// A peer brings a pane into the channel this window shows and then moves it
/// on, and both edges arrive together. The second edge's departure is read
/// against the window as the first left it, so the moved pane is offered
/// as it is when the edges arrive one at a time.
#[test]
fn a_run_offers_a_pane_an_earlier_edge_in_it_brought_here() {
    let outcome = |run: bool| {
        let (mut app, source, destination, [_, stays]) = two_panes(1);
        let mut arrived = stays.clone();
        arrived.id = CardId::new();
        arrived.title = "arrived".into();
        if let CardKind::Browser(browser) = &mut arrived.kind {
            browser.id = BrowserId::new();
        }
        let identity = app.workspace_model.identity;
        let sequence = app.workspace_model.sequence;
        let scope = Scope::Channel(source);
        let mut views = app.workspace_model.views.clone();
        let held = views.iter_mut().find(|view| view.scope == scope).unwrap();
        let before = held.revision;
        held.panes.push(arrived.clone());
        held.revision += 1;
        let inserted = held.clone();
        let insert = ModelUpdate::Changed {
            sequence: sequence + 1,
            edit: ModelEdit::ChangeView {
                scope: scope.clone(),
                expected_revision: before,
                operation: uuid::Uuid::new_v4(),
                change: Change::Insert {
                    pane: arrived.clone(),
                    before: None,
                },
            },
            changes: EdgeChanges {
                identity: Some(identity),
                effects: EditEffects {
                    views: vec![ViewRevision {
                        scope,
                        before: Some(before),
                        after: Some(before + 1),
                    }],
                    ..Default::default()
                },
                views: ViewChanges {
                    sequence: sequence + 1,
                    views: vec![inserted],
                    ..Default::default()
                },
            },
        };
        let (transfer, _) = transfer_over(
            identity,
            sequence + 2,
            views,
            arrived.id,
            source,
            destination,
            uuid::Uuid::new_v4(),
        );
        deliver(&mut app, vec![insert, transfer], run);
        (
            holds(&app, destination, arrived.id),
            app.durable.active_channel == Some(source),
            app.workspace.focused() == Some(stays.id),
            notice(&app).map(str::to_owned),
        )
    };
    let each = outcome(false);
    assert!(each.0, "the pane ends where the move put it");
    assert!(each.3.is_some(), "one at a time, the moved pane is offered");
    assert_eq!(outcome(true), each);
}

/// This window closed its last pane in a channel, and before the receipt
/// arrives a peer's pane came and went: the peer's move out and this
/// window's receipt arrive together. The channel is as empty as it is when
/// they arrive one at a time, so it retires the same way.
#[test]
fn a_run_retires_a_channel_its_edges_emptied() {
    let outcome = |run: bool| {
        let (mut app, source, destination, [moved, stays]) = two_panes(0);
        drop(app.navigate_to_channel(destination));
        // `stays` is this window's own pane, closed here: its removal is
        // published and closing it captured the channel for retirement.
        // `moved` is the peer's, adopted after that capture.
        app.card_state_mut(stays.id).unwrap().existence =
            crate::app::project_ui::removed_existence("closed");
        app.shared_views.closed.insert(stays.id);
        Arc::make_mut(&mut app.durable)
            .channels
            .iter_mut()
            .find(|channel| channel.id == source)
            .unwrap()
            .cards
            .retain(|&card| card != stays.id);
        let home = crate::workspace_model::ChannelIncarnation {
            registry: app.workspace_model.identity,
            channel: source,
            creation_ordinal: 1,
        };
        app.channel_ui
            .retire_candidates
            .insert(home, crate::app::channel_ui::Retirement::Closed);
        let (tx, rx) = test_model_channel();
        app.workspace_model_tx = tx;
        let identity = app.workspace_model.identity;
        let sequence = app.workspace_model.sequence;
        let (transfer, mut views) = transfer_over(
            identity,
            sequence + 1,
            app.workspace_model.views.clone(),
            moved.id,
            source,
            destination,
            uuid::Uuid::new_v4(),
        );
        let scope = Scope::Channel(source);
        let held = views.iter_mut().find(|view| view.scope == scope).unwrap();
        let before = held.revision;
        held.panes.clear();
        held.revision += 1;
        let emptied = held.clone();
        let receipt = ModelUpdate::Committed {
            receipt: crate::workspace_model::delta::EditReceipt {
                identity,
                sequence: sequence + 2,
                effects: EditEffects {
                    views: vec![ViewRevision {
                        scope: scope.clone(),
                        before: Some(before),
                        after: Some(before + 1),
                    }],
                    ..Default::default()
                },
            },
            edit: ModelEdit::ChangeView {
                scope,
                expected_revision: before,
                operation: uuid::Uuid::new_v4(),
                change: Change::Remove { id: stays.id },
            },
            views: ViewChanges {
                sequence: sequence + 2,
                views: vec![emptied],
                ..Default::default()
            },
        };
        deliver(&mut app, vec![transfer, receipt], run);
        std::iter::from_fn(|| rx.try_recv().ok())
            .filter(|edit| matches!(edit, ModelEdit::RetireEmptyChannel { .. }))
            .count()
    };
    assert_eq!(
        outcome(false),
        1,
        "one at a time, the emptied channel retires"
    );
    assert_eq!(outcome(true), 1);
}

/// A peer moves a pane out of a channel and then deletes the channel, and
/// both edges arrive together. Deleting retires the channel's own cards
/// only, so the moved pane stays open where it went, as it does when the
/// edges arrive one at a time.
#[test]
fn a_run_deleting_a_channel_keeps_the_pane_moved_out_of_it() {
    let outcome = |run: bool| {
        let (mut app, source, destination, [moved, _]) = two_panes(0);
        drop(app.navigate_to_channel(destination));
        let identity = app.workspace_model.identity;
        let sequence = app.workspace_model.sequence;
        let (transfer, views) = transfer_over(
            identity,
            sequence + 1,
            app.workspace_model.views.clone(),
            moved.id,
            source,
            destination,
            uuid::Uuid::new_v4(),
        );
        let scope = Scope::Channel(source);
        let revision = views
            .iter()
            .find(|view| view.scope == scope)
            .unwrap()
            .revision;
        let deleted = ModelUpdate::Changed {
            sequence: sequence + 2,
            edit: ModelEdit::DeleteChannel { id: source },
            changes: EdgeChanges {
                identity: Some(identity),
                effects: EditEffects {
                    removed_channels: vec![source],
                    views: vec![ViewRevision {
                        scope: scope.clone(),
                        before: Some(revision),
                        after: None,
                    }],
                    ..Default::default()
                },
                views: ViewChanges {
                    sequence: sequence + 2,
                    removed: vec![(scope, revision)],
                    ..Default::default()
                },
            },
        };
        deliver(&mut app, vec![transfer, deleted], run);
        (
            app.card_state(moved.id)
                .is_some_and(|state| state.existence.is_open()),
            holds(&app, destination, moved.id),
        )
    };
    assert_eq!(
        outcome(false),
        (true, true),
        "one at a time, the moved pane stays open where it went"
    );
    assert_eq!(outcome(true), (true, true));
}
