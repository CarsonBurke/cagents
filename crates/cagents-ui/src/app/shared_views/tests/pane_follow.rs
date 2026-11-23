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
    let (source, destination) = (Scope::Channel(source), Scope::Channel(destination));
    let mut views = app.workspace_model.views.clone();
    let before = views
        .iter()
        .map(|view| (view.scope.clone(), view.revision))
        .collect::<Vec<_>>();
    views::transfer(&mut views, &source, card, &destination, None).unwrap();
    for view in &mut views {
        view.revision += 1;
    }
    let sequence = app.workspace_model.sequence + 1;
    ModelUpdate::Changed {
        sequence,
        edit: ModelEdit::ChangeView {
            scope: source.clone(),
            expected_revision: before[0].1,
            operation,
            change: Change::Transfer {
                id: card,
                destination,
                before: None,
            },
        },
        changes: EdgeChanges {
            identity: Some(app.workspace_model.identity),
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
                views,
                ..Default::default()
            },
        },
    }
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
    let title = app.card_display_title(app.card_state(moved.id).unwrap());
    assert_eq!(
        notice(&app),
        Some(format!("{title} moved to Release notes").as_str())
    );

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

    // Going to the pane some other way leaves nothing for the notice to do.
    drop(app.navigate_to_channel(destination));
    assert_eq!(notice(&app), None);
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
