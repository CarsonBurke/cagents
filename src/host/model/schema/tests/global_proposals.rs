//! Real published22 -> 22 admission. Only typed private fixture data is created;
//! no worker, daemon, provider, portal or native UI is started by these tests.
use super::*;
use crate::{
    agent_execution::Origin,
    automations::{DirectoryIdentity, Occurrence, Proposal, ProposalState},
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

fn real_twenty_two() -> (tempfile::TempDir, std::path::PathBuf, Vec<Proposal>) {
    // Reuse the existing real API fixture: pins, lifecycle/run outboxes,
    // accepted work, CoreHome, placement, history22 lineage and cleanup rows.
    let (mut source, directory, scope) = history_predecessor_source();
    let id = source.automations(None, 100).unwrap()[0].id;
    let definition = source.automation(id).unwrap().definition;
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
    let path = crate::host::model::automation_proposals::published_fixture_from_rows(&source, 22);
    drop(source);
    let db = Connection::open(&path).unwrap();
    require_open_layout(&db, 22).unwrap();
    assert_eq!(inspect(&db).unwrap().version, 22);
    for table in [
        "terminal_retention_pins",
        "shared_terminal_retention_pins",
        "automation_notifications",
        "automation_run_attention",
        "channel_work_homes",
        "channel_work_placement_epochs",
        "automation_precheck_history_cleanup",
    ] {
        assert!(
            count(&db, table) > 0,
            "the genuine22 fixture populates {table}"
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
    (directory, path, proposals)
}

#[test]
fn real_twenty_two_upgrade_backfills_pending_once_preserves_all_rows_and_never_replays_notices() {
    let (_directory, path, proposals) = real_twenty_two();
    let before = Connection::open(&path).unwrap();
    let names = tables(&before);
    let rows = history_preserved_rows(&before, &names, 22);
    let original = metadata(&before);
    let layout = inspect(&before).unwrap();
    drop(before);
    for _ in 0..2 {
        let registry = Registry::at(&path).unwrap();
        require_current(&registry.db).unwrap();
        assert_eq!(inspect(&registry.db).unwrap().version, 23);
        assert_eq!(metadata(&registry.db), original);
        assert_eq!(history_preserved_rows(&registry.db, &names, 22), rows);
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
    assert_eq!(layout.version, 22);
}

#[test]
fn failure_after_both_twenty_three_bodies_rolls_back_true_twenty_two_then_retries_cleanly() {
    let _clear = Clear;
    let (_directory, path, _) = real_twenty_two();
    let before = bytes(&path);
    let db = Connection::open(&path).unwrap();
    let inspection = inspect(&db).unwrap();
    let original = metadata(&db);
    let names = tables(&db);
    let rows = history_preserved_rows(&db, &names, 22);
    drop(db);
    FAIL_STEP.with(|failure| failure.set(Some(23)));
    let error = Registry::at(&path)
        .err()
        .expect("real24checkpoint must fail after both bodies");
    FAIL_STEP.with(|failure| failure.set(None));
    assert!(error.contains("failure at 23"), "{error}");
    assert_eq!(bytes(&path), before);
    assert!(!wal(&path).exists());
    let retained = Connection::open(&path).unwrap();
    require_open_layout(&retained, 22).unwrap();
    assert_eq!(inspect(&retained).unwrap(), inspection);
    assert_eq!(metadata(&retained), original);
    assert_eq!(history_preserved_rows(&retained, &names, 22), rows);
    assert!(retained.is_autocommit());
    drop(retained);
    let registry = Registry::at(&path).unwrap();
    require_current(&registry.db).unwrap();
    assert_eq!(metadata(&registry.db), original);
    assert_eq!(history_preserved_rows(&registry.db, &names, 22), rows);
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
fn damaged_twenty_three_layout_refuses_before_writable_admission_or_counter_repair() {
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
    let id = registry.automations(None, 100).unwrap()[0].id;
    let definition = registry.automation(id).unwrap().definition;
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

fn history_preserved_rows(
    db: &Connection,
    names: &[String],
    version: u32,
) -> Vec<(String, Vec<Vec<String>>)> {
    names
        .iter()
        .map(|name| {
            let quoted = format!("\"{}\"", name.replace('"', "\"\""));
            let projection = if name == "automation_firings" && version < 22 {
                db.prepare(&format!("PRAGMA table_info({quoted})"))
                    .unwrap()
                    .query_map([], |row| row.get::<_, String>(1))
                    .unwrap()
                    .collect::<rusqlite::Result<Vec<_>>>()
                    .unwrap()
                    .into_iter()
                    .filter(|column| column != "previous_firing")
                    .map(|column| format!("\"{}\"", column.replace('"', "\"\"")))
                    .collect::<Vec<_>>()
                    .join(",")
            } else {
                "*".into()
            };
            let select = format!("SELECT {projection} FROM {quoted}");
            let columns = db.prepare(&select).unwrap().column_count();
            let order = (1..=columns)
                .map(|column| column.to_string())
                .collect::<Vec<_>>()
                .join(",");
            let mut query = db.prepare(&format!("{select} ORDER BY {order}")).unwrap();
            let rows = query
                .query_map([], |row| {
                    (0..columns)
                        .map(|column| row.get_ref(column).map(|value| format!("{value:?}")))
                        .collect::<rusqlite::Result<Vec<_>>>()
                })
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap();
            (name.clone(), rows)
        })
        .collect()
}

// Real source APIs populate this private fixture, then the existing published
// constructor copies only the exact tables/columns of20,21or22. No newer store
// is stripped or relabeled. No Worker, provider, terminal or scheduler launches.
fn history_predecessor_source() -> (
    Registry,
    tempfile::TempDir,
    crate::workspace_model::views::Scope,
) {
    use crate::agent_execution::Update;
    use crate::automations::{DirectoryIdentity, FiringState, Occurrence, Trigger};
    use crate::model::{
        CardId, CardKind, LaunchRecipe, TerminalContinuity, TerminalId, TerminalSession,
    };
    use crate::workspace_model::views::{Pane, Scope};
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("source.sqlite3");
    let (_, channel) = legacy(&path, SCHEMA_VERSION);
    let mut registry = Registry::at(&path).unwrap();
    let scope = Scope::Channel(channel);
    let recipe = LaunchRecipe {
        program: "/bin/true".into(),
        arguments: vec![],
        working_directory: "/kept".into(),
        environment: Default::default(),
        unset_environment: Default::default(),
    };
    let terminal = TerminalId::new();
    let pane = Pane {
        id: CardId::new(),
        title: "Saved synthetic terminal".into(),
        user_title: None,
        pending_terminal_start: false,
        width_fraction: 0.5,
        scope: None,
        kind: CardKind::Terminal(TerminalSession {
            id: terminal,
            launch: recipe.clone(),
            transcript_path: None,
            resume_hint: None,
            owned_codex_listener: None,
            recovery_origin: None,
            predecessor: None,
            continuity: TerminalContinuity::RelaunchAvailable {
                recipe,
                last_exit: None,
            },
        }),
    };
    registry
        .apply(&ModelEdit::SeedView {
            scope: scope.clone(),
            panes: vec![pane],
            closed: vec![],
        })
        .unwrap();
    registry.pin_window_terminals(&[terminal]).unwrap();
    let definition: crate::automations::Definition = serde_json::from_value(serde_json::json!({
        "name":"Synthetic upgrade source", "prompt":"No provider launched",
        "agent":{"provider":"Claude","account":null,"target":{"repository":"/kept","directory":"/kept","channel":null},"surface":"Chat","model":null,"permission_mode":null},
        "schedule":{"pattern":{"daily":{"hour":9,"minute":0}},"timezone":"UTC"},"session_mode":"fresh","precheck":null
    })).unwrap();
    let identity = DirectoryIdentity {
        device: 1,
        inode: 1,
    };
    let automation = registry
        .create_automation(definition.clone(), 100, identity.clone())
        .unwrap();
    let accepted = registry
        .claim_automation(
            automation.id,
            automation.execution_revision,
            None,
            Trigger::Manual {
                request: uuid::Uuid::new_v4(),
            },
            None,
            true,
            200,
        )
        .unwrap()
        .accepted
        .unwrap();
    let running = registry
        .update_agent_run(
            accepted.run.id,
            accepted.run.revision,
            Update::Started {
                provider_key: "synthetic".into(),
            },
            300,
        )
        .unwrap();
    registry
        .update_agent_run(running.id, running.revision, Update::Waiting, 400)
        .unwrap();
    assert!(
        !registry
            .pending_automation_attention(100)
            .unwrap()
            .is_empty()
    );

    // Exact22 lineage/cleanup payloads are real source transactions. The first
    // precheck never reaches Worker::launch, so its cleanup evidence is honest.
    let mut checked_definition = definition.clone();
    checked_definition.precheck = Some(crate::automations::Precheck {
        command: "exit 0".into(),
        timeout_seconds: 5,
    });
    let checked = registry
        .create_automation(checked_definition, 500, identity.clone())
        .unwrap();
    let first = registry
        .claim_automation(
            checked.id,
            checked.execution_revision,
            None,
            Trigger::Manual {
                request: uuid::Uuid::new_v4(),
            },
            None,
            true,
            600,
        )
        .unwrap()
        .firing;
    assert!(
        registry
            .finish_automation_precheck(
                first.id,
                crate::host::automations::precheck::Outcome::Stopped {
                    state: FiringState::Failed,
                    skip: None,
                    diagnostic: "Synthetic prelaunch refusal".into()
                },
                700
            )
            .unwrap()
            .is_none()
    );
    registry
        .confirm_unlaunched_automation_precheck(first.id, first.revision, 701)
        .unwrap();
    registry
        .claim_automation(
            checked.id,
            checked.execution_revision,
            None,
            Trigger::Manual {
                request: uuid::Uuid::new_v4(),
            },
            None,
            true,
            800,
        )
        .unwrap();

    registry
        .apply(&ModelEdit::OpenProject {
            repository: "/attention".into(),
            name: "Independent attention source".into(),
            kind: ProjectKind::Folder,
        })
        .unwrap();
    let mut no_run_definition = definition;
    no_run_definition.agent.target.repository = "/attention".into();
    no_run_definition.agent.target.directory = "/attention".into();
    let no_run = registry
        .create_automation(no_run_definition, 900, identity)
        .unwrap();
    registry
        .set_automation_enabled(
            no_run.id,
            no_run.revision,
            Some(Occurrence {
                due_ms: 10_000,
                local: "future fixture".into(),
            }),
            true,
            901,
        )
        .unwrap();
    registry
        .apply(&ModelEdit::CloseProject {
            repository: "/attention".into(),
        })
        .unwrap();
    assert!(
        !registry
            .pending_automation_notifications(100)
            .unwrap()
            .is_empty()
    );
    (registry, directory, scope)
}
