//! One admitted writer owns the cumulative schema19 upgrade. Exact schemas17
//! and18 preserve their issued authority before adding explicit refusal state.
//! Private forks13–16 remain unsupported: their numbers prove no table set.
use super::{SCHEMA_VERSION, agent_locations, automation_extensions_schema, migrate_legacy};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior};

const TABLES: [(&str, &str); 5] = [
    ("agent_locations", "CREATE TABLE agent_locations (
        agent TEXT PRIMARY KEY REFERENCES agents(id) ON DELETE CASCADE,
        repository TEXT NOT NULL, directory TEXT NOT NULL, blocked_directory TEXT
    ) STRICT"),
    ("session_locations", "CREATE TABLE session_locations (
        session TEXT PRIMARY KEY, source TEXT NOT NULL, repository TEXT,
        directory TEXT NOT NULL, approved TEXT, process TEXT,
        blocked_directory TEXT, refusal TEXT, previous_sources TEXT NOT NULL DEFAULT '[]'
    ) STRICT"),
    ("native_actor_cursors", "CREATE TABLE native_actor_cursors (
        actor TEXT PRIMARY KEY, owner TEXT NOT NULL,
        sequence INTEGER NOT NULL CHECK(sequence > 0), digest BLOB NOT NULL
    ) STRICT"),
    ("shared_view_terminal_sources", "CREATE TABLE shared_view_terminal_sources (
        pane TEXT PRIMARY KEY REFERENCES shared_view_panes(id) ON DELETE CASCADE,
        source TEXT NOT NULL, ended TEXT NOT NULL
    ) STRICT"),
    ("session_observer_owners", "CREATE TABLE session_observer_owners (
        session TEXT PRIMARY KEY, instance TEXT NOT NULL, stamp TEXT NOT NULL, source TEXT NOT NULL
    ) STRICT"),
];

fn version(db: &Connection) -> Result<u32, String> {
    db.pragma_query_value(None, "user_version", |row| row.get(0))
        .map_err(|error| error.to_string())
}

fn admit(current: u32) -> Result<(), String> {
    admit_for(current, SCHEMA_VERSION)
}

fn admit_for(current: u32, supported: u32) -> Result<(), String> {
    match current {
        0..=12 => Ok(()),
        17 if supported >= 17 => Ok(()),
        18 if supported >= 18 => Ok(()),
        current if current == supported => Ok(()),
        13..=16 => Err(format!("workspace registry schema {current} is an unpublished ambiguous fork; this build cannot migrate it safely")),
        _ => Err(format!("workspace registry schema {current} is newer than supported schema {supported}")),
    }
}

fn definition(db: &Connection, table: &str) -> Result<Option<String>, String> {
    db.query_row("SELECT sql FROM sqlite_schema WHERE type='table' AND name=?1", [table], |row| row.get(0))
        .optional().map_err(|error| error.to_string())
}

// Only whitespace/case outside quoted tokens is insignificant. Identifiers
// and default literals must never lose substrings while validating authority.
pub(super) fn normalized(sql: &str) -> String {
    let mut result = String::new();
    let mut quoted = None;
    let mut chars = sql.chars().peekable();
    while let Some(character) = chars.next() {
        if let Some(end) = quoted {
            result.push(character);
            if character == end {
                if chars.peek() == Some(&end) {
                    result.push(chars.next().expect("peeked quoted escape"));
                } else {
                    quoted = None;
                }
            }
        } else if matches!(character, '\'' | '"' | '`' | '[') {
            quoted = Some(if character == '[' { ']' } else { character });
            result.push(character);
        } else if !character.is_ascii_whitespace() && character != ';' {
            result.push(character.to_ascii_lowercase());
        }
    }
    result
}

fn validate_selected(db: &Connection, current: u32) -> Result<(), String> {
    for (table, expected) in TABLES {
        let actual = definition(db, table)?;
        if current <= 12 {
            if actual.is_some() {
                return Err(format!("published registry schema {current} unexpectedly contains private table {table}; refusing ambiguous partial migration"));
            }
        } else if actual.as_deref().map(normalized) != Some(normalized(expected)) {
            return Err(format!("registry schema {current} has missing or incompatible table {table}; refusing partial migration"));
        }
    }
    Ok(())
}

/// This guard precedes persistent journal configuration. Writer admission below
/// repeats it after locking; this first read is never upgrade authority.
pub(super) fn check_readonly(db: &Connection) -> Result<(), String> {
    check_readonly_for(db, SCHEMA_VERSION)
}

fn check_readonly_for(db: &Connection, supported: u32) -> Result<(), String> {
    let current = version(db)?;
    admit_for(current, supported)?;
    validate_selected(db, current)?;
    automation_extensions_schema::validate(db,current)
}

/// A previously opened Registry is not write admission. Every future-domain
/// mutation rechecks the exact current checkpoint/layout under its own writer
/// transaction, so another connection cannot move it past this binary's cap.
/// This never migrates or repairs a predecessor, and cannot be called with an
/// autocommit Connection in place of the transaction that owns the mutation.
pub(super) fn require_current_write(tx:&rusqlite::Transaction<'_>)->Result<(),String> {
    let current=version(tx)?;
    if current!=SCHEMA_VERSION {
        return Err(format!("Workspace registry schema {current} is not this writer's required checkpoint {SCHEMA_VERSION}; recover the matching store before retrying the original operation"));
    }
    validate_selected(tx,current)?;
    automation_extensions_schema::validate(tx,current)
}

#[cfg(test)]
pub(super) fn migrate(db: &Connection) -> Result<(), String> {
    migrate_after_read(db, || Ok(()))
}

#[cfg(test)]
pub(super) fn migrate_after_read(db: &Connection, after_read: impl FnOnce() -> Result<(), String>) -> Result<(), String> {
    migrate_with_hooks(db, after_read, |_| Ok(()))
}

/// The constructor carries its real read transaction across read-write open.
/// Release it only after this connection owns the writer reservation: keeping
/// it through COMMIT would block our own rollback-journal writer.
pub(super) fn migrate_with_admission(db: &Connection, inspection: Connection,
    after_read: impl FnOnce() -> Result<(), String>) -> Result<(), String> {
    migrate_with_admission_hooks(db, after_read, |_| Ok(()), move || {
        inspection.execute_batch("ROLLBACK").map_err(|error| error.to_string())?;
        drop(inspection);
        Ok(())
    })
}

#[cfg(test)]
fn migrate_with_hooks(db: &Connection, after_read: impl FnOnce() -> Result<(), String>, after_table: impl FnMut(usize) -> Result<(), String>) -> Result<(), String> {
    migrate_with_admission_hooks(db, after_read, after_table, || Ok(()))
}

fn migrate_with_admission_hooks(db: &Connection, after_read: impl FnOnce() -> Result<(), String>, mut after_table: impl FnMut(usize) -> Result<(), String>, after_writer: impl FnOnce() -> Result<(), String>) -> Result<(), String> {
    check_readonly(db)?;
    after_read()?;
    let tx = rusqlite::Transaction::new_unchecked(db, TransactionBehavior::Immediate)
        .map_err(|error| error.to_string())?;
    after_writer()?;
    let current = version(&tx)?;
    admit(current)?;
    validate_selected(&tx, current)?;
    automation_extensions_schema::validate(&tx,current)?;
    if current == SCHEMA_VERSION {
        return tx.commit().map_err(|error| error.to_string());
    }
    if current <= 12 {
        migrate_legacy(&tx, current, 12)?;
        for (index, (table, ddl)) in TABLES.into_iter().enumerate() {
            tx.execute_batch(ddl).map_err(|error| error.to_string())?;
            if table == "agent_locations" {
                // Only immutable accepted targets are copied. No legacy process,
                // inode, cursor, observer stamp or ended-pane proof is fabricated.
                agent_locations::backfill(&tx)?;
            }
            after_table(index)?;
        }
    }
    automation_extensions_schema::apply(&tx)?;
    // Schema17 has exactly these five tables. Its issued-source JSON remains
    // readable; only new schema18 writers may emit Located provenance. Keep
    // every old row and all authority unchanged rather than rebuilding tables.
    validate_selected(&tx, SCHEMA_VERSION)?;
    after_table(TABLES.len())?;
    tx.pragma_update(None, "user_version", SCHEMA_VERSION).map_err(|error| error.to_string())?;
    tx.commit().map_err(|error| error.to_string())
}

#[cfg(test)]
pub(super) fn discard_selected_for_legacy_fixture(db: &Connection) {
    automation_extensions_schema::discard_for_legacy_fixture(db);
    for (table, _) in TABLES {
        db.execute_batch(&format!("DROP TABLE IF EXISTS {table}")).unwrap();
    }
}

/// Exact stored values and definitions, excluding the separate version header.
#[cfg(test)]
pub(super) fn stored_contents(db: &Connection) -> Vec<(String, String, Vec<Vec<rusqlite::types::Value>>)> {
    let mut query = db.prepare("SELECT name,sql FROM sqlite_schema WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name").unwrap();
    let tables = query.query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))
        .unwrap().collect::<Result<Vec<_>, _>>().unwrap();
    tables.into_iter().map(|(name, ddl)| {
        let mut query = db.prepare(&format!("SELECT * FROM \"{}\"", name.replace('"', "\"\""))).unwrap();
        let mut rows = query.query_map([], |row| (0..row.as_ref().column_count())
            .map(|column| row.get::<_, rusqlite::types::Value>(column)).collect::<rusqlite::Result<Vec<_>>>())
            .unwrap().collect::<Result<Vec<_>, _>>().unwrap();
        rows.sort_by_key(|row| format!("{row:?}"));
        (name, ddl, rows)
    }).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::model::Registry;

    fn legacy(path: &std::path::Path, checkpoint: u32) {
        let db = Connection::open(path).unwrap();
        db.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        let tx = db.unchecked_transaction().unwrap();
        migrate_legacy(&tx, 0, checkpoint).unwrap();
        tx.execute("INSERT INTO projects(repository,name,ordinal,kind) VALUES('/fixture','preserved',1,'folder')", []).unwrap();
        tx.pragma_update(None, "user_version", checkpoint).unwrap();
        tx.commit().unwrap();
    }

    fn image(path: &std::path::Path) -> Vec<u8> {
        std::fs::read(path).unwrap()
    }

    #[test]
    fn every_published_checkpoint_upgrades_and_reopens_without_new_authority() {
        for checkpoint in 0..=12 {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("registry.sqlite3");
            legacy(&path, checkpoint);
            if checkpoint<=1 {
                let predecessor=Connection::open(&path).unwrap();
                let notification_table:bool=predecessor.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name='automation_notifications')",[],|row|row.get(0)).unwrap();
                assert!(!notification_table,"published predecessor {checkpoint} must not precreate private19 notification authority");
            }
            let registry = Registry::at(&path).unwrap();
            let discovery:(Option<String>,Option<String>)=registry.db.query_row("SELECT checkout,primary_checkout FROM projects WHERE repository='/fixture'",[],|row|Ok((row.get(0)?,row.get(1)?))).unwrap();
            assert_eq!(discovery,(None,None),"legacy checkpoint {checkpoint} must not infer discovery from repository identity");
            assert_eq!(registry.db.query_row("SELECT count(*) FROM sqlite_schema WHERE type='table' AND name='automation_notifications'",[],|row|row.get::<_,u64>(0)).unwrap(),1);

            assert_eq!(version(&registry.db).unwrap(), SCHEMA_VERSION, "from {checkpoint}");
            assert_eq!(registry.db.query_row("SELECT name FROM projects WHERE repository='/fixture'", [], |row| row.get::<_, String>(0)).unwrap(), "preserved");
            let identity: String = registry.db.query_row("SELECT identity FROM metadata", [], |row| row.get(0)).unwrap();
            for (table, _) in TABLES {
                let count: i64 = registry.db.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| row.get(0)).unwrap();
                assert_eq!(count, 0, "no guessed authority in {table} from {checkpoint}");
            }
            drop(registry);
            for _ in 0..2 {
                let registry = Registry::at(&path).unwrap();
                assert_eq!(version(&registry.db).unwrap(), SCHEMA_VERSION);
                assert_eq!(registry.db.query_row("SELECT identity FROM metadata", [], |row| row.get::<_, String>(0)).unwrap(), identity);
            }
        }
    }

    #[test]
    fn exact_schema17_interruption_keeps_rows_and_checkpoint_until_final_stamp() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("registry.sqlite3");
        let registry = Registry::at(&path).unwrap();
        registry.db.execute("INSERT INTO projects(repository,name,ordinal,kind) VALUES('/retained-missing-metadata','original custom name',7,'folder')",[]).unwrap();
        automation_extensions_schema::discard_for_legacy_fixture(&registry.db);
        registry.db.pragma_update(None, "user_version", 17).unwrap();
        drop(registry);
        let before = image(&path);
        let db = Connection::open(&path).unwrap();
        let contents = stored_contents(&db);
        let error = migrate_with_hooks(&db, || Ok(()), |step| {
            assert_eq!(step, TABLES.len(), "schema17 must never recreate or backfill a selected table");
            assert_eq!(version(&db).unwrap(), 17, "stamp must be the final write");
            Err("injected before final checkpoint".into())
        }).unwrap_err();
        assert!(error.contains("injected before final checkpoint"));
        assert_eq!(version(&db).unwrap(), 17);
        assert_eq!(stored_contents(&db), contents);
        drop(db);
        assert_eq!(image(&path), before);
        let registry = Registry::at(&path).unwrap();
        assert_eq!(version(&registry.db).unwrap(), SCHEMA_VERSION);
        let migrated=stored_contents(&registry.db);
        for (name,ddl,rows) in contents {
            let (_,new_ddl,new_rows)=migrated.iter().find(|(table,_,_)|table==&name).expect("preexisting table retained");
            if name=="metadata" {
                let old_columns=rows.first().map(Vec::len).unwrap_or(0);
                let projected:Vec<_>=new_rows.iter().map(|row|row[..old_columns].to_vec()).collect();
                assert_eq!(projected,rows,"preexisting metadata authority retained");
            } else if name=="projects" {
                automation_extensions_schema::assert_checkout_only_extension(&ddl,&rows,new_ddl,new_rows);
            } else {
                assert_eq!(new_ddl,&ddl,"preexisting {name} layout retained");
                assert_eq!(new_rows,&rows,"preexisting {name} rows retained");
            }
        }
    }

    #[test]
    fn schema17_reader_policy_refuses_schema18_before_layout_or_payload_reads() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("registry.sqlite3");
        let registry = Registry::at(&path).unwrap();
        automation_extensions_schema::discard_for_legacy_fixture(&registry.db);
        registry.db.pragma_update(None,"user_version",18).unwrap();
        registry.db.set_db_config(rusqlite::config::DbConfig::SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE, true).unwrap();
        registry.db.execute_batch("PRAGMA wal_autocheckpoint=0;").unwrap();
        registry.db.execute("UPDATE metadata SET sequence=sequence+1", []).unwrap();
        drop(registry);
        let wal = std::path::PathBuf::from(format!("{}-wal", path.display()));
        let before = (image(&path), image(&wal));
        assert!(!before.1.is_empty());
        let db = Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        // Exercise the same admission guard with the frozen schema17 reader
        // ceiling. This is a policy fixture, not execution of an old binary.
        let error = check_readonly_for(&db, 17).unwrap_err();
        assert!(error.contains("schema 18 is newer than supported schema 17"));
        drop(db);
        assert_eq!((image(&path), image(&wal)), before);
    }

    #[test]
    fn unpublished_and_future_versions_preserve_database_bytes() {
        for checkpoint in [13, 14, 15, 16, 20, 99] {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("registry.sqlite3");
            legacy(&path, 12);
            let db = Connection::open(&path).unwrap();
            db.pragma_update(None, "user_version", checkpoint).unwrap();
            drop(db);
            let before = image(&path);
            assert!(Registry::at(&path).is_err(), "version {checkpoint}");
            assert_eq!(image(&path), before, "refused version {checkpoint}");
        }
    }

    #[test]
    fn rejected_wal_databases_keep_primary_and_committed_wal_bytes() {
        for checkpoint in [13, 16, 20] {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("registry.sqlite3");
            legacy(&path, 12);
            let db = Connection::open(&path).unwrap();
            db.set_db_config(rusqlite::config::DbConfig::SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE, true).unwrap();
            db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0;").unwrap();
            db.pragma_update(None, "user_version", checkpoint).unwrap();
            drop(db);
            let wal = std::path::PathBuf::from(format!("{}-wal", path.display()));
            let before = (image(&path), image(&wal));
            assert!(!before.1.is_empty(), "fixture must retain actual committed WAL frames");
            assert!(Registry::at(&path).is_err());
            assert_eq!((image(&path), image(&wal)), before, "version {checkpoint}");
        }
    }

    #[test]
    fn constructor_read_pin_prevents_foreign_future_commit_and_preserves_its_journal() {
        for checkpoint in [13, 19] {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("registry.sqlite3");
            legacy(&path, 12);
            let journal = std::path::PathBuf::from(format!("{}-journal", path.display()));
            let writer = Connection::open(&path).unwrap();
            writer.busy_timeout(std::time::Duration::ZERO).unwrap();
            let mut expected = None;
            let result = Registry::at_with_admission_hook(&path,
                |db, inspection| {
                    db.busy_timeout(std::time::Duration::ZERO).unwrap();
                    migrate_with_admission(db, inspection, || Ok(()))
                },
                || {
                    // This genuine writer begins AFTER constructor read-only
                    // admission, in the old unprotected open/read gap. It can
                    // create a rollback journal but cannot commit a private or
                    // future schema while our admitted SHARED read survives.
                    writer.execute_batch("BEGIN IMMEDIATE").unwrap();
                    writer.pragma_update(None, "user_version", checkpoint).unwrap();
                    writer.execute("UPDATE projects SET name='foreign pending writer'", []).unwrap();
                    let error = writer.execute_batch("COMMIT").unwrap_err();
                    assert!(matches!(error, rusqlite::Error::SqliteFailure(code, _)
                        if code.code == rusqlite::ErrorCode::DatabaseBusy));
                    assert!(!writer.is_autocommit(), "failed commit must retain the real writer");
                    let bytes = (image(&path), image(&journal));
                    assert!(!bytes.1.is_empty(), "fixture must own an actual rollback journal");
                    expected = Some(bytes);
                    Ok(())
                });
            assert!(result.is_err(), "the foreign writer still owns admission");
            assert_eq!((image(&path), image(&journal)), expected.unwrap(),
                "rejected constructor must not recover or change the foreign journal");
            writer.execute_batch("ROLLBACK").unwrap();
            assert_eq!(version(&writer).unwrap(), 12);
            // Read authority must be released on refusal. A fresh constructor
            // also proves the successful path releases it before its own
            // rollback-journal COMMIT rather than blocking itself indefinitely.
            let registry = Registry::at(&path).unwrap();
            assert_eq!(version(&registry.db).unwrap(), SCHEMA_VERSION);
            assert_eq!(registry.db.query_row("SELECT name FROM projects WHERE repository='/fixture'", [],
                |row| row.get::<_, String>(0)).unwrap(), "preserved");
        }
    }

    #[test]
    fn stale_writer_rejection_preserves_retained_wal_through_the_actual_constructor() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("registry.sqlite3");
        legacy(&path, 12);
        let wal = std::path::PathBuf::from(format!("{}-wal", path.display()));
        let seed = Connection::open(&path).unwrap();
        seed.set_db_config(rusqlite::config::DbConfig::SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE, true).unwrap();
        seed.execute_batch("PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0;").unwrap();
        seed.execute("UPDATE projects SET name='retained before admission'", []).unwrap();
        drop(seed);
        let mut expected = None;
        let result = Registry::at_with_migration(&path, |db, inspection| {
            migrate_with_admission(db, inspection, || {
                let writer = Connection::open(&path).unwrap();
                writer.set_db_config(rusqlite::config::DbConfig::SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE, true).unwrap();
                let tx = writer.unchecked_transaction().unwrap();
                tx.pragma_update(None, "user_version", 20).unwrap();
                tx.execute("UPDATE projects SET name='newer writer owns this image'", []).unwrap();
                tx.commit().unwrap();
                drop(writer);
                expected = Some((image(&path), image(&wal)));
                Ok(())
            })
        });
        assert!(result.is_err());
        let expected = expected.expect("the second writer must run after outer admission");
        assert!(!expected.1.is_empty());
        assert_eq!((image(&path), image(&wal)), expected);
    }

    #[test]
    fn rejected_hot_journal_is_not_recovered_by_constructor_inspection() {
        const CHILD: &str = "CAGENTS_REGISTRY_HOT_JOURNAL_FIXTURE";
        if let Some(path) = std::env::var_os(CHILD) {
            let path = std::path::PathBuf::from(path);
            let db = Connection::open(&path).unwrap();
            db.execute_batch("PRAGMA cache_size=1; PRAGMA cache_spill=ON; BEGIN IMMEDIATE;
                CREATE TABLE spill (value BLOB);
                WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<128)
                INSERT INTO spill SELECT randomblob(4096) FROM n;").unwrap();
            std::fs::write(path.with_extension("ready"), b"ready").unwrap();
            loop { std::thread::sleep(std::time::Duration::from_secs(1)); }
        }
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("registry.sqlite3");
        legacy(&path, 12);
        let db = Connection::open(&path).unwrap();
        db.pragma_update(None, "user_version", 20).unwrap();
        drop(db);
        struct Child(std::process::Child);
        impl Drop for Child {
            fn drop(&mut self) { let _ = self.0.kill(); let _ = self.0.wait(); }
        }
        let mut child = Child(std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "host::model::schema::tests::rejected_hot_journal_is_not_recovered_by_constructor_inspection", "--nocapture"])
            .env(CHILD, &path).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null())
            .spawn().unwrap());
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !path.with_extension("ready").exists() {
            assert!(child.0.try_wait().unwrap().is_none(), "fixture writer exited before spilling");
            assert!(std::time::Instant::now() < deadline, "fixture writer never spilled");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        child.0.kill().unwrap();
        child.0.wait().unwrap();
        let journal = std::path::PathBuf::from(format!("{}-journal", path.display()));
        let before = (image(&path), image(&journal));
        assert!(!before.1.is_empty(), "fixture requires an actual hot rollback journal");
        assert!(Registry::at(&path).is_err());
        assert_eq!((image(&path), image(&journal)), before);
    }

    #[test]
    fn retained_writer_refuses_future_or_partial_schema_before_mutating_original_rows() {
        for future in [false,true] {
            let directory=tempfile::tempdir().unwrap();let path=directory.path().join("registry.sqlite3");
            let mut registry=Registry::at(&path).unwrap();
            registry.db.execute("INSERT INTO projects(repository,name,ordinal,kind) VALUES('/retained','original pending operation',1,'folder')",[]).unwrap();
            let other=Connection::open(&path).unwrap();
            if future {other.pragma_update(None,"user_version",20).unwrap();}
            else {other.execute_batch("DROP INDEX created_start_requirements_card_phase").unwrap();}
            let before=stored_contents(&registry.db);
            let tx=registry.db.transaction_with_behavior(TransactionBehavior::Immediate).unwrap();
            let guarded=(||->Result<(),String>{
                require_current_write(&tx)?;
                tx.execute("UPDATE projects SET name='must never be published by the old writer'",[]).map_err(|error|error.to_string())?;
                Ok(())
            })();
            assert!(guarded.is_err(),"the retained handle must re-admit its actual write transaction");
            drop(tx);
            assert_eq!(stored_contents(&registry.db),before);
            assert_eq!(version(&registry.db).unwrap(),if future{20}else{SCHEMA_VERSION});
        }
    }

    #[test]
    fn ambiguous_partial_layouts_are_refused_without_repair_or_writes() {
        for checkpoint in [12, 17, SCHEMA_VERSION] {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("registry.sqlite3");
            legacy(&path, 12);
            let db = Connection::open(&path).unwrap();
            db.execute_batch(TABLES[0].1).unwrap();
            db.pragma_update(None, "user_version", checkpoint).unwrap();
            drop(db);
            let before = image(&path);
            assert!(Registry::at(&path).is_err());
            assert_eq!(image(&path), before);
        }
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("registry.sqlite3");
        let registry = Registry::at(&path).unwrap();
        registry.db.execute_batch("ALTER TABLE session_observer_owners ADD COLUMN guessed TEXT").unwrap();
        drop(registry);
        let before = image(&path);
        assert!(Registry::at(&path).is_err());
        assert_eq!(image(&path), before);
    }

    #[test]
    fn malformed_identifiers_and_default_literals_are_refused_unchanged() {
        let cases = [
            ("session_observer_owners", TABLES[4].1.replace("instance TEXT", "instanceifnotexists TEXT")),
            ("session_locations", TABLES[1].1.replace("DEFAULT '[]'", "DEFAULT '[ ]'")),
            ("agent_locations", TABLES[0].1.replace("blocked_directory", "blocKed_directory")),
        ];
        for (table, ddl) in cases {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("registry.sqlite3");
            let registry = Registry::at(&path).unwrap();
            registry.db.execute_batch(&format!("DROP TABLE {table}; {ddl}")).unwrap();
            drop(registry);
            let before = image(&path);
            assert!(Registry::at(&path).is_err(), "malformed {table}");
            assert_eq!(image(&path), before);
        }
    }

    #[test]
    fn writer_admission_rechecks_after_another_connection_advances() {
        for checkpoint in [13, 16, 20] {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("registry.sqlite3");
            legacy(&path, 12);
            let db = Connection::open(&path).unwrap();
            let writer = Connection::open(&path).unwrap();
            let mut after_advance = Vec::new();
            let result = migrate_after_read(&db, || {
                let tx = writer.unchecked_transaction().unwrap();
                tx.pragma_update(None, "user_version", checkpoint).unwrap();
                tx.commit().unwrap();
                after_advance = image(&path);
                Ok(())
            });
            assert!(result.is_err());
            drop(writer);
            drop(db);
            assert_eq!(image(&path), after_advance);
        }
    }

    #[test]
    fn every_interrupted_selected_step_rolls_back_to_the_same_published_image() {
        for fail_at in 0..=TABLES.len() {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("registry.sqlite3");
            legacy(&path, 12);
            let before = image(&path);
            let db = Connection::open(&path).unwrap();
            let error = migrate_with_hooks(&db, || Ok(()), |step| {
                if step == fail_at { Err("injected migration interruption".into()) } else { Ok(()) }
            }).unwrap_err();
            assert!(error.contains("injected"));
            drop(db);
            assert_eq!(image(&path), before, "rollback step {fail_at}");
            let registry = Registry::at(&path).unwrap();
            assert_eq!(version(&registry.db).unwrap(), SCHEMA_VERSION);
        }
    }
}

#[cfg(test)]
mod backfill_tests {
    use super::*;
    use crate::agent_execution::{Agent, Origin, Spec, Target, Placement, Surface};
    use crate::model::{AgentId, ChannelId};

    #[test]
    fn backfill_copies_only_accepted_targets_without_opening_replacement_paths() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("registry.sqlite3");
        let db = Connection::open(&path).unwrap();
        let tx = db.unchecked_transaction().unwrap();
        migrate_legacy(&tx, 0, 12).unwrap();
        let agent = Agent {
            name: None, id: AgentId::new(), origin: Origin::User,
            spec: Spec { provider: crate::agent::Provider::Claude, account: None,
                target: Target {repository: "/missing-original".into(), directory: "/missing-original/child".into(), placement: None},
                surface: Surface::Chat, model: None, permission_mode: None, reasoning_effort: None },
            placement: Placement::Channel(ChannelId::new()), session: None, console: None, created_ms: 1, closed: false,
        };
        let record = serde_json::to_string(&agent).unwrap();
        tx.execute("INSERT INTO agents(id,origin,record,closed) VALUES(?1,?2,?3,0)",
            rusqlite::params![agent.id.to_string(), serde_json::to_string(&agent.origin).unwrap(), record]).unwrap();
        tx.execute("INSERT INTO agent_policies(agent,directory) VALUES(?1,NULL)", [agent.id.to_string()]).unwrap();
        tx.pragma_update(None, "user_version", 12).unwrap();
        tx.commit().unwrap();
        migrate(&db).unwrap();
        let copied: (String, String, Option<String>) = db.query_row("SELECT repository,directory,blocked_directory FROM agent_locations WHERE agent=?1", [agent.id.to_string()], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?))).unwrap();
        assert_eq!(copied, ("/missing-original".into(), "/missing-original/child".into(), None));
        assert_eq!(db.query_row("SELECT record FROM agents WHERE id=?1", [agent.id.to_string()], |row| row.get::<_, String>(0)).unwrap(), record);
        let policy: Option<String> = db.query_row("SELECT directory FROM agent_policies WHERE agent=?1", [agent.id.to_string()], |row| row.get(0)).unwrap();
        assert!(policy.is_none(), "a path cannot mint inode approval");
        for table in ["session_locations", "native_actor_cursors", "shared_view_terminal_sources", "session_observer_owners"] {
            assert_eq!(db.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| row.get::<_, i64>(0)).unwrap(), 0);
        }
    }
}
