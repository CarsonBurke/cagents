use super::*;

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
            if let ModelEdit::ChangeView { scope, change, .. } = edit {
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
    let terminal = CardKind::Terminal(fresh_terminal_session(LaunchRecipe::shell("/repo".into()).unwrap()));
    app.workspace_model.views[0].panes[0].kind = terminal.clone();
    app.card_state_mut(pane.id).unwrap().kind = terminal;
    app.cards.insert(pane.id, CardRuntime::Completed);
    assert!(app.capture_shared_removal(pane.id));
    assert_eq!(app.terminal_cleanup_authority(pane.id).capture().unwrap().model_sequence, 20);
}

#[test]
fn a_refused_close_readmits_an_ended_successor_as_retained_output_only() {
    let directory = tempfile::tempdir_in("/var/tmp").unwrap();
    let (mut app, _, _, mut pane) = channel_move_fixture();
    let owner = HostSessionProof { host_session_id: "fixture".into(), boot_id: "boot".into(), host_incarnation: "daemon".into() };
    let mut source = fresh_terminal_session(LaunchRecipe::shell("/repo".into()).unwrap());
    source.continuity = TerminalContinuity::Live {
        host_session_id: owner.host_session_id.clone(), boot_id: owner.boot_id.clone(), host_incarnation: owner.host_incarnation.clone(),
    };
    pane.kind = CardKind::Terminal(source.clone());
    app.workspace_model.views[0].panes[0] = pane.clone();
    app.card_state_mut(pane.id).unwrap().kind = pane.kind.clone();
    let old = uuid::Uuid::new_v4();
    let surface = crate::app::tests::unconnected_surface(directory.path(), source.id);
    surface.set_worker_receipt_for_test(&owner, old);
    app.cards.insert(pane.id, CardRuntime::Terminal(surface));
    assert!(app.capture_shared_removal(pane.id));
    source.continuity = TerminalContinuity::Exited { last_exit: crate::model::TerminalExit {
        code: Some(0), signal: None, occurred_at_unix_ms: 42,
    }};
    app.workspace_model.views[0].panes[0].kind = CardKind::Terminal(source.clone());
    drop(app.readmit_shared_terminal_after_source_refusal(pane.id));
    assert!(matches!(app.cards.get(&pane.id), Some(CardRuntime::Completed)));
    assert_eq!(app.card_state(pane.id).unwrap().kind, CardKind::Terminal(source));
    assert_eq!(app.terminal_cleanup_authority(pane.id).worker(), Some(&views::TerminalRemovalBasis::Worker { owner, instance: old }));
}
