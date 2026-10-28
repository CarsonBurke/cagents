//! Hold real App publications while a peer moves their pane. The typed model
//! receipts below exercise UI reconciliation, not the publisher or host wire.
use super::*;
use crate::app::{chrome_ui::Action, workspace_model_sync::ModelUpdate};
use crate::workspace_model::WorkspaceModel;

type PublishedUpdate = (uuid::Uuid, Scope, u64, Change);

fn published_updates(rx: &async_channel::Receiver<ModelEdit>) -> Vec<PublishedUpdate> {
    std::iter::from_fn(|| rx.try_recv().ok())
        .filter_map(|edit| match edit {
            ModelEdit::ChangeView {
                operation,
                scope,
                expected_revision,
                change,
            } => Some((operation, scope, expected_revision, change)),
            _ => None,
        })
        .collect()
}

fn rename_and_resize(app: &mut Cagents, source: ChannelId, card: CardId) -> Pane {
    drop(app.update(Message::FocusCard(card)));
    assert_eq!(app.workspace.focused(), Some(card));
    let original_width = app.card_state(card).unwrap().width_fraction;
    drop(app.update(Message::ChromeUi(Action::Rename(card))));
    drop(app.update(Message::ChromeUi(Action::RenameChanged(
        "Named before the peer move".into(),
    ))));
    drop(app.update(Message::ChromeUi(Action::RenameSubmit)));
    app.workspace.resize_viewport(1000.0);
    // Browser panes use the public width preset control; edge dragging only
    // admits terminals and utility panes.
    drop(app.update(Message::CyclePaneWidth));
    let expected = app
        .local_view_panes(&Scope::Channel(source))
        .into_iter()
        .find(|pane| pane.id == card)
        .unwrap();
    assert_eq!(
        expected.user_title.as_deref(),
        Some("Named before the peer move")
    );
    assert_ne!(expected.width_fraction, original_width);
    expected
}

fn peer_move(model: &mut WorkspaceModel, card: CardId, from: ChannelId, to: ChannelId) {
    let source = Scope::Channel(from);
    let destination = Scope::Channel(to);
    views::transfer(&mut model.views, &source, card, &destination, None).unwrap();
    for view in &mut model.views {
        if view.scope == source || view.scope == destination {
            view.revision += 1;
        }
    }
    model.sequence += 1;
}

fn assert_pending(app: &Cagents, updates: &[PublishedUpdate]) {
    assert_eq!(
        app.shared_views.pending,
        updates
            .iter()
            .map(|(operation, scope, _, change)| (*operation, scope.clone(), change.clone()))
            .collect::<Vec<_>>()
    );
}

#[test]
fn peer_moves_retarget_public_name_and_width_edits_until_their_own_receipts() {
    let (mut app, source, destination, pane) = channel_move_fixture();
    let (tx, rx) = async_channel::unbounded();
    app.workspace_model_tx = tx;
    let mut model = app.workspace_model.clone();
    let expected = rename_and_resize(&mut app, source, pane.id);
    let original = published_updates(&rx);
    assert_eq!(
        original.len(),
        2,
        "rename and width are separate publications"
    );
    assert!(original.iter().any(|(_, scope, _, change)| {
        *scope == Scope::Channel(source)
            && matches!(change, Change::Update { before, pane: after }
                if before.title != after.title && before.user_title != after.user_title)
    }));
    assert!(original.iter().any(|(_, scope, _, change)| {
        *scope == Scope::Channel(source)
            && matches!(change, Change::Update { before, pane: after }
                if before.width_fraction != after.width_fraction)
    }));
    assert_pending(&app, &original);
    assert!(app.shared_views.requested_transfers.is_empty());
    let basis = (model.identity, PathBuf::from("/repo/.git"));
    for (operation, _, _, _) in &original {
        assert_eq!(app.shared_views.source_bases.get(operation), Some(&basis));
    }

    peer_move(&mut model, pane.id, source, destination);
    drop(
        app.update(Message::WorkspaceModelObserved(ModelUpdate::Read(
            model.clone(),
        ))),
    );
    assert_moving_metadata(&app, &expected);
    let first_retry = published_updates(&rx);
    assert_eq!(first_retry.len(), original.len());
    for ((old, _, _, old_change), (fresh, scope, revision, change)) in
        original.iter().zip(&first_retry)
    {
        assert_ne!(fresh, old);
        assert_eq!(*scope, Scope::Channel(destination));
        assert_eq!(*revision, 1);
        assert_eq!(change, old_change);
        assert_eq!(app.shared_views.source_bases.get(fresh), Some(&basis));
    }
    assert_pending(&app, &first_retry);
    assert!(app.shared_views.requested_transfers.is_empty());

    // The old source accepts these presentation edits as no-ops. Its typed
    // acknowledgements cannot retire the fresh destination publications.
    for (operation, scope, _, change) in &original {
        assert!(
            !model
                .views
                .iter_mut()
                .find(|view| view.scope == *scope)
                .unwrap()
                .apply(change)
                .unwrap()
        );
        drop(
            app.update(Message::WorkspaceModelObserved(ModelUpdate::ViewApplied {
                scope: scope.clone(),
                operation: Some(*operation),
                model: model.clone(),
            })),
        );
        assert_pending(&app, &first_retry);
        assert_moving_metadata(&app, &expected);
    }
    assert!(published_updates(&rx).is_empty());

    // Another peer move can overtake the first retries, with no local move
    // request keeping the card in requested_transfers.
    peer_move(&mut model, pane.id, destination, source);
    drop(
        app.update(Message::WorkspaceModelObserved(ModelUpdate::Read(
            model.clone(),
        ))),
    );
    assert_moving_metadata(&app, &expected);
    let second_retry = published_updates(&rx);
    assert_eq!(second_retry.len(), original.len());
    for ((_, _, _, old_change), (fresh, scope, revision, change)) in
        first_retry.iter().zip(&second_retry)
    {
        assert!(
            original
                .iter()
                .chain(&first_retry)
                .all(|(old, _, _, _)| old != fresh)
        );
        assert_eq!(*scope, Scope::Channel(source));
        assert_eq!(*revision, 19);
        assert_eq!(change, old_change);
        assert_eq!(app.shared_views.source_bases.get(fresh), Some(&basis));
    }
    for (operation, scope, _, change) in &first_retry {
        assert!(
            !model
                .views
                .iter_mut()
                .find(|view| view.scope == *scope)
                .unwrap()
                .apply(change)
                .unwrap()
        );
        drop(
            app.update(Message::WorkspaceModelObserved(ModelUpdate::ViewApplied {
                scope: scope.clone(),
                operation: Some(*operation),
                model: model.clone(),
            })),
        );
        assert_pending(&app, &second_retry);
        assert_moving_metadata(&app, &expected);
    }
    drop(
        app.update(Message::WorkspaceModelObserved(ModelUpdate::Read(
            model.clone(),
        ))),
    );
    app.persist();
    assert_pending(&app, &second_retry);
    assert!(
        published_updates(&rx).is_empty(),
        "equal reads and persistence must not mint more retries"
    );

    for (operation, scope, _, change) in second_retry {
        let view = model
            .views
            .iter_mut()
            .find(|view| view.scope == scope)
            .unwrap();
        assert!(view.apply(&change).unwrap());
        view.revision += 1;
        model.sequence += 1;
        drop(
            app.update(Message::WorkspaceModelObserved(ModelUpdate::ViewApplied {
                scope,
                operation: Some(operation),
                model: model.clone(),
            })),
        );
        assert_moving_metadata(&app, &expected);
    }
    assert!(app.shared_views.pending.is_empty());
    assert!(app.shared_views.source_bases.is_empty());
    assert!(published_updates(&rx).is_empty());
}

#[test]
fn peer_move_cannot_rebind_real_presentation_edits_to_another_repository() {
    let (mut app, source, destination, pane) = channel_move_fixture();
    let (tx, rx) = async_channel::unbounded();
    app.workspace_model_tx = tx;
    let mut model = app.workspace_model.clone();
    rename_and_resize(&mut app, source, pane.id);
    let original = published_updates(&rx);
    assert_eq!(original.len(), 2);
    assert_pending(&app, &original);
    assert!(app.shared_views.requested_transfers.is_empty());
    let basis = (model.identity, PathBuf::from("/repo/.git"));
    for (operation, _, _, _) in &original {
        assert_eq!(app.shared_views.source_bases.get(operation), Some(&basis));
    }
    peer_move(&mut model, pane.id, source, destination);
    // Deliberately change the model's repository evidence after the actual
    // publications captured their source. New ownership cannot renew that proof.
    for channel in &mut model.channels {
        channel.repository = "/different/.git".into();
    }
    drop(app.update(Message::WorkspaceModelObserved(ModelUpdate::Read(model))));
    assert!(published_updates(&rx).is_empty());
    assert!(app.shared_views.pending.is_empty());
    for (operation, _, _, _) in original {
        assert!(!app.shared_views.source_bases.contains_key(&operation));
    }
    assert_moving_metadata(&app, &pane);
    assert!(app.notices.recent().any(|entry| {
        entry
            .notice
            .message
            .contains("incoming session update could not be verified")
    }));
}
