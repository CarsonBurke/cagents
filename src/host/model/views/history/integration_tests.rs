//! INACTIVE, allocation-dependent integration contracts. The owner must join
//! the allocated opener/reference-layout migration before running this layer.
//! These synthetic Registry fixtures launch no provider or terminal process.
use super::fixtures::{fixture, pane, rows};
use super::*;
use crate::{
    agent::Provider,
    agent_execution::{
        Accepted, Destination, HostedSession, Origin, Spec, State, Submit, Surface, Target, Update,
    },
    host::model::{Registry, schema},
    model::{
        BootId, CardKind, HostIncarnation, HostSessionId, HostSessionProof, LaunchRecipe,
        TerminalContinuity, TerminalExit, TerminalId, TerminalSession,
    },
    workspace_model::{
        ModelEdit,
        views::{Change, Pane},
    },
};
use rusqlite::TransactionBehavior;

fn change(
    registry: &mut Registry,
    scope: &Scope,
    change: Change,
) -> Result<crate::host::model::Applied, String> {
    registry.apply(&ModelEdit::ChangeView {
        scope: scope.clone(),
        expected_revision: registry.view_revision(scope)?,
        operation: uuid::Uuid::new_v4(),
        change,
    })
}

#[test]
fn ten_thousand_closes_bound_recent_json_keep_all_guards_and_preserve_receipts() {
    let (mut registry, directory, scopes) = fixture();
    let scope = &scopes[0];
    let old = pane();
    let tx = registry
        .db
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .unwrap();
    schema::require_current(&tx).unwrap();
    let mut revision = super::super::read(&tx, scope).unwrap().revision;
    for index in 0..10_000 {
        let pane = if index == 0 { old.clone() } else { pane() };
        let id = pane.id;
        for change in [Change::Insert { pane, before: None }, Change::Remove { id }] {
            assert!(
                super::super::apply(
                    &tx,
                    &ModelEdit::ChangeView {
                        scope: scope.clone(),
                        expected_revision: revision,
                        operation: uuid::Uuid::new_v4(),
                        change,
                    }
                )
                .unwrap()
            );
            revision += 1;
        }
    }
    let view = super::super::read(&tx, scope).unwrap();
    assert_eq!(view.retired.len(), RECENT_RETIRED_LIMIT);
    assert!(!view.retired.contains(&old.id));
    assert!(contains(&tx, scope, old.id).unwrap());
    assert_eq!(
        tx.query_row::<u64, _, _>("SELECT count(*) FROM retired_view_panes", [], |row| row
            .get(0))
            .unwrap(),
        10_000
    );
    assert_eq!(
        tx.query_row::<u64, _, _>("SELECT count(*) FROM shared_view_edits", [], |row| row
            .get(0))
            .unwrap(),
        2048,
        "recent receipts are bounded after permanent identity transfer"
    );
    assert_eq!(
        tx.query_row::<u64, _, _>(
            "SELECT count(*) FROM compacted_view_operations",
            [],
            |row| row.get(0)
        )
        .unwrap(),
        20_000 - 2048
    );
    assert!(
        !super::super::apply(
            &tx,
            &ModelEdit::SeedView {
                scope: scope.clone(),
                panes: vec![old.clone()],
                closed: vec![],
            }
        )
        .unwrap()
    );
    assert!(
        !super::super::apply(
            &tx,
            &ModelEdit::ChangeView {
                scope: scope.clone(),
                expected_revision: revision,
                operation: uuid::Uuid::new_v4(),
                change: Change::Insert {
                    pane: old.clone(),
                    before: None
                },
            }
        )
        .unwrap()
    );
    assert_eq!(super::super::read(&tx, scope).unwrap(), view);
    tx.commit().unwrap();
    drop(registry);
    let mut reopened = Registry::at(&directory.path().join("model.sqlite3")).unwrap();
    assert!(contains(&reopened.db, scope, old.id).unwrap());
    assert!(
        !reopened
            .apply(&ModelEdit::SeedView {
                scope: scope.clone(),
                panes: vec![old],
                closed: vec![]
            })
            .unwrap()
            .changed
    );
    assert_eq!(super::super::read(&reopened.db, scope).unwrap(), view);
}

#[test]
fn rotating_a_full_recent_cache_is_a_real_typed_change() {
    let mut view = View::empty(crate::model::ChannelId::new());
    view.retired = (0..RECENT_RETIRED_LIMIT).map(|_| CardId::new()).collect();
    let old = view.retired[0];
    let id = CardId::new();
    assert!(view.apply(&Change::Remove { id }).unwrap());
    assert_eq!(view.retired.len(), RECENT_RETIRED_LIMIT);
    assert!(!view.retired.contains(&old));
    assert_eq!(view.retired.last(), Some(&id));
    assert!(!view.apply(&Change::Remove { id }).unwrap());
}

fn managed_console(end: bool) -> (Registry, tempfile::TempDir, [Scope; 2], Accepted, Pane) {
    let (mut registry, directory, scopes) = fixture();
    let accepted = registry
        .submit_agent(
            &Origin::User,
            &Submit {
                name: None,
                destination: Destination::New(Spec {
                    provider: Provider::Claude,
                    account: None,
                    target: Target {
                        repository: directory.path().into(),
                        directory: directory.path().into(),
                        placement: Some(scopes[0].clone()),
                    },
                    surface: Surface::Chat,
                    model: None,
                    permission_mode: None,
                    reasoning_effort: None,
                }),
                prompt: "Synthetic retirement fixture; no provider launched".into(),
                idempotency_key: "history-fixture".into(),
            },
            1,
        )
        .unwrap();
    let recipe = LaunchRecipe {
        program: "claude".into(),
        arguments: vec![],
        working_directory: directory.path().into(),
        environment: Default::default(),
        unset_environment: Default::default(),
    };
    let proof = HostSessionProof {
        host_session_id: HostSessionId::new("history-host"),
        boot_id: BootId::new("history-boot"),
        host_incarnation: HostIncarnation::new("history-incarnation"),
    };
    let source = HostedSession::Chat(crate::chat::Session {
        native_continuity: Default::default(),
        id: TerminalId::new(),
        launch: crate::chat::Launch {
            provider: Provider::Claude,
            recipe: recipe.clone(),
            session_id: Some(uuid::Uuid::new_v4().to_string()),
        },
        proof: Some(proof.clone()),
        terminal: None,
        owned_codex_listener: None,
    });
    registry
        .bind_agent(accepted.agent.id, None, source.clone())
        .unwrap();
    registry.register_agent_authority(&source).unwrap();
    let started = registry
        .update_agent_run(
            accepted.run.id,
            accepted.run.revision,
            Update::Started {
                provider_key: "synthetic".into(),
            },
            2,
        )
        .unwrap();
    registry
        .update_agent_run(
            accepted.run.id,
            started.revision,
            Update::Finished {
                state: State::Succeeded,
                result: Default::default(),
            },
            3,
        )
        .unwrap();
    let console = TerminalSession {
        id: source.id(),
        launch: recipe,
        transcript_path: None,
        resume_hint: None,
        owned_codex_listener: None,
        recovery_origin: None,
        predecessor: None,
        continuity: TerminalContinuity::Live {
            host_session_id: proof.host_session_id,
            boot_id: proof.boot_id,
            host_incarnation: proof.host_incarnation,
        },
    };
    registry
        .bind_agent_console(accepted.agent.id, &source, console.clone())
        .unwrap();
    let pane = Pane {
        id: CardId::from_uuid(accepted.agent.id.as_uuid()),
        title: "Synthetic console".into(),
        user_title: None,
        pending_terminal_start: false,
        width_fraction: 0.5,
        kind: CardKind::Terminal(console),
        scope: None,
    };
    registry
        .apply(&ModelEdit::SeedView {
            scope: scopes[0].clone(),
            panes: vec![pane.clone()],
            closed: vec![],
        })
        .unwrap();
    if !end {
        return (registry, directory, scopes, accepted, pane);
    }
    let mut ended = pane.clone();
    let CardKind::Terminal(session) = &mut ended.kind else {
        unreachable!()
    };
    session.continuity = TerminalContinuity::RelaunchAvailable {
        recipe: session.launch.clone(),
        last_exit: Some(TerminalExit {
            code: Some(0),
            signal: None,
            occurred_at_unix_ms: 4,
        }),
    };
    change(
        &mut registry,
        &scopes[0],
        Change::Update {
            before: pane,
            pane: ended.clone(),
        },
    )
    .unwrap();
    (registry, directory, scopes, accepted, ended)
}

fn provenance(db: &Connection, id: CardId) -> (String, String) {
    db.query_row(
        "SELECT source,ended FROM shared_view_terminal_sources WHERE pane=?1",
        [id.to_string()],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )
    .unwrap()
}

#[test]
fn unmanaged_ended_transfer_back_and_failure_keep_guards_and_provenance_atomic() {
    let (mut registry, _directory, scopes, accepted, managed) = managed_console(true);
    let mut source: TerminalSession =
        serde_json::from_str(&provenance(&registry.db, managed.id).0).unwrap();
    source.id = TerminalId::new();
    let mut live = managed.clone();
    live.id = CardId::new();
    live.kind = CardKind::Terminal(source);
    registry
        .apply(&ModelEdit::SeedView {
            scope: scopes[0].clone(),
            panes: vec![live.clone()],
            closed: vec![],
        })
        .unwrap();
    let mut ended = live.clone();
    let CardKind::Terminal(session) = &mut ended.kind else {
        unreachable!()
    };
    session.continuity = TerminalContinuity::RelaunchAvailable {
        recipe: session.launch.clone(),
        last_exit: Some(TerminalExit {
            code: Some(0),
            signal: None,
            occurred_at_unix_ms: 6,
        }),
    };
    change(
        &mut registry,
        &scopes[0],
        Change::Update {
            before: live,
            pane: ended.clone(),
        },
    )
    .unwrap();
    assert!(
        super::super::pane_agent(&registry.db, &ended)
            .unwrap()
            .is_none()
    );
    let proof = provenance(&registry.db, ended.id);
    let original_agent = registry.agent(accepted.agent.id).unwrap();
    for (source, target) in [(0, 1), (1, 0)] {
        change(
            &mut registry,
            &scopes[source],
            Change::Transfer {
                id: ended.id,
                destination: scopes[target].clone(),
                before: None,
            },
        )
        .unwrap();
        assert!(contains(&registry.db, &scopes[source], ended.id).unwrap());
        assert!(!contains(&registry.db, &scopes[target], ended.id).unwrap());
        assert_eq!(provenance(&registry.db, ended.id), proof);
        assert_eq!(
            registry.agent(accepted.agent.id).unwrap(),
            original_agent,
            "an unrelated terminal must not move managed agent authority"
        );
    }
    let before = rows(&registry.db, &[]);
    let source_key = key(&scopes[0]).unwrap().replace('\'', "''");
    registry.db.execute_batch(&format!("CREATE TEMP TRIGGER refuse_unmanaged_source_retirement AFTER INSERT ON main.retired_view_panes WHEN new.scope='{source_key}' BEGIN SELECT RAISE(ABORT,'fixture unmanaged guard failure'); END;")).unwrap();
    assert!(
        change(
            &mut registry,
            &scopes[0],
            Change::Transfer {
                id: ended.id,
                destination: scopes[1].clone(),
                before: None
            }
        )
        .unwrap_err()
        .contains("fixture unmanaged guard failure")
    );
    assert_eq!(rows(&registry.db, &[]), before);
}

#[test]
fn live_managed_transfer_back_and_failure_keep_guards_and_placement_atomic() {
    let (mut registry, _directory, scopes, accepted, pane) = managed_console(false);
    let retained_source = registry.agent(accepted.agent.id).unwrap().session;
    for (source, target) in [(0, 1), (1, 0)] {
        change(
            &mut registry,
            &scopes[source],
            Change::Transfer {
                id: pane.id,
                destination: scopes[target].clone(),
                before: None,
            },
        )
        .unwrap();
        assert!(contains(&registry.db, &scopes[source], pane.id).unwrap());
        assert!(!contains(&registry.db, &scopes[target], pane.id).unwrap());
        assert_eq!(
            registry.agent(accepted.agent.id).unwrap().placement,
            scopes[target]
        );
        assert_eq!(
            registry.agent(accepted.agent.id).unwrap().session,
            retained_source
        );
        assert_eq!(
            super::super::read(&registry.db, &scopes[target])
                .unwrap()
                .panes,
            vec![pane.clone()]
        );
    }
    let before = rows(&registry.db, &[]);
    let source_key = key(&scopes[0]).unwrap().replace('\'', "''");
    registry.db.execute_batch(&format!("CREATE TEMP TRIGGER refuse_source_retirement AFTER INSERT ON main.retired_view_panes WHEN new.scope='{source_key}' BEGIN SELECT RAISE(ABORT,'fixture source guard failure'); END;")).unwrap();
    assert!(
        change(
            &mut registry,
            &scopes[0],
            Change::Transfer {
                id: pane.id,
                destination: scopes[1].clone(),
                before: None
            }
        )
        .unwrap_err()
        .contains("fixture source guard failure")
    );
    assert_eq!(
        rows(&registry.db, &[]),
        before,
        "target guard, source guard, agent, CoreHome, placement epochs and retained agent source must all roll back"
    );
}

#[test]
fn both_managed_retirement_paths_keep_permanent_guards_after_recent_eviction_and_reopen() {
    for process in [false, true] {
        let (mut registry, directory, scopes, accepted, pane) = managed_console(true);
        let source = registry.agent(accepted.agent.id).unwrap().session.unwrap();
        if process {
            assert!(registry.retire_agent_process(&source, 5).unwrap());
        } else {
            registry
                .close_agent(&Origin::User, accepted.agent.id)
                .unwrap();
        }
        registry
            .apply(&ModelEdit::SeedView {
                scope: scopes[0].clone(),
                panes: vec![],
                closed: (0..300).map(|_| CardId::new()).collect(),
            })
            .unwrap();
        let view = super::super::read(&registry.db, &scopes[0]).unwrap();
        assert_eq!(view.retired.len(), RECENT_RETIRED_LIMIT);
        assert!(!view.retired.contains(&pane.id));
        assert!(contains(&registry.db, &scopes[0], pane.id).unwrap());
        assert!(registry.agent(accepted.agent.id).unwrap().closed);
        drop(registry);
        let mut reopened = Registry::at(&directory.path().join("model.sqlite3")).unwrap();
        let before = reopened.model().unwrap();
        assert!(
            !reopened
                .apply(&ModelEdit::SeedView {
                    scope: scopes[0].clone(),
                    panes: vec![pane],
                    closed: vec![]
                })
                .unwrap()
                .changed
        );
        assert_eq!(reopened.model().unwrap(), before);
    }
}

#[test]
fn both_managed_retirement_failures_roll_back_guard_authority_agent_run_and_provenance() {
    for process in [false, true] {
        let (mut registry, _directory, scopes, accepted, pane) = managed_console(true);
        let source = registry.agent(accepted.agent.id).unwrap().session.unwrap();
        let before = rows(&registry.db, &[]);
        registry.db.execute_batch("CREATE TEMP TRIGGER refuse_closed_agent AFTER UPDATE OF closed ON main.agents BEGIN SELECT RAISE(ABORT,'fixture after retirement writes'); END;").unwrap();
        let result = if process {
            registry.retire_agent_process(&source, 5).map(drop)
        } else {
            registry
                .close_agent(&Origin::User, accepted.agent.id)
                .map(drop)
        };
        assert!(
            result
                .unwrap_err()
                .contains("fixture after retirement writes")
        );
        assert!(!contains(&registry.db, &scopes[0], pane.id).unwrap());
        assert_eq!(rows(&registry.db, &[]), before);
    }
}

#[test]
fn channel_and_project_deletion_cascade_guards_but_retain_deleted_core_homes() {
    let (mut registry, directory, scopes) = fixture();
    let id = CardId::new();
    for scope in &scopes {
        registry
            .apply(&ModelEdit::SeedView {
                scope: scope.clone(),
                panes: vec![],
                closed: vec![id],
            })
            .unwrap();
        assert!(contains(&registry.db, scope, id).unwrap());
    }
    let Scope::Channel(first) = scopes[0] else {
        unreachable!()
    };
    registry
        .apply(&ModelEdit::DeleteChannel { id: first })
        .unwrap();
    assert!(!contains(&registry.db, &scopes[0], id).unwrap());
    assert!(contains(&registry.db, &scopes[1], id).unwrap());
    registry
        .apply(&ModelEdit::CloseProject {
            repository: directory.path().into(),
        })
        .unwrap();
    assert!(!contains(&registry.db, &scopes[1], id).unwrap());
    assert_eq!(
        registry
            .db
            .query_row::<u64, _, _>(
                "SELECT count(*) FROM channel_work_homes WHERE lifecycle='deleted'",
                [],
                |row| row.get(0)
            )
            .unwrap(),
        2
    );
    assert_eq!(
        registry
            .db
            .query_row::<u64, _, _>("SELECT count(*) FROM retired_view_panes", [], |row| row
                .get(0))
            .unwrap(),
        0
    );
}

#[test]
fn receipts_survive_more_than_2048_operations_and_prevent_rebased_lost_reply_replay() {
    let (mut registry, _dir, scopes) = fixture();
    let scope = &scopes[0];
    let original = pane();
    registry
        .apply(&ModelEdit::SeedView {
            scope: scope.clone(),
            panes: vec![original.clone()],
            closed: vec![],
        })
        .unwrap();
    let mut old_value = original.clone();
    old_value.user_title = Some("Old request".into());
    old_value.width_fraction = 0.75;
    let operation = uuid::Uuid::new_v4();
    let original_revision = registry.view_revision(scope).unwrap();
    let mut retry = ModelEdit::ChangeView {
        scope: scope.clone(),
        expected_revision: original_revision,
        operation,
        change: Change::Update {
            before: original,
            pane: old_value.clone(),
        },
    };
    registry.apply(&retry).unwrap(); // Its successful response is deliberately lost.
    let mut latest = old_value.clone();
    latest.user_title = Some("Later user choice".into());
    latest.width_fraction = 0.25;
    registry
        .apply(&ModelEdit::ChangeView {
            scope: scope.clone(),
            expected_revision: registry.view_revision(scope).unwrap(),
            operation: uuid::Uuid::new_v4(),
            change: Change::Update {
                before: old_value,
                pane: latest.clone(),
            },
        })
        .unwrap();
    let tx = registry
        .db
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .unwrap();
    schema::require_current(&tx).unwrap();
    let revision = super::super::read(&tx, scope).unwrap().revision;
    for _ in 0..2050 {
        super::super::apply(
            &tx,
            &ModelEdit::ChangeView {
                scope: scope.clone(),
                expected_revision: revision,
                operation: uuid::Uuid::new_v4(),
                change: Change::Move {
                    id: latest.id,
                    before: None,
                },
            },
        )
        .unwrap();
    }
    assert_eq!(
        tx.query_row::<u64, _, _>("SELECT count(*) FROM shared_view_edits", [], |row| row
            .get(0))
            .unwrap(),
        2048
    );
    assert_eq!(
        tx.query_row::<u64, _, _>(
            "SELECT count(*) FROM shared_view_edits WHERE operation=?1",
            [operation.to_string()],
            |row| row.get(0)
        )
        .unwrap(),
        0
    );
    assert!(super::super::operation_seen(&tx, operation).unwrap());
    assert!(!super::super::apply(&tx, &retry).unwrap());
    let ModelEdit::ChangeView {
        expected_revision, ..
    } = &mut retry
    else {
        unreachable!()
    };
    *expected_revision = revision; // Same rebase as current workspace_model_sync::send.
    assert!(!super::super::apply(&tx, &retry).unwrap());
    assert_eq!(
        super::super::read(&tx, scope).unwrap().panes,
        vec![latest.clone()]
    );
    // Causal negative control: deleting just this archived identity recreates the unsafe
    // compaction/rebase behavior. This is a HOST contract witness, not an App run.
    tx.execute(
        "DELETE FROM compacted_view_operations WHERE operation=?1",
        [operation.as_bytes().as_slice()],
    )
    .unwrap();
    let ModelEdit::ChangeView {
        expected_revision, ..
    } = &mut retry
    else {
        unreachable!()
    };
    *expected_revision = original_revision;
    assert!(
        super::super::apply(&tx, &retry)
            .unwrap_err()
            .contains("revision conflict")
    );
    let current_revision = super::super::read(&tx, scope).unwrap().revision;
    let ModelEdit::ChangeView {
        expected_revision, ..
    } = &mut retry
    else {
        unreachable!()
    };
    *expected_revision = current_revision;
    assert!(super::super::apply(&tx, &retry).unwrap());
    let replayed = super::super::read(&tx, scope).unwrap();
    assert_eq!(replayed.panes[0].user_title.as_deref(), Some("Old request"));
    assert_eq!(replayed.panes[0].width_fraction, 0.75);
    tx.rollback().unwrap();
}
