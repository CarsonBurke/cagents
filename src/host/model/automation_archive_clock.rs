//! Archive discovery changes independently of ordinary live run activity.
use super::*;

impl Registry {
    pub(crate) fn automation_archive_sequence(&self) -> Result<u64, String> {
        self.db
            .query_row(
                "SELECT automation_archive_sequence FROM metadata WHERE key=0",
                [],
                |row| row.get(0),
            )
            .map_err(|error| error.to_string())
    }
}

pub(super) fn migrate(db: &Connection) -> Result<(), String> {
    let tx = rusqlite::Transaction::new_unchecked(db, rusqlite::TransactionBehavior::Immediate)
        .map_err(|error| error.to_string())?;
    let current: u32 = tx
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .map_err(|error| error.to_string())?;
    if current > SCHEMA_VERSION {
        return Err(format!(
            "workspace registry schema {current} is newer than supported schema {SCHEMA_VERSION}"
        ));
    }
    if current >= 14 {
        return tx.commit().map_err(|error| error.to_string());
    }
    if current != 13 {
        return Err(format!(
            "workspace registry schema {current} must complete the automation checkpoint before archive discovery"
        ));
    }
    let exists:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM pragma_table_info('metadata') WHERE name='automation_archive_sequence')",[],|row|row.get(0)).map_err(|error|error.to_string())?;
    if !exists {
        tx.execute_batch("ALTER TABLE metadata ADD COLUMN automation_archive_sequence INTEGER NOT NULL DEFAULT 0 CHECK(automation_archive_sequence>=0);").map_err(|error|error.to_string())?;
    }
    // Current projection/alias/tombstone content matters, not a view revision
    // that changed without changing its presentation. Active-agent records and
    // agent_runs deliberately do not invalidate discovery.
    for (name, operation, table, condition, publish) in [
        (
            "snapshot_insert",
            "INSERT",
            "automation_snapshots",
            "",
            true,
        ),
        (
            "snapshot_update",
            "UPDATE",
            "automation_snapshots",
            "WHEN OLD.snapshot IS NOT NEW.snapshot OR OLD.agent IS NOT NEW.agent",
            true,
        ),
        (
            "snapshot_delete",
            "DELETE",
            "automation_snapshots",
            "",
            true,
        ),
        (
            "agent_insert",
            "INSERT",
            "agents",
            "WHEN NEW.closed=1 AND EXISTS(SELECT 1 FROM automation_snapshots WHERE agent=NEW.id)",
            true,
        ),
        (
            "agent_update",
            "UPDATE OF record,closed",
            "agents",
            "WHEN (OLD.closed=1 OR NEW.closed=1) AND (OLD.record IS NOT NEW.record OR OLD.closed IS NOT NEW.closed) AND EXISTS(SELECT 1 FROM automation_snapshots WHERE agent=NEW.id)",
            true,
        ),
        (
            "agent_delete",
            "DELETE",
            "agents",
            "WHEN OLD.closed=1 AND EXISTS(SELECT 1 FROM automation_snapshots WHERE agent=OLD.id)",
            true,
        ),
        ("view_insert", "INSERT", "shared_views", "", false),
        (
            "view_update",
            "UPDATE",
            "shared_views",
            "WHEN OLD.panes IS NOT NEW.panes OR OLD.retired IS NOT NEW.retired",
            false,
        ),
        ("view_delete", "DELETE", "shared_views", "", false),
        ("project_insert", "INSERT", "projects", "", false),
        ("project_delete", "DELETE", "projects", "", false),
        ("channel_insert", "INSERT", "channels", "", false),
        ("channel_delete", "DELETE", "channels", "", false),
        (
            "visibility",
            "UPDATE OF record",
            "automations",
            "WHEN json_extract(OLD.record,'$.definition.visibility') IS NOT json_extract(NEW.record,'$.definition.visibility')",
            false,
        ),
    ] {
        // STRICT INTEGER storage rejects overflow rather than resetting the
        // discovery clock. The mutation and clock change share one transaction.
        let sequence = if publish { ",sequence=sequence+1" } else { "" };
        tx.execute_batch(&format!("CREATE TRIGGER IF NOT EXISTS automation_archive_{name} AFTER {operation} ON {table} {condition} BEGIN UPDATE metadata SET automation_archive_sequence=automation_archive_sequence+1{sequence} WHERE key=0; END;")).map_err(|error|error.to_string())?;
    }
    tx.execute_batch("PRAGMA user_version=14;")
        .map_err(|error| error.to_string())?;
    tx.commit().map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn remove_draft_clock(db: &Connection) {
        let triggers:Vec<String>=db.prepare("SELECT name FROM sqlite_master WHERE type='trigger' AND name LIKE 'automation_archive_%'").unwrap().query_map([],|row|row.get(0)).unwrap().collect::<rusqlite::Result<_>>().unwrap();
        for name in triggers {
            assert!(
                name.bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
            );
            db.execute_batch(&format!("DROP TRIGGER {name};")).unwrap();
        }
        db.execute_batch(
            "ALTER TABLE metadata DROP COLUMN automation_archive_sequence; PRAGMA user_version=13;",
        )
        .unwrap();
    }

    #[test]
    fn automation_archive_clock_migration_rolls_back_ddl_and_preserves_legacy_data() {
        let root = tempfile::tempdir().unwrap();
        let database = root.path().join("registry.sqlite3");
        let mut registry = Registry::at(&database).unwrap();
        registry
            .apply(&ModelEdit::OpenProject {
                repository: root.path().into(),
                name: "Preserved legacy project".into(),
                kind: ProjectKind::Folder,
            })
            .unwrap();
        remove_draft_clock(&registry.db);
        let table: String = registry
            .db
            .query_row(
                "SELECT sql FROM sqlite_master WHERE name='shared_views'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        registry
            .db
            .execute_batch("DROP TABLE shared_views;")
            .unwrap();
        let before: (String, u64) = registry
            .db
            .query_row(
                "SELECT identity,sequence FROM metadata WHERE key=0",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert!(
            migrate(&registry.db).is_err(),
            "missing required projection table refuses the whole checkpoint"
        );
        assert_eq!(
            registry
                .db
                .pragma_query_value::<u32, _>(None, "user_version", |row| row.get(0))
                .unwrap(),
            13
        );
        assert!(!registry.db.query_row("SELECT EXISTS(SELECT 1 FROM pragma_table_info('metadata') WHERE name='automation_archive_sequence')",[],|row|row.get::<_,bool>(0)).unwrap());
        assert!(!registry.db.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='trigger' AND name LIKE 'automation_archive_%')",[],|row|row.get::<_,bool>(0)).unwrap());
        assert_eq!(
            registry
                .db
                .query_row(
                    "SELECT identity,sequence FROM metadata WHERE key=0",
                    [],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, u64>(1)?))
                )
                .unwrap(),
            before
        );
        assert_eq!(
            registry
                .db
                .query_row("SELECT name FROM projects", [], |row| row
                    .get::<_, String>(0))
                .unwrap(),
            "Preserved legacy project"
        );
        registry.db.execute_batch(&table).unwrap();
        migrate(&registry.db).unwrap();
        assert_eq!(
            registry.automation_archive_sequence().unwrap(),
            0,
            "legacy records need no rewrite/backfill to discover their first page"
        );
        assert_eq!(
            registry
                .db
                .pragma_query_value::<u32, _>(None, "user_version", |row| row.get(0))
                .unwrap(),
            14
        );
    }

    #[test]
    fn automation_archive_clock_rechecks_future_and_compatible_second_connection_versions() {
        let root = tempfile::tempdir().unwrap();
        let database = root.path().join("registry.sqlite3");
        let registry = Registry::at(&database).unwrap();
        remove_draft_clock(&registry.db);
        let observed: u32 = registry
            .db
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(observed, 13);
        let competing = Connection::open(&database).unwrap();
        let future = SCHEMA_VERSION + 1;
        competing
            .execute_batch(&format!(
                "BEGIN IMMEDIATE; PRAGMA user_version={future}; COMMIT;"
            ))
            .unwrap();
        assert!(
            migrate(&registry.db)
                .unwrap_err()
                .contains("newer than supported")
        );
        assert_eq!(
            registry
                .db
                .pragma_query_value::<u32, _>(None, "user_version", |row| row.get(0))
                .unwrap(),
            future
        );
        assert!(!registry.db.query_row("SELECT EXISTS(SELECT 1 FROM pragma_table_info('metadata') WHERE name='automation_archive_sequence')",[],|row|row.get::<_,bool>(0)).unwrap());
        competing.pragma_update(None, "user_version", 13).unwrap();
        migrate(&competing).unwrap();
        competing
            .execute(
                "UPDATE metadata SET automation_archive_sequence=41 WHERE key=0",
                [],
            )
            .unwrap();
        migrate(&registry.db).unwrap();
        assert_eq!(
            registry.automation_archive_sequence().unwrap(),
            41,
            "a stale opener preserves the compatible checkpoint's later mutations"
        );
        assert_eq!(
            registry
                .db
                .pragma_query_value::<u32, _>(None, "user_version", |row| row.get(0))
                .unwrap(),
            14
        );
    }

    #[test]
    fn automation_archive_clock_overflow_refuses_projection_mutation_atomically() {
        let root = tempfile::tempdir().unwrap();
        let mut registry = Registry::at(&root.path().join("registry.sqlite3")).unwrap();
        registry
            .apply(&ModelEdit::OpenProject {
                repository: root.path().into(),
                name: "Clock limit".into(),
                kind: ProjectKind::Folder,
            })
            .unwrap();
        registry
            .db
            .execute(
                "UPDATE metadata SET automation_archive_sequence=?1 WHERE key=0",
                [i64::MAX],
            )
            .unwrap();
        let before = registry.model().unwrap();
        let error = registry
            .apply(&ModelEdit::SeedView {
                scope: crate::agent_execution::Placement::Worktree {
                    repository: root.path().into(),
                    path: root.path().into(),
                },
                panes: vec![],
                closed: vec![],
            })
            .unwrap_err();
        assert!(!error.is_empty());
        assert_eq!(
            registry.model().unwrap(),
            before,
            "clock overflow cannot partially register a new display or wrap its authority"
        );
    }
}
