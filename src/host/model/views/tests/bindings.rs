use super::*;
use crate::{
    model::{HostSessionProof, LaunchRecipe, TerminalContinuity, TerminalId, TerminalSession},
    workspace_model::views::{CapturedTerminalSource, TerminalRemovalBasis},
};

fn source() -> TerminalSession {
    let launch = LaunchRecipe::shell("/repo".into()).unwrap();
    TerminalSession {
        id: TerminalId::new(),
        launch: launch.clone(),
        transcript_path: None,
        resume_hint: None,
        owned_codex_listener: None,
        recovery_origin: None,
        predecessor: None,
        continuity: TerminalContinuity::RelaunchAvailable {
            recipe: launch,
            last_exit: None,
        },
    }
}
fn worker() -> TerminalRemovalBasis {
    TerminalRemovalBasis::Worker {
        instance: Uuid::new_v4(),
        owner: HostSessionProof {
            host_session_id: "fixture-host".into(),
            boot_id: "fixture-boot".into(),
            host_incarnation: "fixture-daemon".into(),
        },
    }
}
fn capture(registry: &Registry, channel: ChannelId) -> CapturedTerminalSource {
    let model = registry.model().unwrap();
    let view = model
        .views
        .iter()
        .find(|v| v.scope == Scope::Channel(channel))
        .unwrap();
    CapturedTerminalSource {
        registry: model.identity,
        repository: "/repo".into(),
        scope: view.scope.clone(),
        view_revision: view.revision,
        model_sequence: model.sequence,
        before: view.panes[0].clone(),
    }
}
fn publish(registry: &mut Registry, source: &TerminalSession, worker: &TerminalRemovalBasis) {
    registry
        .source_admission()
        .unwrap()
        .publish(source, worker, None, None)
        .unwrap();
}

#[test]
fn identical_publication_fences_cold_close_after_retirement_and_reopen() {
    let (mut registry, directory, channel, _) = setup();
    let source = source();
    let mut pane = pane();
    pane.kind = CardKind::Terminal(source.clone());
    seed(&mut registry, channel, vec![pane]);
    let original = capture(&registry, channel);
    registry
        .source_admission()
        .unwrap()
        .validate(&original)
        .unwrap();
    publish(&mut registry, &source, &worker());
    // Byte-identical source is still another worker. A later absence or an
    // evicted resident cache must not erase the durable publication edge.
    assert!(
        registry
            .source_admission()
            .unwrap()
            .validate(&original)
            .is_err()
    );
    drop(registry);
    let mut registry = Registry::at(&directory.path().join("model.sqlite3")).unwrap();
    assert!(
        registry
            .source_admission()
            .unwrap()
            .validate(&original)
            .is_err()
    );
    let fresh = capture(&registry, channel);
    registry
        .source_admission()
        .unwrap()
        .validate(&fresh)
        .unwrap();
}

#[test]
fn stale_exit_cannot_overwrite_an_identical_replacement_or_consume_a_receipt() {
    let (mut registry, _, channel, _) = setup();
    let source = source();
    let mut pane = pane();
    pane.kind = CardKind::Terminal(source.clone());
    seed(&mut registry, channel, vec![pane]);
    let a = worker();
    publish(&mut registry, &source, &a);
    let original = capture(&registry, channel).before;
    let b = worker();
    publish(&mut registry, &source, &b);
    let mut ended = original.clone();
    if let CardKind::Terminal(s) = &mut ended.kind {
        s.continuity = TerminalContinuity::Exited {
            last_exit: crate::model::TerminalExit {
                code: Some(0),
                signal: None,
                occurred_at_unix_ms: 1,
            },
        };
    }
    let stale = edit(
        &mut registry,
        channel,
        Change::UpdateTerminalSource {
            before: original,
            pane: ended,
            worker: a,
        },
    );
    let before = registry.model().unwrap();
    assert!(registry.apply(&stale).is_err());
    assert_eq!(registry.model().unwrap(), before);
    let ModelEdit::ChangeView { operation, .. } = stale else {
        unreachable!()
    };
    assert!(!registry.view_operation_seen(operation).unwrap());
}

#[test]
fn natural_completion_and_presentation_preserve_the_original_close_generation() {
    let (mut registry, _, channel, _) = setup();
    let source = source();
    let mut pane = pane();
    pane.kind = CardKind::Terminal(source.clone());
    seed(&mut registry, channel, vec![pane]);
    let a = worker();
    publish(&mut registry, &source, &a);
    let original = capture(&registry, channel);
    let mut ended = original.before.clone();
    if let CardKind::Terminal(s) = &mut ended.kind {
        s.continuity = TerminalContinuity::Exited {
            last_exit: crate::model::TerminalExit {
                code: Some(0),
                signal: None,
                occurred_at_unix_ms: 1,
            },
        };
    }
    let update = edit(
        &mut registry,
        channel,
        Change::UpdateTerminalSource {
            before: original.before.clone(),
            pane: ended.clone(),
            worker: a,
        },
    );
    registry.apply(&update).unwrap();
    let mut named = ended.clone();
    named.title = "Completed fixture".into();
    let update = edit(
        &mut registry,
        channel,
        Change::Update {
            before: ended,
            pane: named,
        },
    );
    registry.apply(&update).unwrap();
    registry
        .source_admission()
        .unwrap()
        .validate(&original)
        .unwrap();
}

#[test]
fn moving_a_source_invalidates_old_cleanup_context() {
    let (mut registry, _, channel, target) = setup();
    let mut pane = pane();
    pane.kind = CardKind::Terminal(source());
    seed(&mut registry, channel, vec![pane]);
    let original = capture(&registry, channel);
    let transfer = edit(
        &mut registry,
        channel,
        Change::Transfer {
            id: original.before.id,
            destination: Scope::Channel(target),
            before: None,
        },
    );
    registry.apply(&transfer).unwrap();
    assert!(
        registry
            .source_admission()
            .unwrap()
            .validate(&original)
            .is_err()
    );
}

#[test]
fn cold_conversion_never_reuses_a_captured_generation_watermark() {
    let (mut registry, directory, channel, _) = setup();
    let mut pane = pane();
    pane.kind = CardKind::Terminal(source());
    seed(&mut registry, channel, vec![pane]);
    let mut old = capture(&registry, channel);
    // Model metadata can outrun a scope snapshot; an old high revision alone
    // must not authorize a newly initialized generation after conversion.
    old.view_revision = u64::MAX;
    drop(registry);
    let path = directory.path().join("model.sqlite3");
    crate::host::model::schema::downgrade(&path, 24).unwrap();
    let mut registry = Registry::at(&path).unwrap();
    assert!(registry.source_admission().unwrap().validate(&old).is_err());
    let fresh = capture(&registry, channel);
    registry
        .source_admission()
        .unwrap()
        .validate(&fresh)
        .unwrap();
}

#[test]
fn publication_cannot_overwrite_a_source_changed_while_the_worker_started() {
    let (mut registry, _, channel, _) = setup();
    let a = source();
    let mut pane = pane();
    pane.kind = CardKind::Terminal(a.clone());
    seed(&mut registry, channel, vec![pane.clone()]);
    let original = registry.terminal_publication_source(a.id).unwrap();
    let mut replacement = pane.clone();
    if let CardKind::Terminal(s) = &mut replacement.kind {
        s.transcript_path = Some("/repo/another-transcript".into());
    }
    let update = edit(
        &mut registry,
        channel,
        Change::Update {
            before: pane,
            pane: replacement,
        },
    );
    registry.apply(&update).unwrap();
    let before = registry.model().unwrap();
    assert!(
        registry
            .source_admission()
            .unwrap()
            .publish(&a, &worker(), None, original)
            .is_err()
    );
    assert_eq!(registry.model().unwrap(), before);
    // An unplaced start cannot claim an independently seeded source either.
    assert!(
        registry
            .source_admission()
            .unwrap()
            .publish(&a, &worker(), None, None)
            .is_err()
    );
    assert_eq!(registry.model().unwrap(), before);
}

#[test]
fn a_rebased_removal_cannot_borrow_the_replacements_absence_stamp() {
    let (mut registry, _, channel, _) = setup();
    let a = source();
    let mut pane = pane();
    pane.kind = CardKind::Terminal(a.clone());
    seed(&mut registry, channel, vec![pane]);
    let old = capture(&registry, channel);
    let b = worker();
    publish(&mut registry, &a, &b);
    let operation = Uuid::new_v4();
    let TerminalRemovalBasis::Worker { owner, instance } = b else {
        unreachable!()
    };
    let b = TerminalRemovalBasis::Absent {
        owner,
        latest_worker: Some(instance),
        history_epoch: 0,
    };
    let removal = ModelEdit::ChangeView {
        scope: old.scope.clone(),
        expected_revision: registry.view_revision(&old.scope).unwrap(),
        operation,
        change: Change::RemoveSource {
            registry: old.registry,
            repository: old.repository.clone(),
            before: old.before.clone(),
            terminal: Some(b),
            capture: Some(old),
        },
    };
    let before = registry.model().unwrap();
    assert!(registry.apply(&removal).is_err());
    assert_eq!(registry.model().unwrap(), before);
    assert!(!registry.view_operation_seen(operation).unwrap());
}
