//! Allocation-dependent contracts. This proposal has no published schema slot.
use super::super::fixtures::{fixture, pane, rows};
use super::*;
use crate::{
    host::{
        client::process_request,
        model::Registry,
        state::{HostLock, HostState, SharedHost},
    },
    model::{BootId, HostIncarnation, HostSessionId, HostSessionProof},
    terminal_ipc::{ClientRequest, ProtocolError, ProtocolErrorCode, ServerResponse},
    workspace_model::{
        ModelEdit, WorkspaceModel,
        views::{Change, Scope},
    },
};
use std::sync::Arc;

fn make_host(registry: Registry) -> SharedHost {
    let mut state = HostState::new(HostSessionProof {
        host_session_id: HostSessionId::new("operation-host"),
        boot_id: BootId::new("operation-boot"),
        host_incarnation: HostIncarnation::new("operation-incarnation"),
    });
    state.install_model(registry);
    state.register_client(1);
    Arc::new(HostLock::new(state))
}

fn request(host: &SharedHost, edit: Option<ModelEdit>) -> Result<WorkspaceModel, ProtocolError> {
    let request = edit.map_or(ClientRequest::ReadWorkspaceModel, |edit| {
        ClientRequest::EditWorkspaceModel { edit }
    });
    let (response, _, closing) = process_request(host, 1, request)?;
    assert!(!closing);
    let ServerResponse::WorkspaceModel { model } = response else {
        panic!("wrong response")
    };
    Ok(model)
}

fn revision(model: &WorkspaceModel, scope: &Scope) -> u64 {
    model
        .views
        .iter()
        .find(|view| &view.scope == scope)
        .unwrap()
        .revision
}

fn edit(host: &SharedHost, scope: &Scope, change: Change, operation: Uuid) -> ModelEdit {
    ModelEdit::ChangeView {
        scope: scope.clone(),
        expected_revision: revision(&request(host, None).unwrap(), scope),
        operation,
        change,
    }
}

// The presentation-only branch of actual app/workspace_model_sync.rs::send:
// same UUID/change, Read after ViewConflict, rewrite ONLY expected_revision,
// at most8 retries. Source-changing updates are deliberately not modeled here.
fn current_presentation_send_policy(
    host: &SharedHost,
    mut retry: ModelEdit,
) -> Result<(WorkspaceModel, usize), ProtocolError> {
    let mut conflicts = 0;
    loop {
        match request(host, Some(retry.clone())) {
            Ok(model) => return Ok((model, conflicts)),
            Err(error) if error.code == ProtocolErrorCode::ViewConflict && conflicts < 8 => {
                conflicts += 1;
                let model = request(host, None)?;
                let ModelEdit::ChangeView {
                    scope,
                    expected_revision,
                    ..
                } = &mut retry
                else {
                    return Err(error);
                };
                *expected_revision = revision(&model, scope);
            }
            Err(error) => return Err(error),
        }
    }
}

#[test]
fn actual_host_lost_reply_compaction_and_current_client_rebase_policy_never_reapply_old_edits() {
    for moving in [false, true] {
        let (registry, directory, scopes) = fixture();
        let scope = &scopes[0];
        let mut cards = vec![pane(), pane(), pane()];
        let host = make_host(registry);
        request(
            &host,
            Some(ModelEdit::SeedView {
                scope: scope.clone(),
                panes: cards.clone(),
                closed: vec![],
            }),
        )
        .unwrap();
        let operation = Uuid::new_v4();
        let old = if moving {
            edit(
                &host,
                scope,
                Change::Move {
                    id: cards[2].id,
                    before: Some(cards[0].id),
                },
                operation,
            )
        } else {
            let before = cards[0].clone();
            cards[0].user_title = Some("Old title".into());
            cards[0].width_fraction = 0.75;
            edit(
                &host,
                scope,
                Change::Update {
                    before,
                    pane: cards[0].clone(),
                },
                operation,
            )
        };
        request(&host, Some(old.clone())).unwrap(); // Deliberately lose the successful reply.
        let later = if moving {
            edit(
                &host,
                scope,
                Change::Move {
                    id: cards[0].id,
                    before: Some(cards[2].id),
                },
                Uuid::new_v4(),
            )
        } else {
            let before = cards[0].clone();
            cards[0].user_title = Some("Later title".into());
            cards[0].width_fraction = 0.25;
            edit(
                &host,
                scope,
                Change::Update {
                    before,
                    pane: cards[0].clone(),
                },
                Uuid::new_v4(),
            )
        };
        request(&host, Some(later)).unwrap();
        for _ in 0..2050 {
            let noop = edit(
                &host,
                scope,
                Change::Move {
                    id: cards[0].id,
                    before: Some(cards[0].id),
                },
                Uuid::new_v4(),
            );
            request(&host, Some(noop)).unwrap();
        }
        let observer = Registry::at(&directory.path().join("model.sqlite3")).unwrap();
        assert!(
            observer.view_operation_seen(operation).unwrap(),
            "same lookup used by actual Host preflight"
        );
        assert_eq!(
            observer
                .db
                .query_row::<u64, _, _>(
                    "SELECT count(*) FROM shared_view_edits WHERE operation=?1",
                    [operation.to_string()],
                    |row| row.get(0)
                )
                .unwrap(),
            0
        );
        assert_eq!(
            observer
                .db
                .query_row::<u64, _, _>("SELECT count(*) FROM shared_view_edits", [], |row| row
                    .get(0))
                .unwrap(),
            RECENT_OPERATIONS as u64
        );
        let before = rows(&observer.db, &[]);
        let latest = request(&host, None).unwrap();
        let (response, conflicts) = current_presentation_send_policy(&host, old.clone()).unwrap();
        assert_eq!(
            conflicts, 0,
            "archived receipt must bypass preflight rebase as well as transactional reapplication"
        );
        assert_eq!(response, latest);
        assert_eq!(
            rows(&observer.db, &[]),
            before,
            "no revision, CoreHome, placement or archive changes on replay"
        );
        drop(host);
        drop(observer);
        let reopened = Registry::at(&directory.path().join("model.sqlite3")).unwrap();
        assert!(reopened.view_operation_seen(operation).unwrap());
        let restarted = make_host(reopened);
        assert_eq!(
            current_presentation_send_policy(&restarted, old.clone()).unwrap(),
            (latest.clone(), 0)
        );
        // Causal negative control: only removing permanent identity permits the
        // ACTUAL Host ViewConflict→sameUUID rebase policy to overwrite later work.
        let observer = Registry::at(&directory.path().join("model.sqlite3")).unwrap();
        observer
            .db
            .execute(
                "DELETE FROM compacted_view_operations WHERE operation=?1",
                [operation.as_bytes().as_slice()],
            )
            .unwrap();
        let (overwritten, conflicts) = current_presentation_send_policy(&restarted, old).unwrap();
        assert_eq!(conflicts, 1);
        assert_ne!(overwritten, latest);
        let view = overwritten
            .views
            .iter()
            .find(|view| &view.scope == scope)
            .unwrap();
        if moving {
            assert_eq!(
                view.panes.iter().map(|pane| pane.id).collect::<Vec<_>>(),
                vec![cards[2].id, cards[0].id, cards[1].id]
            );
        } else {
            assert_eq!(view.panes[0].user_title.as_deref(), Some("Old title"));
            assert_eq!(view.panes[0].width_fraction, 0.75);
        }
    }
}

#[test]
fn recent_bound_binary_identity_index_cross_scope_noop_and_scope_lifetime_are_explicit() {
    let (mut registry, _directory, scopes) = fixture();
    let cards = vec![pane(), pane()];
    registry
        .apply(&ModelEdit::SeedView {
            scope: scopes[0].clone(),
            panes: cards.clone(),
            closed: vec![],
        })
        .unwrap();
    let operation = Uuid::new_v4();
    let old = ModelEdit::ChangeView {
        scope: scopes[0].clone(),
        expected_revision: registry.view_revision(&scopes[0]).unwrap(),
        operation,
        change: Change::Move {
            id: cards[1].id,
            before: Some(cards[0].id),
        },
    };
    registry.apply(&old).unwrap();
    let tx = registry.db.transaction().unwrap();
    let revision = super::super::super::read(&tx, &scopes[0]).unwrap().revision;
    for _ in 0..2050 {
        super::super::super::apply(
            &tx,
            &ModelEdit::ChangeView {
                scope: scopes[0].clone(),
                expected_revision: revision,
                operation: Uuid::new_v4(),
                change: Change::Move {
                    id: cards[0].id,
                    before: Some(cards[0].id),
                },
            },
        )
        .unwrap();
    }
    tx.commit().unwrap();
    // The target channel exists but has no registered view: a duplicate must
    // not even create an unsequenced scope row before its UUID check.
    registry
        .db
        .execute(
            "DELETE FROM shared_view_scopes WHERE scope=?1",
            [super::super::key(&scopes[1]).unwrap()],
        )
        .unwrap();
    let before = rows(&registry.db, &[]);
    // A reused UUID with another scope or another change is still an old
    // operation, never new authority. This matches current recent-ID semantics.
    let reused = ModelEdit::ChangeView {
        scope: scopes[1].clone(),
        expected_revision: registry.view_revision(&scopes[1]).unwrap(),
        operation,
        change: Change::Insert {
            pane: pane(),
            before: None,
        },
    };
    assert!(!registry.apply(&reused).unwrap().changed);
    assert_eq!(rows(&registry.db, &[]), before);
    assert_eq!(registry.db.query_row::<u64, _, _>("SELECT count(*) FROM compacted_view_operations WHERE length(operation)=16 AND typeof(scope)='integer'", [], |row| row.get(0)).unwrap(), 3);
    assert_eq!(
        registry
            .db
            .query_row::<u64, _, _>(
                "SELECT count(*) FROM compacted_view_operation_scopes",
                [],
                |row| row.get(0)
            )
            .unwrap(),
        1
    );
    let plan = registry
        .db
        .prepare("EXPLAIN QUERY PLAN SELECT 1 FROM compacted_view_operations WHERE operation=?1")
        .unwrap()
        .query_map([operation.as_bytes().as_slice()], |row| {
            row.get::<_, String>(3)
        })
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap()
        .join("; ");
    assert!(
        plan.contains("SEARCH") && plan.contains("PRIMARY KEY"),
        "{plan}"
    );
    let Scope::Channel(channel) = scopes[0] else {
        unreachable!()
    };
    registry
        .apply(&ModelEdit::DeleteChannel { id: channel })
        .unwrap();
    assert!(!registry.view_operation_seen(operation).unwrap());
    assert_eq!(
        registry
            .db
            .query_row::<u64, _, _>(
                "SELECT count(*) FROM compacted_view_operations",
                [],
                |row| row.get(0)
            )
            .unwrap(),
        0
    );
    assert_eq!(
        registry
            .db
            .query_row::<u64, _, _>(
                "SELECT count(*) FROM compacted_view_operation_scopes",
                [],
                |row| row.get(0)
            )
            .unwrap(),
        0
    );
}

#[test]
fn archive_or_delete_failure_rolls_back_transfer_and_all_replay_state_together() {
    for deleting in [false, true] {
        let (mut registry, _directory, scopes) = fixture();
        let card = pane();
        registry
            .apply(&ModelEdit::SeedView {
                scope: scopes[0].clone(),
                panes: vec![card.clone()],
                closed: vec![],
            })
            .unwrap();
        registry
            .apply(&ModelEdit::SeedView {
                scope: scopes[1].clone(),
                panes: vec![],
                closed: vec![card.id],
            })
            .unwrap();
        let tx = registry.db.transaction().unwrap();
        let revision = super::super::super::read(&tx, &scopes[0]).unwrap().revision;
        for _ in 0..RECENT_OPERATIONS {
            super::super::super::apply(
                &tx,
                &ModelEdit::ChangeView {
                    scope: scopes[0].clone(),
                    expected_revision: revision,
                    operation: Uuid::new_v4(),
                    change: Change::Move {
                        id: card.id,
                        before: Some(card.id),
                    },
                },
            )
            .unwrap();
        }
        tx.commit().unwrap();
        let before = rows(&registry.db, &[]);
        let trigger = if deleting {
            "CREATE TEMP TRIGGER reject_compaction AFTER DELETE ON main.shared_view_edits BEGIN SELECT RAISE(ABORT,'fixture after receipt deletion'); END;"
        } else {
            "CREATE TEMP TRIGGER reject_compaction AFTER INSERT ON main.compacted_view_operations BEGIN SELECT RAISE(ABORT,'fixture after archive insertion'); END;"
        };
        registry.db.execute_batch(trigger).unwrap();
        let failed = Uuid::new_v4();
        let error = registry
            .apply(&ModelEdit::ChangeView {
                scope: scopes[0].clone(),
                expected_revision: registry.view_revision(&scopes[0]).unwrap(),
                operation: failed,
                change: Change::Transfer {
                    id: card.id,
                    destination: scopes[1].clone(),
                    before: None,
                },
            })
            .unwrap_err();
        assert!(error.contains(if deleting {
            "fixture after receipt deletion"
        } else {
            "fixture after archive insertion"
        }));
        assert!(!registry.view_operation_seen(failed).unwrap());
        assert_eq!(
            rows(&registry.db, &[]),
            before,
            "receipt, archive, source/target tombstones, pane ownership, CoreHome and placement must roll back as one commit"
        );
    }
}

#[test]
fn invalid_legacy_operation_uuid_refuses_compaction_without_losing_any_rows() {
    let (mut registry, _directory, scopes) = fixture();
    let scope = &scopes[0];
    // Imported legacy rows are allowed by the old TEXT schema. Fail closed,
    // rather than deleting identities that cannot be represented as UUID bytes.
    for index in 0..2050 {
        let operation = if index == 1 {
            "not-a-uuid".into()
        } else {
            Uuid::new_v4().to_string()
        };
        registry
            .db
            .execute(
                "INSERT INTO shared_view_edits(operation,scope) VALUES(?1,?2)",
                params![operation, super::super::key(scope).unwrap()],
            )
            .unwrap();
    }
    let before = rows(&registry.db, &[]);
    let tx = registry.db.transaction().unwrap();
    assert!(
        compact(&tx)
            .unwrap_err()
            .contains("invalid workspace operation UUID")
    );
    assert_eq!(
        tx.query_row::<u64, _, _>(
            "SELECT count(*) FROM compacted_view_operations",
            [],
            |row| row.get(0)
        )
        .unwrap(),
        1,
        "the first valid identity was staged before the malformed second identity"
    );
    tx.rollback().unwrap();
    assert_eq!(rows(&registry.db, &[]), before);
}

#[test]
fn batched_legacy_compaction_keeps_exact_newest_receipts_and_every_older_identity() {
    let (mut registry, _directory, scopes) = fixture();
    let operations = (0..6000).map(|_| Uuid::new_v4()).collect::<Vec<_>>();
    let tx = registry.db.transaction().unwrap();
    for (index, operation) in operations.iter().enumerate() {
        tx.execute(
            "INSERT INTO shared_view_edits(operation,scope) VALUES(?1,?2)",
            params![
                operation.to_string(),
                super::super::key(&scopes[index % 2]).unwrap()
            ],
        )
        .unwrap();
    }
    let unrelated = rows(
        &tx,
        &[
            "shared_view_edits",
            "compacted_view_operations",
            "compacted_view_operation_scopes",
        ],
    );
    migrate(&tx).unwrap();
    let recent = tx
        .prepare("SELECT operation FROM shared_view_edits ORDER BY rowid")
        .unwrap()
        .query_map([], |row| row.get::<_, String>(0))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    assert_eq!(
        recent,
        operations[operations.len() - RECENT_OPERATIONS..]
            .iter()
            .map(Uuid::to_string)
            .collect::<Vec<_>>()
    );
    assert_eq!(
        tx.query_row::<u64, _, _>(
            "SELECT count(*) FROM compacted_view_operations",
            [],
            |row| row.get(0)
        )
        .unwrap(),
        (operations.len() - RECENT_OPERATIONS) as u64
    );
    assert_eq!(
        tx.query_row::<u64, _, _>(
            "SELECT count(*) FROM compacted_view_operation_scopes",
            [],
            |row| row.get(0)
        )
        .unwrap(),
        2
    );
    for operation in operations {
        assert!(seen(&tx, operation).unwrap());
    }
    assert_eq!(
        rows(
            &tx,
            &[
                "shared_view_edits",
                "compacted_view_operations",
                "compacted_view_operation_scopes"
            ]
        ),
        unrelated
    );
    let before = rows(&tx, &[]);
    migrate(&tx).unwrap();
    assert_eq!(rows(&tx, &[]), before);
    tx.rollback().unwrap();
}
