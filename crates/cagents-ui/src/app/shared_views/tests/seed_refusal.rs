use super::*;
use crate::app::workspace_model_sync::ModelUpdate;

fn seeding_window() -> (Cagents, Scope, Pane, TestModelRequests) {
    let (mut app, channel, _, pane) = channel_move_fixture();
    let scope = Scope::Channel(channel);
    app.shared_views = State::for_workspace(&app.durable);
    app.workspace_model.views.clear();
    Arc::make_mut(&mut app.durable).workspace_model_identity = Some(app.workspace_model.identity);
    let (tx, rx) = test_model_channel();
    app.workspace_model_tx = tx;
    drop(app.reconcile_shared_views());
    assert_eq!(seeds_for(&rx, &scope), 1);
    assert!(app.shared_views.seeding.contains(&scope));
    (app, scope, pane, rx)
}

fn seeds_for(rx: &TestModelRequests, wanted: &Scope) -> usize {
    std::iter::from_fn(|| rx.try_recv().ok())
        .filter(|edit| matches!(edit, ModelEdit::SeedView { scope, .. } if scope == wanted))
        .count()
}

fn refuse(app: &mut Cagents, scope: &Scope, reason: &str) {
    drop(app.apply_workspace_model_update(ModelUpdate::SeedRefused {
        scope: scope.clone(),
        attempt: app.shared_views.seed_attempts[scope],
        error: reason.into(),
    }));
}

fn succeed(app: &mut Cagents, scope: &Scope, pane: Pane) {
    let mut model = app.workspace_model.clone();
    model.sequence += 1;
    let mut view = View::empty(scope.clone());
    view.revision = 1;
    view.panes.push(pane);
    model.views.push(view);
    drop(app.apply_workspace_model_update(ModelUpdate::SeedApplied {
        scope: scope.clone(),
        attempt: app.shared_views.seed_attempts[scope],
        model,
    }));
}

#[test]
fn refused_seed_clears_seeding_and_stays_held_across_reconciliation() {
    let (mut app, scope, _, rx) = seeding_window();
    refuse(&mut app, &scope, "The registry could not store these panes");
    assert!(!app.shared_views.seeding.contains(&scope));
    assert!(!app.shared_views.initialized.contains(&scope));
    assert_eq!(
        app.shared_views.seed_failures[&scope],
        "The registry could not store these panes"
    );
    assert!(app.refused_view_seeds_notice().is_some());
    assert_eq!(app.notice_control_severity(), Some(NoticeSeverity::Warning));
    for _ in 0..3 {
        drop(app.reconcile_shared_views());
        app.publish_view_changes();
    }
    assert_eq!(seeds_for(&rx, &scope), 0);
    assert!(app.can_retry_view_seeds());
}

#[test]
fn refused_seed_retry_succeeds_and_clears_the_persistent_notice() {
    let (mut app, scope, pane, rx) = seeding_window();
    refuse(&mut app, &scope, "Please offer these panes again");
    drop(app.update(Message::RetrySharedViewSeeds));
    assert_eq!(seeds_for(&rx, &scope), 1);
    assert!(!app.can_retry_view_seeds());
    assert!(app.refused_view_seeds_notice().is_some());
    // A repeated click or unrelated wake cannot issue a second in-flight seed.
    drop(app.update(Message::RetrySharedViewSeeds));
    drop(app.reconcile_shared_views());
    assert_eq!(seeds_for(&rx, &scope), 0);
    succeed(&mut app, &scope, pane);
    assert!(!app.shared_views.seeding.contains(&scope));
    assert!(app.shared_views.initialized.contains(&scope));
    assert!(app.refused_view_seeds_notice().is_none());
    assert!(!app.has_refused_view_seeds());
}

#[test]
fn repeated_seed_refusal_keeps_the_new_reason_without_a_retry_loop() {
    let (mut app, scope, _, rx) = seeding_window();
    refuse(&mut app, &scope, "First refusal");
    app.retry_view_seeds();
    assert_eq!(seeds_for(&rx, &scope), 1);
    refuse(&mut app, &scope, "Storage is still unavailable");
    for _ in 0..3 {
        drop(app.reconcile_shared_views());
        app.publish_view_changes();
    }
    assert_eq!(seeds_for(&rx, &scope), 0);
    assert_eq!(
        app.shared_views.seed_failures[&scope],
        "Storage is still unavailable"
    );
    assert!(app.can_retry_view_seeds());
}

#[test]
fn only_a_new_registry_retries_a_refused_seed_once() {
    let (mut app, scope, pane, rx) = seeding_window();
    let old_attempt = app.shared_views.seed_attempts[&scope];
    refuse(&mut app, &scope, "Old registry refusal");
    drop(app.apply_workspace_model_update(ModelUpdate::Connected(app.workspace_model.clone())));
    assert_eq!(seeds_for(&rx, &scope), 0);
    let mut replacement = app.workspace_model.clone();
    replacement.identity = uuid::Uuid::new_v4();
    drop(app.apply_workspace_model_update(ModelUpdate::Connected(replacement.clone())));
    assert_eq!(seeds_for(&rx, &scope), 1);
    assert!(app.refused_view_seeds_notice().is_some());
    assert!(!app.can_retry_view_seeds());
    // A late definitive answer from the old publisher cannot block the new offer.
    drop(app.apply_workspace_model_update(ModelUpdate::SeedRefused {
        scope: scope.clone(),
        attempt: old_attempt,
        error: "Late old refusal".into(),
    }));
    assert!(app.shared_views.seeding.contains(&scope));
    drop(app.apply_workspace_model_update(ModelUpdate::Connected(replacement)));
    assert_eq!(seeds_for(&rx, &scope), 0);
    succeed(&mut app, &scope, pane);
    assert!(app.refused_view_seeds_notice().is_none());
}

#[test]
fn retrying_a_held_scope_does_not_claim_that_a_seed_is_in_flight() {
    let (mut app, scope, _, rx) = seeding_window();
    refuse(&mut app, &scope, "Refused");
    app.shared_views.blocked.insert(scope.clone());
    app.retry_view_seeds();
    assert_eq!(seeds_for(&rx, &scope), 0);
    assert!(app.can_retry_view_seeds());
    assert!(!app.shared_views.seeding.contains(&scope));
}

#[test]
fn a_seed_failure_keeps_the_warning_anchor_even_beside_a_success() {
    let (mut app, scope, _, _) = seeding_window();
    refuse(&mut app, &scope, "The registry refused these panes");
    app.notices
        .record(TransientNotice::success("Session saved"));
    assert_eq!(app.notice_control_severity(), Some(NoticeSeverity::Warning));
}

#[test]
fn retiring_a_refused_channel_removes_its_notice_and_ignores_late_refusals() {
    let (mut app, scope, _, rx) = seeding_window();
    refuse(&mut app, &scope, "The registry refused these panes");
    let attempt = uuid::Uuid::new_v4();
    let Scope::Channel(id) = scope else {
        panic!("channel fixture")
    };
    drop(app.finish_delete_channel(id));
    assert!(!app.has_refused_view_seeds());
    drop(app.apply_workspace_model_update(ModelUpdate::SeedRefused {
        scope: Scope::Channel(id),
        attempt,
        error: "Late answer after deletion".into(),
    }));
    assert!(!app.has_refused_view_seeds());
    assert_eq!(seeds_for(&rx, &Scope::Channel(id)), 0);
}

#[test]
fn a_seed_failed_destination_cannot_receive_an_explicit_transfer() {
    let (mut app, source, destination, pane) = channel_move_fixture();
    let scope = Scope::Channel(destination);
    app.shared_views.initialized.remove(&scope);
    let (tx, rx) = test_model_channel();
    app.workspace_model_tx = tx;
    app.publish_view_changes();
    refuse(&mut app, &scope, "Destination panes were refused");
    assert!(!app.can_move_pane_to_channel(pane.id, destination));
    assert!(
        !app.pane_move_destinations(pane.id)
            .iter()
            .any(|channel| channel.id == destination)
    );
    drop(app.move_pane_to_channel(pane.id, destination));
    assert!(
        !std::iter::from_fn(|| rx.try_recv().ok()).any(|edit| matches!(
            edit,
            ModelEdit::ChangeView {
                change: Change::Transfer { .. },
                ..
            }
        ))
    );
    assert!(
        app.durable
            .channels
            .iter()
            .find(|channel| channel.id == source)
            .unwrap()
            .cards
            .contains(&pane.id)
    );
}

#[test]
fn one_retry_offers_every_refused_scope_once_and_keeps_unsettled_errors() {
    let (mut app, scope, pane, rx) = seeding_window();
    let other = app
        .shared_views
        .seeding
        .iter()
        .find(|other| **other != scope)
        .unwrap()
        .clone();
    refuse(&mut app, &scope, "First channel refused");
    refuse(&mut app, &other, "Second channel refused");
    assert_eq!(app.shared_views.seed_failures.len(), 2);
    app.retry_view_seeds();
    let seeds = std::iter::from_fn(|| rx.try_recv().ok())
        .filter_map(|edit| match edit {
            ModelEdit::SeedView { scope, .. } => Some(scope),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(seeds.iter().filter(|seed| **seed == scope).count(), 1);
    assert_eq!(seeds.iter().filter(|seed| **seed == other).count(), 1);
    succeed(&mut app, &scope, pane);
    assert_eq!(app.shared_views.seed_failures.len(), 1);
    assert!(!app.can_retry_view_seeds());
    refuse(&mut app, &other, "Second channel still refused");
    assert!(app.can_retry_view_seeds());
    assert_eq!(
        app.shared_views.seed_failures[&other],
        "Second channel still refused"
    );
    assert_eq!(seeds_for(&rx, &other), 0);
}

#[test]
fn late_seed_receipts_cannot_settle_a_new_worktree_cleanup_seed() {
    let (mut app, _, _, _) = channel_move_fixture();
    let repository = PathBuf::from("/repo/.git");
    let scope = Scope::Worktree {
        repository: repository.clone(),
        path: "/repo/removed-worktree".into(),
    };
    app.sidebar_model.projects.push(SidebarProject::new(
        ProjectId::new(),
        RepositoryId::new(),
        "repo".into(),
        repository,
        vec![],
        vec![],
    ));
    app.workspace_model.views.push(View::empty(scope.clone()));
    let (tx, rx) = test_model_channel();
    app.workspace_model_tx = tx;
    app.publish_view_changes();
    let old_attempt = app.shared_views.seed_attempts[&scope];
    assert_eq!(seeds_for(&rx, &scope), 1);
    // Successful removal invalidates the old offer; a historical empty view
    // can still require a cleanup seed on the following persistence pass.
    app.forget_view_seed(&scope);
    app.publish_view_changes();
    let current = app.shared_views.seed_attempts[&scope];
    assert_ne!(old_attempt, current);
    assert_eq!(seeds_for(&rx, &scope), 1);
    drop(app.apply_workspace_model_update(ModelUpdate::SeedRefused {
        scope: scope.clone(),
        attempt: old_attempt,
        error: "Late pre-deletion refusal".into(),
    }));
    drop(app.apply_workspace_model_update(ModelUpdate::SeedApplied {
        scope: scope.clone(),
        attempt: old_attempt,
        model: app.workspace_model.clone(),
    }));
    assert_eq!(app.shared_views.seed_attempts[&scope], current);
    assert!(app.shared_views.seeding.contains(&scope));
    assert!(!app.has_refused_view_seeds());
}

#[test]
#[cfg(not(feature = "integrated-browser"))]
fn refused_scope_cannot_use_the_settled_presentation_projection() {
    let (mut app, source, _, _) = presentation_fixture();
    let scope = Scope::Channel(source);
    let before = app
        .workspace_model
        .views
        .iter()
        .find(|view| view.scope == scope)
        .unwrap()
        .clone();
    let mut after = before.clone();
    after.revision += 1;
    assert_eq!(
        app.presentation_projection_ready(&before, &after, None),
        Ok(())
    );
    app.shared_views
        .seed_failures
        .insert(scope, "The registry refused these panes".into());
    assert_eq!(
        app.presentation_projection_ready(&before, &after, None),
        Err("scope admission is unsettled")
    );
}
