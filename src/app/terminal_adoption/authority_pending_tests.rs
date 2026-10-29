use super::*;
use crate::{
    model::{BootId, HostIncarnation, HostSessionId, RuntimeError},
    session::SessionRuntime,
    terminal::ipc::TerminalDescriptor,
};

fn pending_app(shared: bool) -> (Cagents, CardId, TerminalSession) {
    let mut app = super::super::tests::context_inspector_test_app(true);
    let card = app.workspace.focused().unwrap();
    let CardKind::Terminal(mut source) = app.card_state(card).unwrap().kind.clone() else {
        unreachable!()
    };
    source.launch = LaunchRecipe::shell("/repo".into()).unwrap();
    source.launch.arguments = vec!["--saved-command".into()];
    source
        .launch
        .environment
        .insert("SAVED_HOME".into(), "/saved/account".into());
    source
        .launch
        .unset_environment
        .insert("OTHER_ACCOUNT".into());
    source.transcript_path = Some("/saved/transcript.log".into());
    source.resume_hint = Some("saved-foreground-observation".into());
    source.recovery_origin = Some(TerminalId::new());
    source.predecessor = Some(TerminalId::new());
    source.continuity = TerminalContinuity::Live {
        host_session_id: HostSessionId::new("saved-host"),
        boot_id: BootId::new("pending-test-boot"),
        host_incarnation: HostIncarnation::new("saved-incarnation"),
    };
    app.card_state_mut(card).unwrap().kind = CardKind::Terminal(source.clone());
    if shared {
        app.shared_views.attached.insert(card);
    }
    app.cards.insert(card, CardRuntime::Loading);
    app.lifecycle.register(
        card,
        Exposure::Visible,
        Residency::Hydrating { epoch: 1 },
        Instant::now(),
    );
    (app, card, source)
}

fn pending_descriptor(source: &TerminalSession) -> TerminalDescriptor {
    let mut descriptor = TerminalDescriptor::relaunchable(source.id, source.launch.clone(), None);
    descriptor.session = source.clone();
    descriptor.session.continuity = TerminalContinuity::Failed {
        typed_error: RuntimeError {
            code: "terminal_authority_rebind_pending".into(),
            message: "Original terminal preserved; retry authenticated reconciliation".into(),
        },
        retryable: true,
    };
    descriptor.upgrade_sources = vec![source.clone()];
    descriptor
}

#[test]
fn queued_pending_inventory_preserves_the_whole_source_through_failed_attach_and_storage() {
    for shared in [false, true] {
        let (mut app, card, source) = pending_app(shared);
        let proof = source.continuity.live_proof().unwrap();
        let directory = tempfile::tempdir().unwrap();
        let (writer, _) = SessionRuntime::open(
            directory.path().join("pending.sqlite3"),
            proof.boot_id.clone(),
        )
        .unwrap();
        let store = writer.operation_store();
        writer.snapshot(app.durable.clone());
        writer.flush(Duration::from_secs(5)).unwrap();
        app.session = Some(writer);

        // The restored/shared attachment is already outstanding when inventory
        // reports that its surviving worker could not bind durable authority.
        app.begin_terminal_start(card);
        drop(app.observe_hosted_descriptors(vec![pending_descriptor(&source)]));
        assert_eq!(app.pending_adoptions.len(), 1);
        assert_eq!(app.terminal_starts_in_flight(), 1);
        assert_eq!(
            app.card_state(card).unwrap().kind,
            CardKind::Terminal(source.clone())
        );

        drop(app.terminal_started(card, source.id, Err("automatic attachment refused".into())));
        assert!(app.pending_adoptions.is_empty());
        assert_eq!(app.terminal_starts_in_flight(), 0);
        assert!(matches!(app.cards.get(&card), Some(CardRuntime::Failed(_))));
        assert_eq!(
            app.card_state(card).unwrap().kind,
            CardKind::Terminal(source.clone())
        );
        for _ in 0..3 {
            drop(app.observe_hosted_descriptors(vec![pending_descriptor(&source)]));
            assert_eq!(
                app.terminal_starts_in_flight(),
                0,
                "pending inventory must not retry attachment automatically"
            );
            assert_eq!(
                app.card_state(card).unwrap().kind,
                CardKind::Terminal(source.clone())
            );
        }
        app.persist();
        app.session
            .as_ref()
            .unwrap()
            .flush(Duration::from_secs(5))
            .unwrap();
        assert_eq!(
            store.load_card(card, Some(&proof)).unwrap().unwrap().kind,
            CardKind::Terminal(source.clone())
        );
        app.session.as_mut().unwrap().shutdown();
    }
}

#[test]
fn deferred_pending_presentation_cannot_downgrade_a_changed_or_closed_source() {
    for change in 0..4 {
        let (mut app, card, source) = pending_app(true);
        let other_start = CardId::new();
        app.begin_terminal_start(other_start);
        drop(app.observe_hosted_descriptors(vec![pending_descriptor(&source)]));
        assert_eq!(app.pending_adoptions.len(), 1);
        let mut replacement = source.clone();
        match change {
            0 => {
                replacement.continuity = TerminalContinuity::Live {
                    host_session_id: HostSessionId::new("new-host"),
                    boot_id: BootId::new("pending-test-boot"),
                    host_incarnation: HostIncarnation::new("new-incarnation"),
                }
            }
            1 => {
                replacement
                    .launch
                    .environment
                    .insert("SAVED_HOME".into(), "/new/account".into());
            }
            2 => replacement.predecessor = Some(TerminalId::new()),
            _ => {
                app.card_state_mut(card).unwrap().existence = Existence::Closed {
                    tombstone: crate::model::Tombstone {
                        operation_id: None,
                        closed_at_unix_ms: 1,
                        reason: None,
                    },
                }
            }
        }
        app.card_state_mut(card).unwrap().kind = CardKind::Terminal(replacement.clone());
        app.cards.insert(card, CardRuntime::Completed);
        drop(app.finish_terminal_start(other_start));
        assert!(app.pending_adoptions.is_empty());
        assert_eq!(app.terminal_starts_in_flight(), 0);
        assert_eq!(
            app.card_state(card).unwrap().kind,
            CardKind::Terminal(replacement)
        );
        assert!(
            matches!(app.cards.get(&card), Some(CardRuntime::Completed)),
            "stale availability cannot downgrade newer presentation"
        );
    }
}

#[test]
fn pending_availability_normalizes_only_observational_issued_source_fields() {
    let (mut app, card, source) = pending_app(false);
    let mut pending = pending_descriptor(&source);
    let proof = source.continuity.live_proof().unwrap();
    pending.upgrade_sources[0].continuity = TerminalContinuity::DetachedSameBoot {
        host_session_id: proof.host_session_id,
        boot_id: proof.boot_id,
        host_incarnation: proof.host_incarnation,
    };
    pending.upgrade_sources[0].resume_hint = Some("holder-observation".into());
    drop(app.observe_hosted_descriptors(vec![pending]));
    assert!(matches!(app.cards.get(&card), Some(CardRuntime::Failed(_))));
    assert_eq!(
        app.card_state(card).unwrap().kind,
        CardKind::Terminal(source)
    );
    assert_eq!(app.terminal_starts_in_flight(), 0);
}

#[test]
fn unmatched_or_flattened_pending_inventory_has_no_replacement_authority() {
    let (mut app, card, source) = pending_app(true);
    let mut pending = pending_descriptor(&source);
    pending.upgrade_sources[0]
        .launch
        .unset_environment
        .insert("FOREIGN".into());
    app.cards.insert(card, CardRuntime::Completed);
    drop(app.observe_hosted_descriptors(vec![pending.clone()]));
    assert!(matches!(app.cards.get(&card), Some(CardRuntime::Completed)));
    app.cards
        .insert(card, CardRuntime::Failed("original attach failed".into()));
    drop(app.observe_hosted_terminals(vec![pending.session]));
    assert!(matches!(app.cards.get(&card), Some(CardRuntime::Failed(_))));
    assert_eq!(
        app.card_state(card).unwrap().kind,
        CardKind::Terminal(source)
    );
    assert_eq!(app.durable.cards.len(), 1);
    assert_eq!(app.terminal_starts_in_flight(), 0);
}
