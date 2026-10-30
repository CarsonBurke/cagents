use super::*;
use crate::app::workspace_model_sync::ModelUpdate;

fn ordered_fixture(split: bool) -> (Cagents, ChannelId, ChannelId, [Pane; 3]) {
    let (mut app, source, destination, mut a) = channel_move_fixture();
    app.active_workspace = Some(PathBuf::from("/repo"));
    a.title = "A".into();
    let mut b = a.clone();
    b.id = CardId::new();
    b.title = "B".into();
    let mut c = a.clone();
    c.id = CardId::new();
    c.title = "C".into();
    for pane in [&mut b, &mut c] {
        let CardKind::Browser(browser) = &mut pane.kind else {
            panic!("the existing fixture supplies inert browser descriptors");
        };
        browser.id = BrowserId::new();
    }
    for view in &mut app.workspace_model.views {
        view.panes = if view.scope == Scope::Channel(source) {
            if split {
                vec![a.clone()]
            } else {
                vec![b.clone(), c.clone(), a.clone()]
            }
        } else if split {
            vec![b.clone(), c.clone()]
        } else {
            Vec::new()
        };
    }
    Arc::make_mut(&mut app.durable).active_channel = Some(if split { destination } else { source });
    drop(app.reconcile_shared_views());
    assert!(app.shared_views.pending.is_empty());
    (app, source, destination, [a, b, c])
}

fn reorder(app: &mut Cagents, card: CardId, before: CardId) {
    let generation = app.workspace_view_generation;
    drop(app.update(Message::Viewport {
        generation,
        event: WorkspaceViewportEvent::Reordered {
            card,
            before: Some(before),
        },
    }));
}

fn channel_order(app: &Cagents, channel: ChannelId) -> Vec<CardId> {
    app.durable
        .channels
        .iter()
        .find(|view| view.id == channel)
        .unwrap()
        .cards
        .clone()
}

fn publications(rx: &async_channel::Receiver<ModelEdit>) -> Vec<(uuid::Uuid, Scope, Change)> {
    std::iter::from_fn(|| rx.try_recv().ok())
        .map(|edit| match edit {
            ModelEdit::ChangeView {
                operation,
                scope,
                change,
                ..
            } => (operation, scope, change),
            other => panic!("unexpected publication: {other:?}"),
        })
        .collect()
}

#[test]
fn two_public_reorders_replay_once_over_an_equal_model_read() {
    let (mut app, source, _, [a, b, c]) = ordered_fixture(false);
    let model = app.workspace_model.clone();
    let (tx, rx) = async_channel::unbounded();
    app.workspace_model_tx = tx;
    assert_eq!(
        app.workspace.order().collect::<Vec<_>>(),
        [b.id, c.id, a.id]
    );

    reorder(&mut app, a.id, c.id);
    reorder(&mut app, c.id, b.id);
    app.persist();
    let issued = publications(&rx);
    assert_eq!(issued.len(), 2);
    assert_ne!(issued[0].0, issued[1].0);
    assert_eq!(app.shared_views.pending, issued);
    assert_eq!(
        issued
            .iter()
            .map(|(_, scope, change)| (scope.clone(), change.clone()))
            .collect::<Vec<_>>(),
        [
            (
                Scope::Channel(source),
                Change::Move {
                    id: a.id,
                    before: Some(c.id)
                }
            ),
            (
                Scope::Channel(source),
                Change::Move {
                    id: c.id,
                    before: Some(b.id)
                }
            ),
        ]
    );
    assert_eq!(channel_order(&app, source), [c.id, b.id, a.id]);

    // No ACK is fabricated: the same canonical inventory must overlay the
    // original two public edits once, keeping their original receipt UUIDs.
    drop(app.update(Message::WorkspaceModelObserved(ModelUpdate::Read(model))));
    assert_eq!(
        channel_order(&app, source),
        [c.id, b.id, a.id],
        "pending moves must not replay twice"
    );
    assert_eq!(
        app.workspace.order().collect::<Vec<_>>(),
        [c.id, b.id, a.id]
    );
    assert_eq!(
        app.shared_views.baseline[&Scope::Channel(source)]
            .iter()
            .map(|p| p.id)
            .collect::<Vec<_>>(),
        [c.id, b.id, a.id]
    );
    assert_eq!(app.shared_views.pending, issued);
    app.persist();
    assert!(
        publications(&rx).is_empty(),
        "read/persist must not republish the original edits"
    );
}

#[test]
fn publisher_migration_keeps_an_earlier_destination_move_before_transfer() {
    let (mut app, source, destination, [a, b, c]) = ordered_fixture(true);
    let model = app.workspace_model.clone();
    let (tx, rx) = async_channel::unbounded();
    app.workspace_model_tx = tx;
    reorder(&mut app, c.id, b.id);

    // Source-bound local-projection migration fixture for the existing
    // publisher. This is not the move-to-channel menu, which waits for ACK
    // and uses before=None. Never insert synthetic pending operations.
    let durable = Arc::make_mut(&mut app.durable);
    for channel in &mut durable.channels {
        if channel.id == source {
            channel.cards.clear();
        } else if channel.id == destination {
            channel.cards.insert(0, a.id);
        }
    }
    app.publish_view_changes();
    let issued = publications(&rx);
    assert_eq!(issued.len(), 2);
    assert_ne!(issued[0].0, issued[1].0);
    assert_eq!(app.shared_views.pending, issued);
    assert_eq!(
        issued
            .iter()
            .map(|(_, scope, change)| (scope.clone(), change.clone()))
            .collect::<Vec<_>>(),
        [
            (
                Scope::Channel(destination),
                Change::Move {
                    id: c.id,
                    before: Some(b.id)
                }
            ),
            (
                Scope::Channel(source),
                Change::Transfer {
                    id: a.id,
                    destination: Scope::Channel(destination),
                    before: Some(c.id)
                }
            ),
        ]
    );

    drop(app.update(Message::WorkspaceModelObserved(ModelUpdate::Read(model))));
    assert_eq!(
        channel_order(&app, destination),
        [a.id, c.id, b.id],
        "pending transfers must not overtake earlier moves"
    );
    assert!(channel_order(&app, source).is_empty());
    assert_eq!(
        app.workspace.order().collect::<Vec<_>>(),
        [a.id, c.id, b.id]
    );
    assert_eq!(
        app.shared_views.baseline[&Scope::Channel(destination)]
            .iter()
            .map(|p| p.id)
            .collect::<Vec<_>>(),
        [a.id, c.id, b.id]
    );
    assert_eq!(app.shared_views.pending, issued);
    assert!(
        !app.pane_transfer_pending(a.id),
        "publisher migration is not an explicit menu transfer request"
    );
    app.persist();
    assert!(
        publications(&rx).is_empty(),
        "ordered projection must not publish another migration"
    );
}

#[test]
fn pending_overlay_reports_last_error_only_for_eligible_scopes() {
    for eligibility in ["eligible", "uninitialized", "seeding", "blocked"] {
        let (mut app, source, _, [mut a, _, _]) = ordered_fixture(false);
        let scope = Scope::Channel(source);
        match eligibility {
            "uninitialized" => {
                app.shared_views.initialized.remove(&scope);
            }
            "seeding" => {
                app.shared_views.seeding.insert(scope.clone());
            }
            "blocked" => {
                // A real local draft prevents alias adoption, establishing
                // the production blocked-scope guard without setting it by hand.
                let alias = a.id;
                a.id = CardId::new();
                app.workspace_model
                    .views
                    .iter_mut()
                    .find(|v| v.scope == scope)
                    .unwrap()
                    .panes = vec![a.clone()];
                app.chat_ui = chat_ui::State::with_drafts(HashMap::from([(
                    alias,
                    "keep local draft".into(),
                )]));
            }
            _ => {}
        }
        app.status = Some(TransientNotice::warning("retained status"));
        let mut wrong_id = a.clone();
        wrong_id.id = CardId::new();
        let mut wrong_scope = a.clone();
        wrong_scope.scope = Some(PathBuf::from("relative"));
        // Invalid source data is confined to this error-boundary test. The
        // ordering regressions above obtain every operation from real producers.
        app.shared_views.pending.extend([
            (
                uuid::Uuid::new_v4(),
                scope.clone(),
                Change::Update {
                    before: a.clone(),
                    pane: wrong_id,
                },
            ),
            (
                uuid::Uuid::new_v4(),
                scope.clone(),
                Change::Update {
                    before: a,
                    pane: wrong_scope,
                },
            ),
        ]);
        drop(app.reconcile_shared_views());
        let status = app.status.as_ref().unwrap();
        if eligibility == "eligible" {
            assert_eq!(status.message, "pane scope must be an absolute device path");
        } else {
            assert_ne!(status.message, "pane identity cannot change");
            assert_ne!(status.message, "pane scope must be an absolute device path");
            if eligibility == "blocked" {
                assert!(app.shared_views.blocked.contains(&scope));
                assert!(status.message.contains("local draft"));
            } else {
                assert_eq!(status.message, "retained status");
            }
        }
    }
}
