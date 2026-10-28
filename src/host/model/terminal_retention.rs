//! Durable saved-pane membership. Pins defer retirement; they never certify
//! cleanup, authenticate a process, or release an exact terminal worker.
use super::{Registry, schema};
use crate::{model::TerminalId, workspace_model::views::Pane};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use std::collections::BTreeSet;

pub(super) fn migrate(tx: &Transaction<'_>) -> Result<(), String> {
    tx.execute_batch(
        "CREATE TABLE terminal_retention_pins(
            terminal TEXT PRIMARY KEY,
            generation TEXT NOT NULL
        ) STRICT;
        CREATE TABLE shared_terminal_retention_pins(
            pane TEXT PRIMARY KEY REFERENCES shared_view_panes(id) ON DELETE CASCADE,
            terminal TEXT NOT NULL
        ) STRICT;
        CREATE INDEX shared_terminal_retention_by_terminal
            ON shared_terminal_retention_pins(terminal);",
    )
    .map_err(|error| error.to_string())?;
    // The only all-view scan is this migration. Stream one saved view at a
    // time; later censuses perform only indexed candidate lookups. A corrupt
    // pane/owner cannot silently lose its conservative retention pin.
    let mut views = tx
        .prepare("SELECT scope,panes FROM shared_views")
        .map_err(|error| error.to_string())?;
    let rows = views
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(|error| error.to_string())?;
    for row in rows {
        let (scope, panes) = row.map_err(|error| error.to_string())?;
        let panes: Vec<Pane> = serde_json::from_str(&panes)
            .map_err(|error| format!("Cannot recover saved terminal retention: {error}"))?;
        for pane in &panes {
            let owner: Option<String> = tx
                .query_row(
                    "SELECT scope FROM shared_view_panes WHERE id=?1",
                    [pane.id.to_string()],
                    |row| row.get(0),
                )
                .optional()
                .map_err(|error| error.to_string())?;
            if owner.as_deref() != Some(scope.as_str()) {
                return Err(
                    "Saved pane ownership is inconsistent; terminal retention is unknown".into(),
                );
            }
        }
        sync_view_pins(tx, &panes)?;
    }
    Ok(())
}

/// Called within the same existing writer transaction as the saved view.
/// FK cascade removes closed panes; updates also clear a changed nonterminal.
pub(super) fn sync_view_pins(db: &Connection, panes: &[Pane]) -> Result<(), String> {
    let mut put = db
        .prepare(
            "INSERT INTO shared_terminal_retention_pins(pane,terminal) VALUES (?1,?2)
         ON CONFLICT(pane) DO UPDATE SET terminal=excluded.terminal
         WHERE shared_terminal_retention_pins.terminal<>excluded.terminal",
        )
        .map_err(|error| error.to_string())?;
    let mut remove = db
        .prepare("DELETE FROM shared_terminal_retention_pins WHERE pane=?1")
        .map_err(|error| error.to_string())?;
    let mut seen = BTreeSet::new();
    for pane in panes {
        if !seen.insert(pane.id) {
            return Err("Saved pane IDs are duplicated; terminal retention is unknown".into());
        }
        let id = pane.id.to_string();
        let terminal = match &pane.kind {
            crate::model::CardKind::Terminal(session) => Some(session.id),
            crate::model::CardKind::Chat(session) => {
                session.terminal.as_ref().map(|terminal| terminal.id)
            }
            _ => None,
        };
        if let Some(terminal) = terminal {
            put.execute(params![id, terminal.to_string()])
                .map_err(|error| error.to_string())?;
        } else {
            remove.execute([id]).map_err(|error| error.to_string())?;
        }
    }
    Ok(())
}

const PINNED_SQL: &str = "SELECT EXISTS(SELECT 1 FROM terminal_retention_pins WHERE terminal=?1)
         OR EXISTS(SELECT 1 FROM shared_terminal_retention_pins WHERE terminal=?1)";

pub(super) fn pinned_terminals(
    db: &Connection,
    candidates: &BTreeSet<TerminalId>,
) -> Result<BTreeSet<TerminalId>, String> {
    // Header/layout admission and membership use one snapshot. An old retained
    // connection must not treat a newer store as a successful empty census.
    let tx = Transaction::new_unchecked(db, TransactionBehavior::Deferred)
        .map_err(|error| error.to_string())?;
    schema::require_current(&tx)?;
    let mut pinned = BTreeSet::new();
    {
        let mut query = tx.prepare(PINNED_SQL).map_err(|error| error.to_string())?;
        for id in candidates {
            if query
                .query_row([id.to_string()], |row| row.get::<_, bool>(0))
                .map_err(|error| error.to_string())?
            {
                pinned.insert(*id);
            }
        }
    }
    tx.commit().map_err(|error| error.to_string())?;
    Ok(pinned)
}

impl Registry {
    pub(crate) fn pin_window_terminals(&mut self, terminals: &[TerminalId]) -> Result<(), String> {
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| error.to_string())?;
        schema::require_current(&tx)?;
        // Even delete/recreate must differ from a stale close's opaque receipt.
        let generation = uuid::Uuid::new_v4().to_string();
        for terminal in terminals {
            tx.execute(
                "INSERT INTO terminal_retention_pins(terminal,generation) VALUES (?1,?2)
                 ON CONFLICT(terminal) DO UPDATE SET generation=excluded.generation",
                params![terminal.to_string(), generation],
            )
            .map_err(|error| error.to_string())?;
        }
        tx.commit().map_err(|error| error.to_string())
    }

    pub(crate) fn retention_pin_generation(
        &self,
        terminal: TerminalId,
    ) -> Result<Option<String>, String> {
        let tx = Transaction::new_unchecked(&self.db, TransactionBehavior::Deferred)
            .map_err(|error| error.to_string())?;
        schema::require_current(&tx)?;
        let generation = tx
            .query_row(
                "SELECT generation FROM terminal_retention_pins WHERE terminal=?1",
                [terminal.to_string()],
                |row| row.get(0),
            )
            .optional()
            .map_err(|error| error.to_string())?;
        tx.commit().map_err(|error| error.to_string())?;
        Ok(generation)
    }

    /// Only removes this saved membership generation. A successful metadata
    /// delete is not a successful Close, OS join, or native-holder teardown.
    pub(crate) fn forget_retention_pin(
        &mut self,
        terminal: TerminalId,
        generation: &str,
    ) -> Result<(), String> {
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| error.to_string())?;
        schema::require_current(&tx)?;
        tx.execute(
            "DELETE FROM terminal_retention_pins WHERE terminal=?1 AND generation=?2",
            params![terminal.to_string(), generation],
        )
        .map_err(|error| error.to_string())?;
        tx.commit().map_err(|error| error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        model::{CardId, CardKind, LaunchRecipe, TerminalContinuity, TerminalSession},
        workspace_model::views::Scope,
    };
    use std::path::Path;

    fn terminal_pane() -> Pane {
        let recipe = LaunchRecipe {
            program: "/bin/sh".into(),
            arguments: vec![],
            working_directory: "/saved".into(),
            environment: Default::default(),
            unset_environment: Default::default(),
        };
        Pane {
            id: CardId::new(),
            title: "saved terminal".into(),
            user_title: None,
            pending_terminal_start: false,
            width_fraction: 1.0,
            scope: Some("/saved".into()),
            kind: CardKind::Terminal(TerminalSession {
                id: TerminalId::new(),
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
        }
    }
    fn terminal(pane: &Pane) -> TerminalId {
        match &pane.kind {
            CardKind::Terminal(session) => session.id,
            CardKind::Chat(session) => session.terminal.as_ref().expect("terminal-backed chat").id,
            _ => panic!("expected terminal"),
        }
    }

    /// Construct published18 using the real17 then placement18 migrations.
    /// Populate the saved17 view BEFORE placement backfill; never downgrade a newer store.
    fn published18(path: &Path, pane: &Pane) -> (String, String) {
        let mut db = Connection::open(path).unwrap();
        let tx = db.transaction().unwrap();
        super::super::migrate_through(&tx, 0, 17).unwrap();
        let channel = crate::model::ChannelId::new();
        tx.execute("INSERT INTO projects(repository,name,ordinal,kind) VALUES('/saved','saved',1,'folder')", []).unwrap();
        tx.execute("INSERT INTO channels(id,repository,title,ordinal,title_source) VALUES(?1,'/saved','saved',1,2)", [channel.to_string()]).unwrap();
        super::super::channel_work::created_in(&tx, channel).unwrap();
        let scope = serde_json::to_string(&Scope::Channel(channel)).unwrap();
        let panes = serde_json::to_string(&vec![pane.clone()]).unwrap();
        tx.execute(
            "INSERT INTO shared_view_scopes(scope,channel) VALUES(?1,?2)",
            params![scope, channel.to_string()],
        )
        .unwrap();
        tx.execute(
            "INSERT INTO shared_views(scope,revision,panes,retired) VALUES(?1,7,?2,'[]')",
            params![scope, panes],
        )
        .unwrap();
        tx.execute(
            "INSERT INTO shared_view_panes(id,scope) VALUES(?1,?2)",
            params![pane.id.to_string(), scope],
        )
        .unwrap();
        let identity: String = tx
            .query_row("SELECT identity FROM metadata WHERE key=0", [], |row| {
                row.get(0)
            })
            .unwrap();
        super::super::migrate_through(&tx, 17, 18).unwrap();
        tx.pragma_update(None, "user_version", 18).unwrap();
        tx.commit().unwrap();
        (identity, scope)
    }
    fn files(path: &Path) -> (Vec<u8>, Option<Vec<u8>>) {
        let wal = path.with_file_name(format!(
            "{}-wal",
            path.file_name().unwrap().to_str().unwrap()
        ));
        (std::fs::read(path).unwrap(), std::fs::read(wal).ok())
    }

    #[test]
    fn local_window_terminal_pins_survive_disconnect_and_registry_restart() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("registry.sqlite3");
        let terminal = TerminalId::new();
        let mut registry = Registry::at(&path).unwrap();
        registry.pin_window_terminals(&[terminal]).unwrap();
        registry.pin_window_terminals(&[]).unwrap();
        assert_eq!(
            registry
                .pinned_terminals(&BTreeSet::from([terminal]))
                .unwrap(),
            BTreeSet::from([terminal])
        );
        drop(registry);
        let mut reopened = Registry::at(&path).unwrap();
        assert!(
            reopened
                .pinned_terminals(&BTreeSet::from([terminal]))
                .unwrap()
                .contains(&terminal)
        );
        let generation = reopened
            .retention_pin_generation(terminal)
            .unwrap()
            .unwrap();
        reopened
            .forget_retention_pin(terminal, &generation)
            .unwrap();
        assert!(
            reopened
                .pinned_terminals(&BTreeSet::from([terminal]))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn stale_close_cannot_erase_republished_or_recreated_window_ownership() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("registry.sqlite3");
        let mut registry = Registry::at(&path).unwrap();
        let mut publisher = registry.execution_connection().unwrap();
        let terminal = TerminalId::new();
        publisher.pin_window_terminals(&[terminal]).unwrap();
        let closing = registry
            .retention_pin_generation(terminal)
            .unwrap()
            .unwrap();
        publisher.pin_window_terminals(&[terminal]).unwrap();
        let replacement = registry
            .retention_pin_generation(terminal)
            .unwrap()
            .unwrap();
        assert_ne!(replacement, closing);
        registry.forget_retention_pin(terminal, &closing).unwrap();
        assert_eq!(
            registry.retention_pin_generation(terminal).unwrap(),
            Some(replacement.clone())
        );
        registry
            .forget_retention_pin(terminal, &replacement)
            .unwrap();
        publisher.pin_window_terminals(&[terminal]).unwrap();
        registry
            .forget_retention_pin(terminal, &replacement)
            .unwrap();
        let recreated = registry
            .retention_pin_generation(terminal)
            .unwrap()
            .unwrap();
        assert_ne!(recreated, replacement);
        assert!(
            registry
                .pinned_terminals(&BTreeSet::from([terminal]))
                .unwrap()
                .contains(&terminal)
        );
    }

    #[test]
    fn published18_migration_backfills_saved_panes_without_changing_model_or_work() {
        for as_chat in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("registry.sqlite3");
            let mut pane = terminal_pane();
            if as_chat {
                let CardKind::Terminal(terminal) = pane.kind else {
                    unreachable!()
                };
                pane.kind = CardKind::Chat(crate::chat::Session {
                    id: terminal.id,
                    launch: crate::chat::Launch {
                        provider: crate::agent::Provider::Claude,
                        recipe: terminal.launch.clone(),
                        session_id: None,
                    },
                    proof: terminal.continuity.live_proof(),
                    terminal: Some(terminal),
                    owned_codex_listener: None,
                    native_continuity: Default::default(),
                });
            }
            let (identity, scope) = published18(&path, &pane);
            let before = Connection::open(&path).unwrap();
            let work: String = before
                .query_row(
                    "SELECT group_concat(channel||':'||revision) FROM channel_work_homes",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(
                before
                    .pragma_query_value(None, "user_version", |row| row.get::<_, u32>(0))
                    .unwrap(),
                18
            );
            assert_eq!(
                before
                    .query_row(
                        "SELECT count(*) FROM sqlite_schema WHERE name='terminal_retention_pins'",
                        [],
                        |row| row.get::<_, u32>(0)
                    )
                    .unwrap(),
                0
            );
            let placement_before: Vec<(String, i64)> = before
                .prepare("SELECT pane,epoch FROM channel_work_placement_epochs ORDER BY pane")
                .unwrap()
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap();
            assert_eq!(placement_before, vec![(pane.id.to_string(), 1)]);
            let clock_before: Vec<(i64, i64)> = before
                .prepare("SELECT id,last_epoch FROM channel_work_placement_clock ORDER BY id")
                .unwrap()
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap();
            assert_eq!(clock_before, vec![(1, 1)]);
            drop(before);
            let registry = Registry::at(&path).unwrap();
            assert_eq!(
                registry
                    .db
                    .pragma_query_value(None, "user_version", |row| row.get::<_, u32>(0))
                    .unwrap(),
                super::super::SCHEMA_VERSION
            );
            let placement_after: Vec<(String, i64)> = registry
                .db
                .prepare("SELECT pane,epoch FROM channel_work_placement_epochs ORDER BY pane")
                .unwrap()
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap();
            assert_eq!(
                placement_after, placement_before,
                "retention migration must preserve published active placement epochs"
            );
            let clock_after: Vec<(i64, i64)> = registry
                .db
                .prepare("SELECT id,last_epoch FROM channel_work_placement_clock ORDER BY id")
                .unwrap()
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap();
            assert_eq!(
                clock_after, clock_before,
                "retention migration must preserve the published global allocator"
            );
            assert_eq!(registry.model().unwrap().identity.to_string(), identity);
            let saved: String = registry
                .db
                .query_row(
                    "SELECT panes FROM shared_views WHERE scope=?1",
                    [&scope],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(
                serde_json::from_str::<Vec<Pane>>(&saved).unwrap(),
                vec![pane.clone()]
            );
            assert_eq!(
                registry
                    .db
                    .query_row(
                        "SELECT revision FROM shared_views WHERE scope=?1",
                        [&scope],
                        |row| row.get::<_, u64>(0)
                    )
                    .unwrap(),
                7
            );
            assert_eq!(
                registry
                    .db
                    .query_row(
                        "SELECT group_concat(channel||':'||revision) FROM channel_work_homes",
                        [],
                        |row| row.get::<_, String>(0)
                    )
                    .unwrap(),
                work
            );
            assert!(
                registry
                    .pinned_terminals(&BTreeSet::from([terminal(&pane)]))
                    .unwrap()
                    .contains(&terminal(&pane))
            );
            drop(registry);
            assert!(
                Registry::at(&path)
                    .unwrap()
                    .pinned_terminals(&BTreeSet::from([terminal(&pane)]))
                    .unwrap()
                    .contains(&terminal(&pane))
            );
        }
    }

    #[test]
    fn damaged_published18_pane_backfill_refuses_without_publishing_schema_or_losing_bytes() {
        for damage in ["json", "owner", "duplicate"] {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("registry.sqlite3");
            let pane = terminal_pane();
            let (_, scope) = published18(&path, &pane);
            let db = Connection::open(&path).unwrap();
            match damage {
                "json" => {
                    db.execute("UPDATE shared_views SET panes='not-json'", [])
                        .unwrap();
                }
                "owner" => {
                    db.execute("DELETE FROM shared_view_panes", []).unwrap();
                }
                "duplicate" => {
                    db.execute(
                        "UPDATE shared_views SET panes=?1",
                        [serde_json::to_string(&vec![pane.clone(), pane]).unwrap()],
                    )
                    .unwrap();
                }
                _ => unreachable!(),
            }
            let saved: String = db
                .query_row(
                    "SELECT panes FROM shared_views WHERE scope=?1",
                    [&scope],
                    |row| row.get(0),
                )
                .unwrap();
            drop(db);
            let original = files(&path);
            assert!(Registry::at(&path).is_err());
            assert_eq!(
                files(&path),
                original,
                "failed {damage} migration must retain original bytes"
            );
            let db = Connection::open(&path).unwrap();
            assert_eq!(
                db.pragma_query_value(None, "user_version", |row| row.get::<_, u32>(0))
                    .unwrap(),
                18
            );
            assert_eq!(
                db.query_row(
                    "SELECT panes FROM shared_views WHERE scope=?1",
                    [&scope],
                    |row| row.get::<_, String>(0)
                )
                .unwrap(),
                saved
            );
            assert_eq!(
                db.query_row(
                    "SELECT count(*) FROM sqlite_schema WHERE name='terminal_retention_pins'",
                    [],
                    |row| row.get::<_, u32>(0)
                )
                .unwrap(),
                0
            );
        }
    }

    /// Build the actual outbox19 predecessor, never a downgraded retention store.
    fn outbox19(path: &Path, pane: &Pane) -> (String, String) {
        use crate::{
            agent::Provider,
            agent_execution::{Spec, Surface, Target},
            automations::{
                Definition, DirectoryIdentity, LifecycleCause, Schedule, SchedulePattern,
                SessionMode,
            },
        };
        let (identity, scope) = published18(path, pane);
        let mut db = Connection::open(path).unwrap();
        let tx = db.transaction().unwrap();
        super::super::migrate_through(&tx, 18, 19).unwrap();
        let automation = super::super::automations::create_automation_in(
            &tx,
            Definition {
                name: "Retained private name".into(),
                prompt: "Retained private prompt".into(),
                agent: Spec {
                    provider: Provider::Codex,
                    account: None,
                    target: Target {
                        repository: "/saved".into(),
                        directory: "/saved".into(),
                        placement: None,
                    },
                    surface: Surface::Chat,
                    model: None,
                    permission_mode: None,
                    reasoning_effort: None,
                },
                schedule: Schedule {
                    pattern: SchedulePattern::Daily { hour: 9, minute: 0 },
                    timezone: "UTC".into(),
                },
                session_mode: SessionMode::Reuse,
                precheck: None,
            },
            100,
            DirectoryIdentity {
                device: 1,
                inode: 1,
            },
            None,
        )
        .unwrap();
        super::super::automation_audit::record(
            &tx,
            &automation,
            LifecycleCause::ProjectClosed,
            None,
        )
        .unwrap();
        super::super::automation_notifications::queue(
            &tx,
            &automation,
            LifecycleCause::ProjectClosed,
            None,
        )
        .unwrap();
        tx.execute(
            "INSERT INTO shared_view_edits(operation,scope) VALUES(?1,?2)",
            params![uuid::Uuid::new_v4().to_string(), scope],
        )
        .unwrap();
        tx.pragma_update(None, "user_version", 19).unwrap();
        tx.commit().unwrap();
        (identity, scope)
    }

    fn table_rows(
        db: &Connection,
    ) -> std::collections::BTreeMap<String, Vec<Vec<rusqlite::types::Value>>> {
        let names = db
            .prepare("SELECT name FROM sqlite_schema WHERE type='table' ORDER BY name")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        names
            .into_iter()
            .map(|name| {
                let order = if name == "compacted_view_operations" {
                    "operation"
                } else {
                    "rowid"
                };
                let mut statement = db
                    .prepare(&format!(
                        "SELECT * FROM \"{}\" ORDER BY {order}",
                        name.replace('"', "\"\"")
                    ))
                    .unwrap();
                let columns = statement.column_count();
                let rows = statement
                    .query_map([], |row| {
                        (0..columns)
                            .map(|column| row.get(column))
                            .collect::<rusqlite::Result<Vec<_>>>()
                    })
                    .unwrap()
                    .collect::<rusqlite::Result<Vec<_>>>()
                    .unwrap();
                (name, rows)
            })
            .collect()
    }

    #[test]
    fn authentic_outbox19_migrates_to_retention20_without_changing_receipts_or_placement() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("outbox19.sqlite3");
        let pane = terminal_pane();
        let (identity, scope) = outbox19(&path, &pane);
        let db = Connection::open(&path).unwrap();
        assert_eq!(
            db.pragma_query_value(None, "user_version", |row| row.get::<_, u32>(0))
                .unwrap(),
            19
        );
        // The new history column has an exact insertion-order backfill;
        // original payload projection must not hide its value or lineage.
        let expected_previous = db.prepare(
            "SELECT id,lag(id) OVER(PARTITION BY automation ORDER BY rowid) FROM automation_firings ORDER BY rowid"
        ).unwrap().query_map([], |row| Ok((row.get::<_,String>(0)?, row.get::<_,Option<String>>(1)?)))
            .unwrap().collect::<rusqlite::Result<Vec<_>>>().unwrap();
        let before = table_rows(&db);
        assert!(!before.contains_key("terminal_retention_pins"));
        assert!(!before.contains_key("shared_terminal_retention_pins"));
        assert_eq!(before["automation_notifications"].len(), 1);
        assert_eq!(before["automation_lifecycle"].len(), 2);
        assert_eq!(before["shared_view_edits"].len(), 1);
        assert_eq!(before["channel_work_placement_epochs"].len(), 1);
        assert_eq!(before["channel_work_placement_clock"].len(), 1);
        let attention = super::super::automation_notifications::pending(&db, 100).unwrap();
        assert_eq!(attention.len(), 1);
        drop(db);
        for _ in 0..2 {
            let registry = Registry::at(&path).unwrap();
            let previous = registry
                .db
                .prepare("SELECT id,previous_firing FROM automation_firings ORDER BY rowid")
                .unwrap()
                .query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
                })
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap();
            assert_eq!(previous, expected_previous);
            assert_eq!(
                registry
                    .db
                    .pragma_query_value(None, "user_version", |row| row.get::<_, u32>(0))
                    .unwrap(),
                super::super::SCHEMA_VERSION
            );
            let mut after = table_rows(&registry.db);
            for table in [
                "retired_view_panes",
                "compacted_view_operation_scopes",
                "compacted_view_operations",
            ] {
                assert!(
                    after.remove(table).unwrap().is_empty(),
                    "this fixture has no retired panes or receipt overflow"
                );
            }
            assert!(
                after.remove("automation_run_attention").unwrap().is_empty(),
                "the added run-attention table must not replay historical runs"
            );
            assert!(
                after
                    .remove("automation_precheck_history_cleanup")
                    .unwrap()
                    .is_empty()
            );
            assert!(
                after
                    .remove("automation_retired_manual_requests")
                    .unwrap()
                    .is_empty()
            );
            assert!(
                after
                    .remove("automation_proposal_notifications")
                    .unwrap()
                    .is_empty(),
                "schema24 does not replay historical proposals"
            );
            let counters = after.get_mut("metadata").unwrap();
            assert_eq!(counters.len(), 1);
            let metadata = &mut counters[0];
            assert_eq!(metadata.len(), before["metadata"][0].len() + 2);
            let pending: i64 = registry
                .db
                .query_row(
                    "SELECT count(*) FROM automation_proposals WHERE pending=1",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(
                metadata.pop().unwrap(),
                rusqlite::types::Value::Integer(pending)
            );
            assert_eq!(metadata.pop().unwrap(), rusqlite::types::Value::Integer(1));
            // History22 appends one nullable column; preserve every original
            // firing payload in this published19 comparison.
            for row in after.get_mut("automation_firings").unwrap() {
                assert_eq!(row.len(), 9);
                row.pop().unwrap();
            }
            assert!(after.remove("terminal_retention_pins").unwrap().is_empty());
            assert_eq!(
                after.remove("shared_terminal_retention_pins").unwrap(),
                vec![vec![
                    rusqlite::types::Value::Text(pane.id.to_string()),
                    rusqlite::types::Value::Text(terminal(&pane).to_string())
                ]]
            );
            assert_eq!(
                after, before,
                "every original table value, including audit/outbox/view receipts and placement allocator, must remain exact"
            );
            assert_eq!(
                registry.pending_automation_notifications(100).unwrap(),
                attention
            );
            assert_eq!(registry.model().unwrap().identity.to_string(), identity);
            assert_eq!(
                registry
                    .db
                    .query_row(
                        "SELECT scope FROM shared_view_panes WHERE id=?1",
                        [pane.id.to_string()],
                        |row| row.get::<_, String>(0)
                    )
                    .unwrap(),
                scope
            );
            assert!(
                registry
                    .pinned_terminals(&BTreeSet::from([terminal(&pane)]))
                    .unwrap()
                    .contains(&terminal(&pane))
            );
        }
        // The exact pending journal receipt still acknowledges after migration;
        // terminal membership is independent of this destructive outbox ACK.
        let mut registry = Registry::at(&path).unwrap();
        assert!(
            registry
                .acknowledge_automation_notification(&attention[0])
                .unwrap()
        );
        assert!(
            registry
                .pending_automation_notifications(100)
                .unwrap()
                .is_empty()
        );
        assert!(
            registry
                .pinned_terminals(&BTreeSet::from([terminal(&pane)]))
                .unwrap()
                .contains(&terminal(&pane))
        );
        assert!(
            !registry
                .acknowledge_automation_notification(&attention[0])
                .unwrap()
        );
    }

    #[test]
    fn outbox19_unknown_layout_and_retention_version_aliases_refuse_without_writes() {
        for damage in [
            "unknown19",
            "false20",
            "private_retention19",
            "damaged_saved19",
            "downgraded20",
        ] {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("refused.sqlite3");
            let pane = terminal_pane();
            if damage == "private_retention19" {
                published18(&path, &pane);
                let mut db = Connection::open(&path).unwrap();
                let tx = db.transaction().unwrap();
                // Reproduce the old PRIVATE conflicting19 allocation only as
                // a negative preimage. It is not the authentic predecessor.
                super::migrate(&tx).unwrap();
                tx.pragma_update(None, "user_version", 19).unwrap();
                tx.commit().unwrap();
            } else {
                outbox19(&path, &pane);
                if damage == "downgraded20" {
                    // Start from an actually migrated current store, then
                    // relabel only its header. Extra known objects are not an
                    // authentic outbox19 layout and must not be reopened.
                    drop(Registry::at(&path).unwrap());
                }
                let db = Connection::open(&path).unwrap();
                db.execute_batch(if damage == "unknown19" {
                    "CREATE TABLE unknown_outbox_payload(value TEXT) STRICT;"
                } else if damage == "damaged_saved19" {
                    "UPDATE shared_views SET panes='not-json';"
                } else if damage == "downgraded20" {
                    "PRAGMA user_version=19;"
                } else {
                    "PRAGMA user_version=20;"
                })
                .unwrap();
            }
            let original = files(&path);
            let error = Registry::at(&path)
                .err()
                .expect("an unknown or mislabeled layout must refuse");
            assert!(
                if damage == "damaged_saved19" {
                    error.contains("Cannot recover saved terminal retention")
                } else {
                    error.contains("layout does not match published schema")
                },
                "{damage}: {error}"
            );
            assert_eq!(
                files(&path),
                original,
                "{damage} may not publish migration or change journal bytes"
            );
        }
    }

    #[test]
    fn retained_pin_connections_refuse_newer_or_drifted_schema_without_empty_success_or_dml() {
        for alteration in [
            format!("PRAGMA user_version={}", super::super::SCHEMA_VERSION + 1),
            "CREATE TABLE unexpected_retention(value TEXT)".to_owned(),
        ] {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("registry.sqlite3");
            let mut registry = Registry::at(&path).unwrap();
            let terminal = TerminalId::new();
            registry.pin_window_terminals(&[terminal]).unwrap();
            let generation = registry
                .retention_pin_generation(terminal)
                .unwrap()
                .unwrap();
            let writer = registry.execution_connection().unwrap();
            writer.db.execute_batch(&alteration).unwrap();
            let changed = registry.db.total_changes();
            assert!(
                registry
                    .pinned_terminals(&BTreeSet::from([terminal]))
                    .is_err()
            );
            assert!(registry.retention_pin_generation(terminal).is_err());
            assert!(registry.pin_window_terminals(&[terminal]).is_err());
            assert!(
                registry
                    .forget_retention_pin(terminal, &generation)
                    .is_err()
            );
            assert_eq!(registry.db.total_changes(), changed);
            assert_eq!(
                writer
                    .db
                    .query_row(
                        "SELECT generation FROM terminal_retention_pins WHERE terminal=?1",
                        [terminal.to_string()],
                        |row| row.get::<_, String>(0)
                    )
                    .unwrap(),
                generation
            );
            drop(writer);
            drop(registry);
            let original = files(&path);
            assert!(Registry::at(&path).is_err());
            assert_eq!(files(&path), original);
        }
    }

    #[test]
    fn candidate_census_uses_indexes_and_does_not_parse_unrelated_saved_view_bodies() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("registry.sqlite3");
        let pane = terminal_pane();
        published18(&path, &pane);
        let mut registry = Registry::at(&path).unwrap();
        let history: Vec<_> = (0..2000).map(|_| TerminalId::new()).collect();
        registry.pin_window_terminals(&history).unwrap();
        let unrelated = Scope::Worktree {
            repository: "/saved".into(),
            path: "/saved/unrelated".into(),
        };
        registry
            .apply(&crate::workspace_model::ModelEdit::SeedView {
                scope: unrelated.clone(),
                panes: vec![],
                closed: vec![],
            })
            .unwrap();
        // A per-census JSON/table scan would fail on this unrelated view.
        // Target membership was already established by the migration.
        registry
            .db
            .execute(
                "UPDATE shared_views SET panes='not-json' WHERE scope=?1",
                [serde_json::to_string(&unrelated).unwrap()],
            )
            .unwrap();
        let missing = TerminalId::new();
        let candidates = BTreeSet::from([terminal(&pane), history[17], missing]);
        assert_eq!(
            registry.pinned_terminals(&candidates).unwrap(),
            BTreeSet::from([terminal(&pane), history[17]])
        );
        let plan: Vec<String> = registry
            .db
            .prepare(&format!("EXPLAIN QUERY PLAN {PINNED_SQL}"))
            .unwrap()
            .query_map([missing.to_string()], |row| row.get(3))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert!(
            plan.iter()
                .any(|line| line.contains("SEARCH terminal_retention_pins")),
            "{plan:?}"
        );
        assert!(
            plan.iter()
                .any(|line| line.contains("SEARCH shared_terminal_retention_pins")),
            "{plan:?}"
        );
        assert!(
            !plan
                .iter()
                .any(|line| line.contains("SCAN terminal_retention_pins")
                    || line.contains("SCAN shared_terminal_retention_pins")
                    || line.contains("shared_views")),
            "{plan:?}"
        );
    }
}
