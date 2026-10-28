//! Real published23 ->24 admission. Only typed private fixture data is created;
//! no worker, daemon, provider, portal or native UI is started by these tests.
use super::*;
use crate::{
    agent_execution::Origin,
    automations::{DirectoryIdentity, Occurrence, Proposal, ProposalState},
    model::CardId,
    workspace_model::views::Change,
};
use std::os::unix::fs::PermissionsExt;
use uuid::Uuid;

type Metadata = (i64, String, u64, u64, u64);

fn metadata(db: &Connection) -> Metadata {
    db.query_row(
        "SELECT key,identity,sequence,next_project_ordinal,next_channel_ordinal FROM metadata WHERE key=0",
        [], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?,row.get(4)?)),
    ).unwrap()
}

fn tables(db: &Connection) -> Vec<String> {
    db.prepare("SELECT name FROM sqlite_schema WHERE type='table' AND name NOT GLOB 'sqlite_*' AND name!='metadata' ORDER BY name")
        .unwrap().query_map([], |row| row.get(0)).unwrap()
        .collect::<rusqlite::Result<Vec<_>>>().unwrap()
}

fn count(db: &Connection, table: &str) -> u64 {
    db.query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
        row.get(0)
    })
    .unwrap()
}

fn real_twenty_three() -> (tempfile::TempDir, std::path::PathBuf, Vec<Proposal>, Uuid) {
    // Reuse the existing real API fixture: pins, lifecycle/run outboxes,
    // accepted work, CoreHome, placement, history22 lineage and cleanup rows.
    let (mut source, directory, scope) = history_predecessor_source();
    let definition = source.automations(None, 100).unwrap()[0].definition.clone();
    let mut proposals = Vec::new();
    for number in 0..4 {
        let proposal = source
            .propose_automation(
                Uuid::new_v4(),
                Origin::User,
                definition.clone(),
                definition.clone(),
                DirectoryIdentity {
                    device: 1,
                    inode: 1,
                },
                None,
                1_000 + number,
            )
            .unwrap();
        let proposal = match number {
            2 => source
                .resolve_automation_proposal(&proposal, None, 2_000)
                .unwrap(),
            3 => source
                .resolve_automation_proposal(
                    &proposal,
                    Some(Occurrence {
                        due_ms: 10_000,
                        local: "synthetic future occurrence".into(),
                    }),
                    2_001,
                )
                .unwrap(),
            _ => proposal,
        };
        proposals.push(proposal);
    }
    assert_eq!(
        source
            .automation_proposal_delivery()
            .unwrap()
            .pending_automation_proposal_count,
        2
    );
    let retired = CardId::new();
    source
        .apply(&ModelEdit::SeedView {
            scope: scope.clone(),
            panes: vec![],
            closed: vec![retired],
        })
        .unwrap();
    let mut view = crate::host::model::views::read(&source.db, &scope).unwrap();
    let first_operation = Uuid::new_v4();
    // Real view updates create receipts and cross the existing2048 threshold.
    // The oldest UUID must remain recognizable in the permanent23 archive.
    for index in 0..2_050 {
        let before = view.panes[0].clone();
        let mut pane = before.clone();
        pane.width_fraction = if index % 2 == 0 { 0.4 } else { 0.6 };
        let applied = source
            .apply(&ModelEdit::ChangeView {
                scope: scope.clone(),
                expected_revision: view.revision,
                operation: if index == 0 {
                    first_operation
                } else {
                    Uuid::new_v4()
                },
                change: Change::Update { before, pane },
            })
            .unwrap();
        assert!(applied.changed);
        view = applied
            .model
            .views
            .into_iter()
            .find(|view| view.scope == scope)
            .unwrap();
    }
    assert!(source.view_operation_seen(first_operation).unwrap());
    let path = crate::host::model::automation_proposals::published_fixture_from_rows(&source, 23);
    drop(source);
    let db = Connection::open(&path).unwrap();
    require_open_layout(&db, 23).unwrap();
    assert_eq!(inspect(&db).unwrap().version, 23);
    for table in [
        "terminal_retention_pins",
        "shared_terminal_retention_pins",
        "automation_notifications",
        "automation_run_attention",
        "channel_work_homes",
        "channel_work_placement_epochs",
        "automation_precheck_history_cleanup",
        "retired_view_panes",
        "shared_view_edits",
        "compacted_view_operations",
        "compacted_view_operation_scopes",
    ] {
        assert!(
            count(&db, table) > 0,
            "the genuine23 fixture populates {table}"
        );
    }
    assert_eq!(count(&db, "automation_proposals"), 4);
    assert_eq!(
        db.query_row::<u64, _, _>(
            "SELECT count(*) FROM automation_proposals WHERE pending=1",
            [],
            |row| row.get(0)
        )
        .unwrap(),
        2
    );
    assert_eq!(db.query_row::<u64, _, _>("SELECT count(*) FROM sqlite_schema WHERE name IN ('global_pending_automation_proposals','automation_proposal_notifications','pending_automation_proposal_notifications')", [], |row| row.get(0)).unwrap(), 0);
    assert_eq!(db.query_row::<u64, _, _>("SELECT count(*) FROM pragma_table_info('metadata') WHERE name IN ('automation_proposal_sequence','pending_automation_proposal_count')", [], |row| row.get(0)).unwrap(), 0);
    drop(db);
    (directory, path, proposals, first_operation)
}

#[test]
fn real_twenty_three_upgrade_backfills_pending_once_preserves_all_rows_and_never_replays_notices() {
    let (_directory, path, proposals, archived) = real_twenty_three();
    let before = Connection::open(&path).unwrap();
    let names = tables(&before);
    let rows = history_preserved_rows(&before, &names, 23);
    let original = metadata(&before);
    let layout = inspect(&before).unwrap();
    drop(before);
    for _ in 0..2 {
        let registry = Registry::at(&path).unwrap();
        require_current(&registry.db).unwrap();
        assert_eq!(inspect(&registry.db).unwrap().version, 24);
        assert_eq!(metadata(&registry.db), original);
        assert_eq!(history_preserved_rows(&registry.db, &names, 23), rows);
        let snapshot = registry.automation_proposal_delivery().unwrap();
        assert_eq!(snapshot.identity.to_string(), original.1);
        assert_eq!(snapshot.automation_proposal_sequence, 1);
        assert_eq!(snapshot.pending_automation_proposal_count, 2);
        let mut expected = proposals
            .iter()
            .filter(|proposal| proposal.state == ProposalState::Pending)
            .map(crate::automations::ProposalSummary::from)
            .collect::<Vec<_>>();
        expected.sort_by_key(|proposal| proposal.id);
        assert_eq!(snapshot.pending_automation_proposals, expected);
        for proposal in &proposals {
            assert_eq!(
                registry
                    .automation_proposal(proposal.id, None)
                    .unwrap()
                    .as_ref(),
                Some(proposal)
            );
        }
        assert!(
            registry
                .pending_proposal_notifications(32)
                .unwrap()
                .is_empty()
        );
        assert_eq!(count(&registry.db, "automation_proposal_notifications"), 0);
        assert!(registry.view_operation_seen(archived).unwrap());
        let mut explain = registry.db.prepare("EXPLAIN QUERY PLAN SELECT summary FROM automation_proposals INDEXED BY global_pending_automation_proposals WHERE pending=1 AND id>?1 ORDER BY id LIMIT 64").unwrap();
        let plan = explain
            .query_map([""], |row| row.get::<_, String>(3))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
            .join("\n");
        assert!(
            plan.contains("SEARCH")
                && plan.contains("global_pending_automation_proposals")
                && !plan.contains("TEMP B-TREE"),
            "{plan}"
        );
        assert!(registry.db.is_autocommit());
    }
    // The predecessor was a real23layout, not a currentheader rewrite.
    assert_eq!(layout.version, 23);
}

#[test]
fn failure_after_both_twenty_four_bodies_rolls_back_true_twenty_three_then_retries_cleanly() {
    let _clear = Clear;
    let (_directory, path, _, archived) = real_twenty_three();
    let before = bytes(&path);
    let db = Connection::open(&path).unwrap();
    let inspection = inspect(&db).unwrap();
    let original = metadata(&db);
    let names = tables(&db);
    let rows = history_preserved_rows(&db, &names, 23);
    drop(db);
    FAIL_STEP.with(|failure| failure.set(Some(24)));
    let error = Registry::at(&path)
        .err()
        .expect("real24checkpoint must fail after both bodies");
    FAIL_STEP.with(|failure| failure.set(None));
    assert!(error.contains("failure at 24"), "{error}");
    assert_eq!(bytes(&path), before);
    assert!(!wal(&path).exists());
    let retained = Connection::open(&path).unwrap();
    require_open_layout(&retained, 23).unwrap();
    assert_eq!(inspect(&retained).unwrap(), inspection);
    assert_eq!(metadata(&retained), original);
    assert_eq!(history_preserved_rows(&retained, &names, 23), rows);
    assert!(retained.is_autocommit());
    drop(retained);
    let registry = Registry::at(&path).unwrap();
    require_current(&registry.db).unwrap();
    assert_eq!(metadata(&registry.db), original);
    assert_eq!(history_preserved_rows(&registry.db, &names, 23), rows);
    assert!(registry.view_operation_seen(archived).unwrap());
    assert_eq!(
        registry
            .automation_proposal_delivery()
            .unwrap()
            .automation_proposal_sequence,
        1
    );
    assert_eq!(
        registry
            .automation_proposal_delivery()
            .unwrap()
            .pending_automation_proposal_count,
        2
    );
    assert!(
        registry
            .pending_proposal_notifications(1)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn damaged_twenty_four_layout_refuses_before_writable_admission_or_counter_repair() {
    let _clear = Clear;
    for damage in [
        "DROP INDEX global_pending_automation_proposals;",
        "DROP TABLE automation_proposal_notifications;",
        "DROP INDEX pending_automation_proposal_notifications;",
        "ALTER TABLE metadata RENAME COLUMN automation_proposal_sequence TO changed_clock;",
        "ALTER TABLE metadata RENAME COLUMN pending_automation_proposal_count TO changed_count;",
        "ALTER TABLE automation_proposal_notifications ADD COLUMN unknown_payload TEXT;",
    ] {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("damaged24.sqlite3");
        drop(Registry::at(&path).unwrap());
        let db = Connection::open(&path).unwrap();
        db.execute_batch(damage).unwrap();
        drop(db);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
        let before = bytes(&path);
        assert!(Registry::at(&path).is_err(), "{damage}");
        assert_eq!(bytes(&path), before, "{damage}");
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o640
        );
    }
}

#[test]
fn future_header_refuses_retained_delivery_review_decision_and_notice_without_mutation() {
    let _clear = Clear;
    let (mut registry, _directory, _) = history_predecessor_source();
    let definition = registry.automations(None, 100).unwrap()[0]
        .definition
        .clone();
    let proposal = registry
        .propose_automation(
            Uuid::new_v4(),
            Origin::User,
            definition.clone(),
            definition,
            DirectoryIdentity {
                device: 1,
                inode: 1,
            },
            None,
            100,
        )
        .unwrap();
    let identity = registry.automation_proposal_delivery().unwrap().identity;
    let before_source = registry.pending_proposal_notifications(32).unwrap();
    assert_eq!(before_source.len(), 1);
    let path = registry.path.clone();
    let external = Connection::open(&path).unwrap();
    external.pragma_update(None, "user_version", 25).unwrap();
    drop(external);
    let before = bytes(&path);
    let wal_before = std::fs::read(wal(&path)).ok();
    assert!(registry.automation_proposal_delivery().is_err());
    assert!(
        registry
            .pending_automation_proposals_page(Some(identity), Some(1), None, 64)
            .is_err()
    );
    assert!(
        registry
            .review_automation_proposal(identity, proposal.id)
            .is_err()
    );
    assert!(
        registry
            .resolve_automation_proposal_in_registry(identity, &proposal, None, 999)
            .is_err()
    );
    assert!(registry.pending_proposal_notifications(32).is_err());
    assert!(Registry::at(&path).is_err());
    assert!(registry.db.is_autocommit());
    assert_eq!(count(&registry.db, "automation_proposals"), 1);
    assert_eq!(count(&registry.db, "automation_proposal_notifications"), 1);
    assert_eq!(bytes(&path), before);
    assert_eq!(std::fs::read(wal(&path)).ok(), wal_before);
}
