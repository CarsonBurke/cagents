use super::*;
use crate::app::workspace_model_sync::ModelUpdate;

fn desktop() -> (Cagents, crate::workspace_model::WorkspaceModel, Pane) {
    let (mut app, _, _, template) = fixture();
    let repository = PathBuf::from("/repo/.git");
    let project = app.durable.channels[0].project_id;
    app.sidebar_model.projects.push(SidebarProject::new(
        project,
        RepositoryId::new(),
        "repo".into(),
        repository.clone(),
        vec![],
        vec![],
    ));
    let mut model = crate::workspace_model::WorkspaceModel {
        identity: app.workspace_model.identity,
        sequence: 9,
        ..Default::default()
    };
    let durable = Arc::make_mut(&mut app.durable);
    durable.channels.clear();
    durable.cards.clear();
    durable.card_scopes.clear();
    durable.active_channel = None;
    // The real generator records this registry identity in each window store.
    // A cold subscription is not a join to an unrelated/new registry.
    durable.workspace_model_identity = Some(model.identity);
    for ordinal in 0..76 {
        let id = ChannelId::new();
        durable.channels.push(ProjectChannel {
            id,
            project_id: project,
            title: format!("Channel {ordinal}"),
            title_source: Default::default(),
            ordinal,
            shared: true,
            cards: vec![],
            focused: None,
        });
        model.channels.push(crate::workspace_model::ModelChannel {
            id,
            repository: repository.clone(),
            title: format!("Channel {ordinal}"),
            title_source: Default::default(),
            ordinal,
        });
        model.views.push(View::empty(Scope::Channel(id)));
    }
    for index in 0..1769 {
        let mut pane = template.clone();
        pane.id = CardId::new();
        pane.title = format!("Saved pane {index}");
        if let CardKind::Browser(browser) = &mut pane.kind {
            browser.id = BrowserId::new();
        }
        let open = index < 83;
        durable.cards.push(CardState {
            id: pane.id,
            generation: 0,
            title: pane.title.clone(),
            width_fraction: pane.width_fraction,
            existence: if open {
                Existence::Open
            } else {
                super::super::super::project_ui::removed_existence("historical fixture")
            },
            kind: pane.kind.clone(),
        });
        if open {
            let channel = index % 76;
            durable.channels[channel].cards.push(pane.id);
            durable.channels[channel].focused.get_or_insert(pane.id);
            model.views[channel].panes.push(pane);
        }
    }
    for index in 0..1780 {
        model.views.push(View::empty(Scope::Worktree {
            repository: repository.clone(),
            path: format!("/repo/historical-{index}").into(),
        }));
    }
    app.shared_views.initialized.clear();
    app.shared_views.baseline.clear();
    app.shared_views.seeding.clear();
    app.shared_views.pending.clear();
    (app, model, template)
}

#[test]
fn cold_desktop_admits_equal_history_without_repeated_projection_rows() {
    for receipt_first in [false, true] {
        let (mut app, model, _) = desktop();
        assert_eq!(model.views.len(), 1856);
        assert_eq!(app.durable.cards.len(), 1769);
        assert!(app.shared_views.initialized.is_empty());
        let (tx, rx) = async_channel::unbounded();
        app.workspace_model_tx = tx;
        app.workspace_model = crate::workspace_model::WorkspaceModel::default();
        drop(app.apply_workspace_model_update(ModelUpdate::Connected(model.clone())));
        assert_eq!(app.shared_views.initialized.len(), 1856);
        assert!(
            !std::iter::from_fn(|| rx.try_recv().ok()).any(|edit| matches!(
                edit,
                ModelEdit::SeedView { .. } | ModelEdit::ChangeView { .. }
            ))
        );
        let saved = app.durable.clone();
        let scope = model.views[0].scope.clone();
        for receipt in if receipt_first {
            [true, false]
        } else {
            [false, true]
        } {
            app.shared_views.local_projection_entries.set(0);
            let update = if receipt {
                ModelUpdate::ViewApplied {
                    scope: scope.clone(),
                    operation: None,
                    model: model.clone(),
                }
            } else {
                ModelUpdate::Read(model.clone())
            };
            drop(app.apply_workspace_model_update(update));
            assert!(
                app.shared_views.local_projection_entries.get() <= 77,
                "only live channels and the exact seed receipt need rows"
            );
            assert_eq!(app.durable.cards, saved.cards);
            assert_eq!(app.durable.channels, saved.channels);
            assert_eq!(app.durable.card_scopes, saved.card_scopes);
            assert!(
                !std::iter::from_fn(|| rx.try_recv().ok()).any(|edit| matches!(
                    edit,
                    ModelEdit::SeedView { .. } | ModelEdit::ChangeView { .. }
                ))
            );
        }
        let pane = model.views[0].panes[0].id;
        app.card_state_mut(pane).unwrap().title = "unaccepted optimism".into();
        drop(app.apply_workspace_model_update(ModelUpdate::Read(model.clone())));
        assert_eq!(
            app.card_state(pane).unwrap().title,
            model.views[0].panes[0].title
        );
    }
}

#[test]
fn sparse_history_keeps_absent_active_baseline_and_pending_scope_obligations() {
    let (mut app, model, _) = desktop();
    drop(app.apply_workspace_model_update(ModelUpdate::Connected(model.clone())));
    let scope = model.views[76].scope.clone();
    let contains =
        |app: &Cagents, scope: &Scope| app.local_views().0.iter().any(|view| &view.scope == scope);
    assert!(!contains(&app, &scope));
    let Scope::Worktree { path, .. } = &scope else {
        unreachable!()
    };
    app.active_workspace = Some(path.clone());
    assert!(
        contains(&app, &scope),
        "the active empty strip still settles"
    );
    app.active_workspace = None;
    app.shared_views
        .baseline
        .get_mut(&scope)
        .unwrap()
        .push(model.views[0].panes[0].clone());
    assert!(
        contains(&app, &scope),
        "a previous owner still needs publication"
    );
    app.shared_views.baseline.get_mut(&scope).unwrap().clear();
    let source = model.views[0].scope.clone();
    app.shared_views.pending.push((
        uuid::Uuid::new_v4(),
        source,
        Change::Transfer {
            id: model.views[0].panes[0].id,
            destination: scope.clone(),
            before: None,
        },
    ));
    assert!(contains(&app, &scope), "a pending transfer keeps both ends");
    app.shared_views.pending.clear();
    app.shared_views.initialized.remove(&scope);
    app.shared_views.baseline.remove(&scope);
    assert!(
        contains(&app, &scope),
        "unadmitted empties retain migration semantics"
    );
    let absent = Scope::Worktree {
        repository: "/repo/.git".into(),
        path: "/repo".into(),
    };
    app.known_worktree_roots.insert("/repo".into());
    assert!(
        contains(&app, &absent),
        "a genuinely absent local scope must seed"
    );
    let (tx, rx) = async_channel::unbounded();
    app.workspace_model_tx = tx;
    app.publish_view_changes();
    assert_eq!(
        std::iter::from_fn(|| rx.try_recv().ok())
            .filter(|edit| matches!(edit, ModelEdit::SeedView { scope, .. } if scope == &absent))
            .count(),
        1
    );
    app.publish_view_changes();
    assert!(
        !std::iter::from_fn(|| rx.try_recv().ok())
            .any(|edit| matches!(edit, ModelEdit::SeedView { scope, .. } if scope == absent))
    );
}

#[test]
fn sparse_history_retains_canonical_retirement_and_ordered_custody_work() {
    let (mut app, model, mut template) = desktop();
    drop(app.apply_workspace_model_update(ModelUpdate::Connected(model.clone())));
    let scope = model.views[76].scope.clone();
    let contains = |app: &Cagents| app.local_views().0.iter().any(|view| view.scope == scope);
    assert!(!contains(&app));
    template.id = CardId::new();
    app.workspace_model.views[76].panes.push(template);
    assert!(
        contains(&app),
        "a canonical pane with no local member must be reconciled"
    );
    app.workspace_model.views[76].panes.clear();
    app.workspace_model.views[76]
        .retired
        .push(model.views[0].panes[0].id);
    assert!(
        contains(&app),
        "retirement of a live local identity keeps its scope"
    );
    app.workspace_model.views[76].retired.clear();
    app.shared_views.pending.push((
        uuid::Uuid::new_v4(),
        scope.clone(),
        Change::Remove { id: CardId::new() },
    ));
    assert!(contains(&app), "a pending source scope remains publishable");
    app.shared_views.pending.clear();
    app.shared_views.seeding.insert(scope.clone());
    assert!(contains(&app), "an in-flight seed still owns its baseline");
    app.shared_views.seeding.clear();
    let Scope::Worktree { path, .. } = &scope else {
        unreachable!()
    };
    app.shared_views.deferred_default = Some(path.clone());
    assert!(
        contains(&app),
        "a deferred default must settle its own scope"
    );
    app.shared_views.deferred_default = None;
    app.shared_views.creating_default = true;
    assert!(contains(&app), "default admission is not settled history");
    app.shared_views.creating_default = false;
    app.shared_views
        .adoptions
        .push(crate::persistence::TerminalPaneAdoption {
            alias: CardId::new(),
            canonical: CardId::new(),
            terminal: TerminalId::new(),
        });
    assert!(
        contains(&app),
        "ordered alias custody retains the full projection"
    );
    app.shared_views.adoptions.clear();
    app.shared_views
        .restorations
        .push(crate::persistence::TerminalPaneRestoration {
            canonical: CardId::new(),
            terminal: TerminalId::new(),
        });
    assert!(
        contains(&app),
        "ordered archived custody retains the full projection"
    );
    app.shared_views.restorations.clear();
    let CardKind::Terminal(terminal) = moving_terminal_kind() else {
        unreachable!()
    };
    let canonical = crate::chat::Session {
        id: terminal.id,
        launch: crate::chat::Launch {
            provider: crate::agent::Provider::Claude,
            recipe: terminal.launch,
            session_id: None,
        },
        proof: None,
        terminal: None,
        owned_codex_listener: None,
        native_continuity: Default::default(),
    };
    app.shared_views.chat_adoptions.insert(
        CardId::new(),
        crate::session::ChatIdentityAdoption {
            card: CardId::new(),
            expected: TerminalId::new(),
            canonical,
        },
    );
    assert!(
        contains(&app),
        "ordered chat identity custody retains the full projection"
    );
    app.shared_views.chat_adoptions.clear();
    assert!(
        !contains(&app),
        "once custody settles, empty history stays sparse"
    );
}

#[test]
fn a_missing_saved_identity_keeps_each_cached_registry_migration_receipt() {
    let (mut app, model, _) = desktop();
    Arc::make_mut(&mut app.durable).workspace_model_identity = None;
    let (tx, rx) = async_channel::unbounded();
    app.workspace_model_tx = tx;
    drop(app.apply_workspace_model_update(ModelUpdate::Connected(model.clone())));
    let seeds = std::iter::from_fn(|| rx.try_recv().ok())
        .filter_map(|edit| match edit {
            ModelEdit::SeedView { scope, panes, .. } => Some((scope, panes.len())),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        seeds.len(),
        76,
        "joining a new registry retains real migration obligations"
    );
    assert_eq!(seeds.iter().map(|(_, panes)| panes).sum::<usize>(), 83);
    for (scope, _) in seeds {
        app.shared_views.local_projection_entries.set(0);
        drop(app.apply_workspace_model_update(ModelUpdate::Read(model.clone())));
        assert!(
            app.shared_views.seeding.contains(&scope),
            "observer echoes never acknowledge another transport's seed"
        );
        drop(app.apply_workspace_model_update(ModelUpdate::ViewApplied {
            scope,
            operation: None,
            model: model.clone(),
        }));
        assert!(
            app.shared_views.local_projection_entries.get() <= 153,
            "two full reconciliation messages and one exact receipt stay bounded by live channels"
        );
    }
    assert!(app.shared_views.seeding.is_empty());
    assert_eq!(app.shared_views.initialized.len(), 1856);
    assert!(
        !std::iter::from_fn(|| rx.try_recv().ok()).any(|edit| matches!(
            edit,
            ModelEdit::SeedView { .. } | ModelEdit::ChangeView { .. }
        ))
    );
}

#[test]
fn a_cold_unknown_registry_admits_equal_views_without_preinstall_seeding() {
    let (mut app, model, _) = desktop();
    app.workspace_model = crate::workspace_model::WorkspaceModel::default();
    Arc::make_mut(&mut app.durable).workspace_model_identity = None;
    let (tx, rx) = async_channel::unbounded();
    app.workspace_model_tx = tx;
    drop(app.apply_workspace_model_update(ModelUpdate::Connected(model.clone())));
    assert_eq!(app.durable.workspace_model_identity, Some(model.identity));
    assert_eq!(app.shared_views.initialized.len(), 1856);
    assert!(app.shared_views.seeding.is_empty());
    assert!(
        !std::iter::from_fn(|| rx.try_recv().ok()).any(|edit| matches!(
            edit,
            ModelEdit::SeedView { .. } | ModelEdit::ChangeView { .. }
        ))
    );
    assert_eq!(
        app.durable
            .cards
            .iter()
            .filter(|card| card.existence.is_open())
            .count(),
        83
    );
}
