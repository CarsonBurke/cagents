use super::*;

/// What the person is told when a close of `card` was refused: the pane, by
/// its title, closed here and may still show in other windows.
fn closed_here_notice(app: &Cagents, card: CardId) -> String {
    let title = app
        .pane_notice_title(card)
        .expect("a closed pane keeps the title it is named by");
    let notice = pane_closed_here_kept_elsewhere(Some(&title));
    assert!(notice.starts_with(&format!("\u{201c}{title}\u{201d} closed here")));
    notice
}

#[test]
fn refused_shared_close_readmits_only_its_obsolete_surface_without_retargeting_intent() {
    use crate::workspace_model::views::TerminalRemovalBasis;
    for newer_surface in [false, true] {
        let directory = tempfile::tempdir_in("/var/tmp").unwrap();
        let (mut app, _, _, mut pane) = channel_move_fixture();
        let owner = HostSessionProof {
            host_session_id: "fixture".into(),
            boot_id: "boot".into(),
            host_incarnation: "incarnation".into(),
        };
        let mut source = fresh_terminal_session(LaunchRecipe::shell("/repo".into()).unwrap());
        source.continuity = TerminalContinuity::Live {
            host_session_id: owner.host_session_id.clone(),
            boot_id: owner.boot_id.clone(),
            host_incarnation: owner.host_incarnation.clone(),
        };
        pane.kind = CardKind::Terminal(source.clone());
        app.workspace_model.views[0].panes[0] = pane.clone();
        app.card_state_mut(pane.id).unwrap().kind = pane.kind.clone();
        let original_worker = uuid::Uuid::new_v4();
        let surface = crate::app::tests::unconnected_surface(directory.path(), source.id);
        surface.set_worker_receipt_for_test(&owner, original_worker);
        app.cards.insert(pane.id, CardRuntime::Terminal(surface));
        assert!(app.capture_shared_removal(pane.id));
        let original_basis = TerminalRemovalBasis::Worker {
            owner: owner.clone(),
            instance: original_worker,
        };
        assert_eq!(
            app.terminal_cleanup_authority(pane.id).worker(),
            Some(&original_basis)
        );
        // A's surface can retain its exit after the canonical source is B.
        if let CardKind::Terminal(local) = &mut app.card_state_mut(pane.id).unwrap().kind {
            local.continuity = TerminalContinuity::Exited {
                last_exit: crate::model::TerminalExit {
                    code: Some(0),
                    signal: None,
                    occurred_at_unix_ms: 1,
                },
            };
        }
        let replacement_worker = uuid::Uuid::new_v4();
        if newer_surface {
            app.card_state_mut(pane.id).unwrap().kind = pane.kind.clone();
            let current = crate::app::tests::unconnected_surface(directory.path(), source.id);
            current.set_worker_receipt_for_test(&owner, replacement_worker);
            app.cards.insert(pane.id, CardRuntime::Terminal(current));
        }
        let (tx, published) = async_channel::unbounded();
        app.workspace_model_tx = tx;
        drop(app.readmit_shared_terminal_after_source_refusal(pane.id));
        assert_eq!(
            app.terminal_cleanup_authority(pane.id).worker(),
            Some(&original_basis),
            "refreshing display never grants B to the old Close"
        );
        if newer_surface {
            let Some(CardRuntime::Terminal(current)) = app.cards.get(&pane.id) else {
                panic!("new surface must survive")
            };
            assert_eq!(current.execution_instance(), Some(replacement_worker));
            app.reset_shared_removal_for_new_close(pane.id);
            assert!(app.capture_shared_removal(pane.id));
            assert_eq!(
                app.terminal_cleanup_authority(pane.id).worker(),
                Some(&TerminalRemovalBasis::Worker {
                    owner: owner.clone(),
                    instance: replacement_worker,
                })
            );
        } else {
            assert!(matches!(
                app.cards.get(&pane.id),
                Some(CardRuntime::Loading)
            ));
            assert_eq!(app.card_state(pane.id).unwrap().kind, pane.kind);
        }
        assert!(
            published.try_recv().is_err(),
            "readmission is not pane deletion or execution"
        );
    }
}

#[test]
fn completed_shared_close_cannot_adopt_a_replacement_worker_receipt() {
    use crate::workspace_model::views::TerminalRemovalBasis;
    let mut source = fresh_terminal_session(LaunchRecipe::shell("/repo".into()).unwrap());
    source.continuity = TerminalContinuity::RelaunchAvailable {
        recipe: source.launch.clone(),
        last_exit: Some(crate::model::TerminalExit {
            code: Some(0),
            signal: None,
            occurred_at_unix_ms: 1,
        }),
    };
    let (mut app, _, _, mut pane) = channel_move_fixture();
    pane.kind = CardKind::Terminal(source);
    app.workspace_model.views[0].panes[0] = pane.clone();
    app.card_state_mut(pane.id).unwrap().kind = pane.kind.clone();
    app.cards.remove(&pane.id);
    assert!(app.capture_shared_removal(pane.id));
    let replacement = TerminalRemovalBasis::Worker {
        owner: HostSessionProof {
            host_session_id: "fixture-host".into(),
            boot_id: "fixture-boot".into(),
            host_incarnation: "fixture-incarnation".into(),
        },
        instance: uuid::Uuid::new_v4(),
    };
    // The current inventory may name B, even though the saved pane still
    // describes A. Neither a completion nor Active/Confirm grants B to Close.
    let authority = app.terminal_cleanup_authority(pane.id);
    assert!(authority.requires_absence());
    assert!(!authority.permits(&replacement));
    assert!(!app.shared_removal_terminal_basis(pane.id, replacement));
    assert!(!app.shared_removal_ready(pane.id));
    assert!(app.card_state(pane.id).unwrap().existence.is_open());
}

#[test]
fn active_confirmation_preserves_the_captured_worker_authority() {
    use super::super::TerminalCleanupAuthority;
    use crate::workspace_model::views::TerminalRemovalBasis;
    let owner = HostSessionProof {
        host_session_id: "fixture-host".into(),
        boot_id: "fixture-boot".into(),
        host_incarnation: "fixture-incarnation".into(),
    };
    let original = TerminalRemovalBasis::Worker {
        owner: owner.clone(),
        instance: uuid::Uuid::new_v4(),
    };
    let replacement = TerminalRemovalBasis::Worker {
        owner,
        instance: uuid::Uuid::new_v4(),
    };
    let authority = TerminalCleanupAuthority::Shared {
        basis: Some(original.clone()),
        capture: None,
    };
    assert!(authority.permits(&original));
    assert!(!authority.permits(&replacement));
}

#[test]
fn missing_worker_receipt_can_only_probe_a_shared_live_terminal() {
    let mut source = fresh_terminal_session(LaunchRecipe::shell("/repo".into()).unwrap());
    source.continuity = TerminalContinuity::Live {
        host_session_id: "fixture-host".into(),
        boot_id: "fixture-boot".into(),
        host_incarnation: "fixture-incarnation".into(),
    };
    let (mut app, _, _, pane) = channel_move_fixture_with_kind(Some(CardKind::Terminal(source)));
    app.uncertain_terminal_proofs.remove(&pane.id);
    drop(app.begin_card_close(pane.id));
    assert!(app.terminal_close.as_ref().unwrap().is_probe_only());
    assert!(app.card_state(pane.id).unwrap().existence.is_open());
}

#[test]
fn queued_close_continuation_keeps_the_original_shared_source() {
    let (mut app, _, _, original) = channel_move_fixture();
    assert!(app.capture_shared_removal(original.id));
    app.queue_card_close(original.id, false);
    let mut replacement = original.clone();
    if let CardKind::Browser(browser) = &mut replacement.kind {
        browser.id = BrowserId::new();
    }
    app.workspace_model.views[0].panes[0] = replacement.clone();
    app.card_state_mut(original.id).unwrap().kind = replacement.kind;
    drop(app.next_queued_card_close());
    assert!(app.card_state(original.id).unwrap().existence.is_open());
    assert!(app.workspace.pending_close().is_none());
}

#[test]
fn shared_close_keeps_its_captured_source_through_focus_handoff() {
    let (mut app, channel, _, original) = channel_move_fixture();
    let (tx, published) = async_channel::unbounded();
    app.workspace_model_tx = tx;
    assert!(app.capture_shared_removal(original.id));
    assert!(app.prepare_shared_removal(original.id));
    let mut replacement = original.clone();
    if let CardKind::Browser(browser) = &mut replacement.kind {
        browser.id = BrowserId::new();
    }
    app.workspace_model.views[0].panes[0] = replacement.clone();
    app.workspace_model.views[0].revision += 1;
    app.card_state_mut(original.id).unwrap().kind = replacement.kind.clone();
    assert!(!app.prepare_shared_removal(original.id));
    assert!(app.card_state(original.id).unwrap().existence.is_open());
    assert!(published.try_recv().is_err());
    assert_eq!(
        app.local_view_panes(&Scope::Channel(channel))[0].kind,
        replacement.kind
    );
}

#[test]
fn local_scope_change_cannot_borrow_captured_shared_close_consent() {
    let (mut app, _, _, original) = channel_move_fixture();
    assert!(app.capture_shared_removal(original.id));
    Arc::make_mut(&mut app.durable)
        .card_scopes
        .insert(original.id, "/repo/elsewhere".into());
    assert!(!app.prepare_shared_removal(original.id));
    assert!(app.card_state(original.id).unwrap().existence.is_open());
}

#[test]
fn unpublished_close_cannot_acquire_a_later_shared_replacement() {
    let (mut app, channel, _, original) = channel_move_fixture();
    app.workspace_model.views.clear();
    app.shared_views.baseline.clear();
    app.shared_views.attached.remove(&original.id);
    assert!(app.capture_shared_removal(original.id));
    app.workspace_model.views = vec![View {
        panes: vec![original.clone()],
        ..View::empty(channel)
    }];
    assert!(!app.prepare_shared_removal(original.id));
}

#[test]
fn canonical_retirement_before_cleanup_reply_needs_no_new_removal() {
    let (mut app, channel, _, original) = channel_move_fixture();
    assert!(app.capture_shared_removal(original.id));
    app.workspace_model.views[0]
        .apply(&Change::Remove { id: original.id })
        .unwrap();
    assert!(app.prepare_shared_removal(original.id));
    assert!(
        app.shared_removal_change(original.id, &Scope::Channel(channel))
            .is_none()
    );
}

#[test]
fn shared_close_publication_contains_the_original_source_and_registry() {
    let (mut app, channel, _, original) = channel_move_fixture();
    let (tx, published) = async_channel::unbounded();
    app.workspace_model_tx = tx;
    assert!(app.capture_shared_removal(original.id));
    app.workspace.request_close(original.id).unwrap();
    drop(app.commit_card_removal(original.id, None));
    app.publish_view_changes();
    let changes = std::iter::from_fn(|| published.try_recv().ok())
        .filter_map(|edit| {
            if let ModelEdit::ChangeView { scope, change, .. } = edit.edit {
                Some((scope, change))
            } else {
                None
            }
        })
        .collect::<Vec<_>>();
    assert!(changes.iter().any(|(scope, change)| matches!(change,
        Change::RemoveSource { registry, before, .. }
        if scope == &Scope::Channel(channel) && *registry == app.workspace_model.identity && before == &original)));
    assert!(
        !changes
            .iter()
            .any(|(_, change)| matches!(change, Change::Remove { .. }))
    );
}

#[test]
fn a_cold_completed_source_readmits_a_live_successor_without_retargeting_close() {
    let (mut app, _, _, mut pane) = channel_move_fixture();
    let mut source = fresh_terminal_session(LaunchRecipe::shell("/repo".into()).unwrap());
    source.continuity = TerminalContinuity::Exited {
        last_exit: crate::model::TerminalExit {
            code: Some(0),
            signal: None,
            occurred_at_unix_ms: 1,
        },
    };
    pane.kind = CardKind::Terminal(source.clone());
    app.workspace_model.views[0].panes[0] = pane.clone();
    app.card_state_mut(pane.id).unwrap().kind = pane.kind.clone();
    app.cards.insert(pane.id, CardRuntime::Completed);
    let owned = HashSet::from([pane.id]);
    let ended = app.workspace_model.views[0].clone();
    drop(app.project_shared_view(&ended, &owned));
    assert!(app.capture_shared_removal(pane.id));
    let captured = app.terminal_cleanup_authority(pane.id).capture().cloned();
    source.continuity = TerminalContinuity::DetachedSameBoot {
        host_session_id: "fixture-host".into(),
        boot_id: "fixture-boot".into(),
        host_incarnation: "fixture-daemon".into(),
    };
    let mut live = ended;
    live.revision += 1;
    live.panes[0].kind = CardKind::Terminal(source.clone());
    app.workspace_model.views[0] = live.clone();
    drop(app.project_shared_view(&live, &owned));
    assert!(matches!(
        app.cards.get(&pane.id),
        Some(CardRuntime::Loading)
    ));
    assert!(app.ended_terminal(pane.id).is_none());
    assert_eq!(
        app.card_state(pane.id).unwrap().kind,
        CardKind::Terminal(source)
    );
    assert_eq!(
        app.terminal_cleanup_authority(pane.id).capture(),
        captured.as_ref()
    );
}

#[test]
fn a_resolved_receipt_cannot_fill_another_gestures_missing_authority() {
    use crate::workspace_model::views::TerminalRemovalBasis;
    let (mut app, _, _, mut pane) = channel_move_fixture();
    pane.kind = CardKind::Terminal(fresh_terminal_session(
        LaunchRecipe::shell("/repo".into()).unwrap(),
    ));
    app.workspace_model.views[0].panes[0] = pane.clone();
    app.card_state_mut(pane.id).unwrap().kind = pane.kind.clone();
    app.cards.insert(pane.id, CardRuntime::Completed);
    assert!(app.capture_shared_removal(pane.id));
    let basis = TerminalRemovalBasis::Absent {
        owner: HostSessionProof {
            host_session_id: "fixture-host".into(),
            boot_id: "fixture-boot".into(),
            host_incarnation: "fixture-daemon".into(),
        },
        latest_worker: None,
        history_epoch: 0,
    };
    let mut receipt = app.terminal_cleanup_authority(pane.id).receipt(basis);
    receipt.capture.as_mut().unwrap().model_sequence += 1;
    assert!(!app.shared_removal_terminal_receipt(pane.id, receipt));
    assert!(!app.shared_removal_ready(pane.id));
}

#[test]
fn close_capture_uses_only_complete_authority_for_its_scope() {
    let (mut app, _, _, pane) = channel_move_fixture();
    let scope = app.workspace_model.views[0].scope.clone();
    app.shared_views.snapshot_sequence = 10;
    app.workspace_model.sequence = 900; // metadata alone did not read the pane
    app.shared_views.view_sequences.insert(scope, 20);
    let terminal = CardKind::Terminal(fresh_terminal_session(
        LaunchRecipe::shell("/repo".into()).unwrap(),
    ));
    app.workspace_model.views[0].panes[0].kind = terminal.clone();
    app.card_state_mut(pane.id).unwrap().kind = terminal;
    app.cards.insert(pane.id, CardRuntime::Completed);
    assert!(app.capture_shared_removal(pane.id));
    assert_eq!(
        app.terminal_cleanup_authority(pane.id)
            .capture()
            .unwrap()
            .model_sequence,
        20
    );
}

#[test]
fn a_refused_close_readmits_an_ended_successor_as_retained_output_only() {
    let directory = tempfile::tempdir_in("/var/tmp").unwrap();
    let (mut app, _, _, mut pane) = channel_move_fixture();
    let owner = HostSessionProof {
        host_session_id: "fixture".into(),
        boot_id: "boot".into(),
        host_incarnation: "daemon".into(),
    };
    let mut source = fresh_terminal_session(LaunchRecipe::shell("/repo".into()).unwrap());
    source.continuity = TerminalContinuity::Live {
        host_session_id: owner.host_session_id.clone(),
        boot_id: owner.boot_id.clone(),
        host_incarnation: owner.host_incarnation.clone(),
    };
    pane.kind = CardKind::Terminal(source.clone());
    app.workspace_model.views[0].panes[0] = pane.clone();
    app.card_state_mut(pane.id).unwrap().kind = pane.kind.clone();
    let old = uuid::Uuid::new_v4();
    let surface = crate::app::tests::unconnected_surface(directory.path(), source.id);
    surface.set_worker_receipt_for_test(&owner, old);
    app.cards.insert(pane.id, CardRuntime::Terminal(surface));
    assert!(app.capture_shared_removal(pane.id));
    source.continuity = TerminalContinuity::Exited {
        last_exit: crate::model::TerminalExit {
            code: Some(0),
            signal: None,
            occurred_at_unix_ms: 42,
        },
    };
    app.workspace_model.views[0].panes[0].kind = CardKind::Terminal(source.clone());
    drop(app.readmit_shared_terminal_after_source_refusal(pane.id));
    assert!(matches!(
        app.cards.get(&pane.id),
        Some(CardRuntime::Completed)
    ));
    assert_eq!(
        app.card_state(pane.id).unwrap().kind,
        CardKind::Terminal(source)
    );
    assert_eq!(
        app.terminal_cleanup_authority(pane.id).worker(),
        Some(&views::TerminalRemovalBasis::Worker {
            owner,
            instance: old
        })
    );
}

/// This window changed the pane's source and the registry has not answered:
/// the window shows the new source, and its update is published ahead of Close.
fn with_own_source_update_pending(app: &mut Cagents, original: &Pane) -> Pane {
    let (tx, _published) = async_channel::unbounded();
    app.workspace_model_tx = tx;
    let mut shown = original.clone();
    if let CardKind::Browser(browser) = &mut shown.kind {
        browser.id = BrowserId::new();
    }
    app.card_state_mut(original.id).unwrap().kind = shown.kind.clone();
    app.publish_view_changes();
    assert!(app.shared_views.pending.iter().any(|(_, _, change)| matches!(
        change,
        Change::Update { before, pane } if before.kind == original.kind && pane.kind == shown.kind
    )));
    shown
}

#[test]
fn close_during_this_windows_own_update_removes_the_source_it_shows() {
    let (mut app, channel, _, original) = channel_move_fixture();
    let shown = with_own_source_update_pending(&mut app, &original);

    assert!(app.capture_shared_removal(original.id));
    assert!(app.prepare_shared_removal(original.id));
    // The update lands; the Close still refers to the same source.
    app.workspace_model.views[0].panes[0] = shown.clone();
    app.workspace_model.views[0].revision += 1;
    app.shared_views.pending.clear();
    assert!(app.prepare_shared_removal(original.id));
    assert!(matches!(
        app.shared_removal_change(original.id, &Scope::Channel(channel)),
        Some(Change::RemoveSource { before, .. }) if before == shown
    ));
}

#[test]
fn close_during_this_windows_own_update_cannot_take_another_windows_replacement() {
    let (mut app, _, _, original) = channel_move_fixture();
    with_own_source_update_pending(&mut app, &original);
    // Another window replaced the source first, so the registry refuses the
    // update this window still shows.
    let mut replacement = original.clone();
    if let CardKind::Browser(browser) = &mut replacement.kind {
        browser.id = BrowserId::new();
    }
    app.workspace_model.views[0].panes[0] = replacement;
    app.workspace_model.views[0].revision += 1;

    assert!(!app.capture_shared_removal(original.id));
    assert!(app.card_state(original.id).unwrap().existence.is_open());
}

#[test]
fn close_captured_during_own_update_refuses_once_that_update_is_refused() {
    let (mut app, _, _, original) = channel_move_fixture();
    with_own_source_update_pending(&mut app, &original);
    assert!(app.capture_shared_removal(original.id));

    let mut replacement = original.clone();
    if let CardKind::Browser(browser) = &mut replacement.kind {
        browser.id = BrowserId::new();
    }
    app.workspace_model.views[0].panes[0] = replacement.clone();
    app.workspace_model.views[0].revision += 1;
    // Before and after the refusal returns this window to the registry's pane.
    assert!(!app.prepare_shared_removal(original.id));
    app.shared_views.pending.clear();
    app.card_state_mut(original.id).unwrap().kind = replacement.kind;
    assert!(!app.prepare_shared_removal(original.id));
    assert!(app.card_state(original.id).unwrap().existence.is_open());
}

#[test]
fn a_refused_own_update_leaves_close_to_the_registrys_pane() {
    let (mut app, channel, _, original) = channel_move_fixture();
    with_own_source_update_pending(&mut app, &original);
    // Another window's replacement reached this window's pane first; this
    // window's earlier update will be refused, and Close refers to what it shows.
    let mut replacement = original.clone();
    if let CardKind::Browser(browser) = &mut replacement.kind {
        browser.id = BrowserId::new();
    }
    app.workspace_model.views[0].panes[0] = replacement.clone();
    app.workspace_model.views[0].revision += 1;
    app.card_state_mut(original.id).unwrap().kind = replacement.kind.clone();

    assert!(app.capture_shared_removal(original.id));
    assert!(matches!(
        app.shared_removal_change(original.id, &Scope::Channel(channel)),
        Some(Change::RemoveSource { before, .. }) if before == replacement
    ));
}

/// Closes `original` in this window and returns the Close it published.
fn published_close(app: &mut Cagents, name: &str, original: &Pane) -> (uuid::Uuid, Scope, Change) {
    let (tx, _published) = async_channel::unbounded();
    app.workspace_model_tx = tx;
    assert!(
        app.capture_shared_removal(original.id),
        "{name}: the close captures its source"
    );
    app.workspace.request_close(original.id).unwrap();
    drop(app.commit_card_removal(original.id, None));
    app.publish_view_changes();
    app.shared_views
        .pending
        .iter()
        .find_map(|(operation, scope, change)| {
            matches!(change, Change::RemoveSource { before, .. } if before == original)
                .then(|| (*operation, scope.clone(), change.clone()))
        })
        .unwrap_or_else(|| {
            panic!(
                "{name}: the close is published: pending {:?}, status {:?}, removals {:?}",
                app.shared_views.pending,
                app.status,
                app.shared_views.removals.keys().collect::<Vec<_>>()
            )
        })
}

/// The registry refuses `change` and reports `view` as the scope it now
/// holds, as the publisher delivers a refused Close or source update.
fn registry_refuses(app: &mut Cagents, operation: uuid::Uuid, change: Change, mut view: View) {
    use super::super::workspace_model_sync::ModelUpdate;
    view.revision += 1;
    let scope = view.scope.clone();
    let identity = app.workspace_model.identity;
    drop(
        app.apply_workspace_model_update(ModelUpdate::ScopeConflicted {
            identity: Some(identity),
            scope,
            operation,
            change,
            views: crate::workspace_model::delta::ViewChanges {
                sequence: 1_000,
                views: vec![view],
                ..Default::default()
            },
            error: crate::workspace_model::views::REMOVAL_SOURCE_CONFLICT.into(),
        }),
    );
}

fn notices(app: &Cagents) -> Vec<String> {
    app.status
        .iter()
        .chain(app.notices.recent().map(|entry| &entry.notice))
        .map(|notice| notice.message.to_string())
        .collect()
}

/// Each pane a Close is tested on: a browser or a terminal, shown in its
/// channel or in a worktree's own view.
const CLOSE_CASES: [(&str, bool, bool); 4] = [
    ("browser in a channel", false, false),
    ("browser in a worktree", false, true),
    ("terminal in a channel", true, false),
    ("terminal in a worktree", true, true),
];

fn close_fixture(directory: &std::path::Path, terminal: bool, worktree: bool) -> (Cagents, Pane) {
    let kind = terminal.then(moving_terminal_kind);
    let (mut app, channel, _, mut pane) = channel_move_fixture_with_kind(kind);
    if worktree {
        let scope = Scope::Worktree {
            repository: PathBuf::from("/repo/.git"),
            path: PathBuf::from("/repo/worktree"),
        };
        pane.scope = Some(PathBuf::from("/repo/worktree"));
        let view = app
            .workspace_model
            .views
            .iter_mut()
            .find(|view| view.scope == Scope::Channel(channel))
            .unwrap();
        view.scope = scope.clone();
        view.panes[0] = pane.clone();
        // The registry and this window agree: the pane is the worktree's,
        // and its old channel is empty.
        let mut emptied = View::empty(channel);
        emptied.revision = 17;
        app.workspace_model.views.push(emptied);
        app.shared_views.initialize(scope, vec![pane.clone()]);
        app.shared_views.initialize(Scope::Channel(channel), vec![]);
        app.sidebar_model.projects.push(SidebarProject::new(
            ProjectId::new(),
            RepositoryId::new(),
            "repo".into(),
            PathBuf::from("/repo/.git"),
            vec![],
            vec![],
        ));
        let durable = Arc::make_mut(&mut app.durable);
        durable.active_channel = None;
        for channel in &mut durable.channels {
            channel.cards.retain(|card| *card != pane.id);
        }
        durable
            .card_scopes
            .insert(pane.id, PathBuf::from("/repo/worktree"));
        drop(app.reconcile_shared_views());
    }
    if let CardKind::Terminal(session) = &pane.kind {
        // A live terminal's Close captures the worker it closes.
        let owner = session.continuity.live_proof().unwrap();
        let surface = crate::app::tests::unconnected_surface(directory, session.id);
        surface.set_worker_receipt_for_test(&owner, uuid::Uuid::new_v4());
        app.cards.insert(pane.id, CardRuntime::Terminal(surface));
    }
    // Only what the Close itself reports matters here.
    app.status = None;
    app.notices.entries.clear();
    assert!(
        app.workspace.order().any(|id| id == pane.id),
        "terminal {terminal}, worktree {worktree}: the pane is shown"
    );
    (app, pane)
}

fn view_holding(app: &Cagents, scope: &Scope) -> View {
    app.workspace_model
        .views
        .iter()
        .find(|view| &view.scope == scope)
        .unwrap()
        .clone()
}

/// Projecting while a Close is on its way overlays it once, without
/// reporting anything: the pane is simply gone (#2905).
#[test]
fn a_pending_close_projects_without_a_notice() {
    let directory = tempfile::tempdir_in("/var/tmp").unwrap();
    for (name, terminal, worktree) in CLOSE_CASES {
        let (mut app, original) = close_fixture(directory.path(), terminal, worktree);
        let (operation, ..) = published_close(&mut app, name, &original);
        drop(app.reconcile_shared_views());
        drop(app.reconcile_shared_views());
        assert_eq!(notices(&app), Vec::<String>::new(), "{name}");
        assert!(!app.workspace.order().any(|id| id == original.id), "{name}");
        assert!(
            app.shared_views
                .pending
                .iter()
                .any(|(id, ..)| *id == operation),
            "{name}: the Close still waits for its answer"
        );
    }
}

/// Closing an agent's conversation retires its pane in the registry as
/// well, which can land before this window's own Close is answered (#2905).
#[test]
fn a_close_retired_first_reports_nothing() {
    let directory = tempfile::tempdir_in("/var/tmp").unwrap();
    for (name, terminal, worktree) in CLOSE_CASES {
        let (mut app, original) = close_fixture(directory.path(), terminal, worktree);
        let (operation, scope, change) = published_close(&mut app, name, &original);
        let mut retired = view_holding(&app, &scope);
        retired.apply(&Change::Remove { id: original.id }).unwrap();
        retired.revision += 1;
        let held = app
            .workspace_model
            .views
            .iter_mut()
            .find(|view| view.scope == scope)
            .unwrap();
        *held = retired.clone();
        drop(app.reconcile_shared_views());
        registry_refuses(&mut app, operation, change, retired);
        assert_eq!(notices(&app), Vec::<String>::new(), "{name}");
        assert!(!app.workspace.order().any(|id| id == original.id), "{name}");
        assert!(
            !app.shared_views
                .pending
                .iter()
                .any(|(id, ..)| *id == operation),
            "{name}"
        );
    }
}

/// A refused Close whose pane the registry no longer holds at all, as when
/// its scope's last pane went with another window's edit, left the pane
/// closed, as the person asked.
#[test]
fn a_refused_close_of_a_pane_already_gone_reports_nothing() {
    let directory = tempfile::tempdir_in("/var/tmp").unwrap();
    for (name, terminal, worktree) in CLOSE_CASES {
        let (mut app, original) = close_fixture(directory.path(), terminal, worktree);
        let (operation, scope, change) = published_close(&mut app, name, &original);
        let mut gone = view_holding(&app, &scope);
        gone.panes.clear();
        registry_refuses(&mut app, operation, change, gone);
        assert_eq!(notices(&app), Vec::<String>::new(), "{name}");
        assert!(!app.workspace.order().any(|id| id == original.id), "{name}");
    }
}

/// The removal of `card` this window sent again after `refused`, if any.
fn resent_close(app: &Cagents, card: CardId, refused: uuid::Uuid) -> Option<(uuid::Uuid, Change)> {
    app.shared_views
        .pending
        .iter()
        .find_map(|(operation, _, change)| {
            (*operation != refused
                && matches!(change, Change::RemoveSource { before, .. } if before.id == card))
            .then(|| (*operation, change.clone()))
        })
}

/// A Close the registry refused while it still holds the pane leaves the
/// pane closed here: its identity is never reopened, which storage would
/// refuse (#2093, #2444). Other windows may still show it, which is what the
/// person is told; nothing is sent again behind their back.
#[test]
fn a_refused_close_keeps_the_pane_closed_and_says_other_windows_may_show_it() {
    let directory = tempfile::tempdir_in("/var/tmp").unwrap();
    for (name, terminal, worktree) in CLOSE_CASES {
        let (mut app, original) = close_fixture(directory.path(), terminal, worktree);
        let (operation, scope, change) = published_close(&mut app, name, &original);
        let held = view_holding(&app, &scope);
        assert!(held.panes.contains(&original), "{name}");
        registry_refuses(&mut app, operation, change, held);
        assert_eq!(
            notices(&app),
            [closed_here_notice(&app, original.id)],
            "{name}"
        );
        assert!(!app.workspace.order().any(|id| id == original.id), "{name}");
        assert!(
            !app.card_state(original.id)
                .is_some_and(|card| card.existence.is_open()),
            "{name}: the closed card stays closed"
        );
        assert!(
            !app.shared_views
                .pending
                .iter()
                .any(|(id, ..)| *id == operation),
            "{name}"
        );
        drop(app.reconcile_shared_views());
        assert_eq!(resent_close(&app, original.id, operation), None, "{name}");
        assert!(!app.workspace.order().any(|id| id == original.id), "{name}");
    }
}

/// A Close the registry refuses without reporting the scope, as when its
/// resend waited too long - the daemon trouble behind #2444 - leaves the
/// pane closed here, and says other windows may show it only while the
/// observed view still holds it. The registry's own wording stays in the trace.
#[test]
fn a_close_refused_without_a_view_says_only_what_happened_to_the_pane() {
    use super::super::workspace_model_sync::ModelUpdate;
    let directory = tempfile::tempdir_in("/var/tmp").unwrap();
    for retired in [false, true] {
        let (mut app, original) = close_fixture(directory.path(), false, false);
        let (operation, scope, _) = published_close(&mut app, "browser in a channel", &original);
        if retired {
            let held = app
                .workspace_model
                .views
                .iter_mut()
                .find(|view| view.scope == scope)
                .unwrap();
            held.apply(&Change::Remove { id: original.id }).unwrap();
            held.revision += 1;
            drop(app.reconcile_shared_views());
        }
        drop(app.apply_workspace_model_update(ModelUpdate::ViewRefused {
            scope,
            operation,
            error: "The pane change waited too long for the daemon to be resent".into(),
        }));
        let expected: Vec<String> = if retired {
            Vec::new()
        } else {
            vec![closed_here_notice(&app, original.id)]
        };
        assert_eq!(notices(&app), expected, "retired={retired}");
        assert!(
            !app.workspace.order().any(|id| id == original.id),
            "retired={retired}"
        );
        assert_eq!(
            resent_close(&app, original.id, operation),
            None,
            "retired={retired}"
        );
        assert!(
            !app.shared_views
                .pending
                .iter()
                .any(|(id, ..)| *id == operation),
            "retired={retired}"
        );
    }
}

/// A terminal whose Close the registry refused stays closed: its shell was
/// already ended by that Close, so nothing is left to close again, and
/// nothing still counts it as closing (#3266).
#[test]
fn a_terminal_whose_close_was_refused_stays_closed() {
    use crate::app::terminal_close::CloseOutcome;
    let directory = tempfile::tempdir_in("/var/tmp").unwrap();
    let (mut app, original) = close_fixture(directory.path(), true, false);
    let card = original.id;
    let (tx, _published) = async_channel::unbounded();
    app.workspace_model_tx = tx;
    drop(app.update(Message::CloseCard(card)));
    let intent = app
        .terminal_close
        .clone()
        .expect("the Close asks the daemon to end the terminal");
    let authority = app.terminal_cleanup_authority(card);
    let worker = authority.worker().cloned().unwrap();
    let receipt = authority.receipt(worker);
    drop(app.update(Message::TerminalCloseFinished {
        card,
        terminal: intent.terminal,
        proof: intent.proof,
        generation: intent.generation,
        outcome: CloseOutcome::ClosedWithBasis(receipt),
    }));
    drop(app.update(Message::CommitClose(card)));
    app.publish_view_changes();
    let (operation, scope, change) = app
        .shared_views
        .pending
        .iter()
        .find_map(|(operation, scope, change)| {
            matches!(change, Change::RemoveSource { before, .. } if before.id == card)
                .then(|| (*operation, scope.clone(), change.clone()))
        })
        .expect("the Close is published");
    let held = view_holding(&app, &scope);
    registry_refuses(&mut app, operation, change, held);
    assert_eq!(notices(&app), [closed_here_notice(&app, card)]);
    assert!(!app.workspace.order().any(|id| id == card));
    assert!(
        !app.cards.contains_key(&card),
        "nothing attaches the closed terminal"
    );
    assert!(
        !app.closed_terminal_cards.contains(&card) && !app.terminal_close_pending(card),
        "the closed pane is no longer closing"
    );
    assert!(
        app.terminal_close.is_none(),
        "the daemon is not asked again"
    );
    assert_eq!(resent_close(&app, card, operation), None);
}

/// When the pane changed to a successor in the registry, as another window
/// navigating it does, the Close is refused. The pane stays closed here, and
/// open in that window, which this one does not close for it.
#[test]
fn a_close_refused_for_a_successor_stays_closed_here_and_says_so() {
    let (mut app, channel, _, original) = channel_move_fixture();
    let (operation, scope, change) = published_close(&mut app, "browser in a channel", &original);
    let mut successor = view_holding(&app, &scope);
    let CardKind::Browser(browser) = &mut successor.panes[0].kind else {
        panic!("browser fixture")
    };
    browser.current_url = "https://example.com/next".parse().unwrap();
    assert_eq!(scope, Scope::Channel(channel));
    registry_refuses(&mut app, operation, change, successor);
    assert_eq!(notices(&app), [closed_here_notice(&app, original.id)]);
    assert!(!app.workspace.order().any(|id| id == original.id));
    let card = app.card_state(original.id).unwrap();
    assert!(!card.existence.is_open());
    assert_eq!(
        card.kind, original.kind,
        "the closed card keeps what it closed"
    );
    assert_eq!(
        resent_close(&app, original.id, operation),
        None,
        "another window's pane is not closed for it"
    );
}

/// A terminal's refused source update says nothing once the person has
/// closed its pane, and plainly that the pane changed elsewhere while it is
/// still shown.
#[test]
fn a_refused_terminal_source_update_speaks_only_for_a_shown_pane() {
    let directory = tempfile::tempdir_in("/var/tmp").unwrap();
    for close in [false, true] {
        for (name, terminal, worktree) in CLOSE_CASES {
            let (mut app, original) = close_fixture(directory.path(), terminal, worktree);
            if !matches!(original.kind, CardKind::Terminal(_)) {
                continue;
            }
            let scope = app
                .workspace_model
                .views
                .iter()
                .find(|view| view.panes.iter().any(|pane| pane.id == original.id))
                .unwrap()
                .scope
                .clone();
            let mut updated = original.clone();
            updated.pending_terminal_start = true;
            let change = Change::UpdateTerminalSource {
                before: original.clone(),
                pane: updated,
                worker: views::TerminalRemovalBasis::Absent {
                    owner: HostSessionProof {
                        host_session_id: "fixture".into(),
                        boot_id: "boot".into(),
                        host_incarnation: "incarnation".into(),
                    },
                    latest_worker: None,
                    history_epoch: 0,
                },
            };
            let operation = uuid::Uuid::new_v4();
            app.shared_views
                .pending
                .push((operation, scope.clone(), change.clone()));
            if close {
                published_close(&mut app, name, &original);
            }
            let held = view_holding(&app, &scope);
            registry_refuses(&mut app, operation, change, held);
            let expected: &[&str] = if close {
                &[]
            } else {
                &[PANE_CHANGED_ELSEWHERE]
            };
            assert_eq!(notices(&app), expected, "{name}, closed {close}");
        }
    }
}
