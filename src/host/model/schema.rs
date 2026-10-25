//! Admission for the published registry format. This module owns no runtime
//! authority. Each published format is admitted before its migrations run.
//!
//! Admission serializes cooperating SQLite connections to the existing file.
//! The read transaction is not a physical identity proof for a separately
//! opened connection if an external actor replaces the database pathname.

use super::{BUSY_TIMEOUT, FILE_MODE, SCHEMA_VERSION};
use rusqlite::{Connection, OpenFlags, Transaction, TransactionBehavior, config::DbConfig};
use std::{
    fs::OpenOptions,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::Path,
    sync::{Mutex, OnceLock},
};

#[derive(Clone, Debug, PartialEq, Eq)]
struct Inspection {
    version: u32,
    cookie: i64,
    objects: Vec<(String, String, String, Vec<String>)>,
}

fn connect(path: &Path, readonly: bool) -> Result<Connection, String> {
    let access = if readonly {
        OpenFlags::SQLITE_OPEN_READ_ONLY
    } else {
        OpenFlags::SQLITE_OPEN_READ_WRITE
    };
    let db = Connection::open_with_flags(
        path,
        access | OpenFlags::SQLITE_OPEN_NO_MUTEX | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )
    .map_err(|e| e.to_string())?;
    // Set before the first SQL, including refusals. A retained handle can later
    // become stale, so do not re-enable checkpoint-on-close after admission.
    db.set_db_config(DbConfig::SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE, true)
        .map_err(|e| e.to_string())?;
    db.busy_timeout(BUSY_TIMEOUT).map_err(|e| e.to_string())?;
    Ok(db)
}

pub(super) fn open(path: &Path) -> Result<Connection, String> {
    prepare_file(path)?;
    open_existing(path)
}

fn prepare_file(path: &Path) -> Result<(), String> {
    // POSIX close(any descriptor for an inode) releases the process's fcntl
    // locks, even if another SQLite connection owns them. Never open/close an
    // existing database outside SQLite. Serialize only the creation prelude so
    // a new private file descriptor closes before another Registry constructor
    // in this process can open that newly created inode through SQLite.
    static CREATION: Mutex<()> = Mutex::new(());
    let _creation = CREATION
        .lock()
        .map_err(|_| "registry file creation lock was poisoned")?;
    if let Some(parent) = path.parent() {
        super::create_private_directory(parent).map_err(|e| e.to_string())?;
    }
    loop {
        match std::fs::symlink_metadata(path) {
            Ok(metadata) => {
                return if metadata.is_file() {
                    Ok(())
                } else {
                    Err("workspace registry is not a regular file".into())
                };
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.to_string()),
        }
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(FILE_MODE)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(path)
        {
            Ok(created) => {
                drop(created);
                return Ok(());
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e.to_string()),
        }
    }
}

fn open_existing(path: &Path) -> Result<Connection, String> {
    // A read transaction pins rollback-journal inspection until writer
    // admission. A hot journal must fail here, before a RW connection can
    // recover it. WAL writers may still advance; the writer reread detects it.
    let mut reader = connect(path, true)?;
    let pin = reader.transaction().map_err(|e| e.to_string())?;
    let inspected = inspect(&pin)?;
    // Every admitted published layout remains exact after later migrations.
    // Refuse damage before opening a writer can recreate missing objects.
    if inspected.version >= 12 {
        require_open_layout(&pin, inspected.version)?;
    }
    checkpoint("inspected")?;
    let mut db = connect(path, false)?;
    db.execute_batch("PRAGMA foreign_keys=ON; PRAGMA synchronous=FULL;")
        .map_err(|e| e.to_string())?;
    if db
        .pragma_query_value(None, "foreign_keys", |r| r.get::<_, i64>(0))
        .map_err(|e| e.to_string())?
        != 1
    {
        return Err("SQLite foreign key enforcement could not be enabled".into());
    }
    if db
        .pragma_query_value(None, "synchronous", |r| r.get::<_, i64>(0))
        .map_err(|e| e.to_string())?
        != 2
    {
        return Err("SQLite synchronous mode could not be set to FULL".into());
    }
    let tx = db
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|e| e.to_string())?;
    if inspect(&tx)? != inspected {
        return Err("workspace registry changed during admission".into());
    }
    let journal: String = tx
        .pragma_query_value(None, "journal_mode", |r| r.get(0))
        .map_err(|e| e.to_string())?;
    let convert = !journal.eq_ignore_ascii_case("wal");
    if convert {
        // Reserved writer ownership precedes releasing the reader. Once a
        // write takes EXCLUSIVE, exclusive locking retains that database lock
        // across COMMIT and the persistent journal-mode conversion below.
        tx.pragma_update(None, "locking_mode", "EXCLUSIVE")
            .map_err(|e| e.to_string())?;
    }
    pin.commit().map_err(|e| e.to_string())?;
    drop(reader);
    checkpoint("admitted")?;
    super::migrate_through(&tx, inspected.version, SCHEMA_VERSION)?;
    require_layout(&tx)?;
    super::channel_work::repair_missing_homes_in(&tx)?;
    // Also forces the rollback-mode EXCLUSIVE write before commit for an
    // already-current store. No future writer can enter the conversion gap.
    tx.pragma_update(None, "user_version", SCHEMA_VERSION)
        .map_err(|e| e.to_string())?;
    // Path-based chmod does not open/close an additional database descriptor.
    // External namespace replacement is outside this same-file admission scope.
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(FILE_MODE))
        .map_err(|e| e.to_string())?;
    tx.commit().map_err(|e| e.to_string())?;
    checkpoint("committed")?;
    if convert {
        // Keep the actual rollback-mode EXCLUSIVE lock acquired above, but
        // select NORMAL before the first WAL access. Entering WAL while the
        // policy is EXCLUSIVE creates a heap wal-index that cannot later be
        // changed to shared-memory NORMAL mode without leaving WAL again.
        db.pragma_update(None, "locking_mode", "NORMAL")
            .map_err(|e| e.to_string())?;
        let locking: String = db
            .pragma_query_value(Some("main"), "locking_mode", |r| r.get(0))
            .map_err(|e| e.to_string())?;
        if !locking.eq_ignore_ascii_case("normal") {
            return Err("SQLite could not select normal registry locking".into());
        }
        checkpoint("normal_before_wal")?;
        let mode: String = db
            .query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0))
            .map_err(|e| e.to_string())?;
        if !mode.eq_ignore_ascii_case("wal") {
            return Err(format!(
                "SQLite journal mode is `{mode}`, but WAL is required"
            ));
        }
        // End the exclusive-lock lifetime only after persistent configuration.
        let release = db.transaction().map_err(|e| e.to_string())?;
        require_current(&release)?;
        release.commit().map_err(|e| e.to_string())?;
    }
    Ok(db)
}

fn inspect(db: &Connection) -> Result<Inspection, String> {
    let version: u32 = db
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .map_err(|e| e.to_string())?;
    if version > SCHEMA_VERSION {
        return Err(format!(
            "workspace registry schema {version} is newer than supported schema {SCHEMA_VERSION}"
        ));
    }
    let cookie = db
        .pragma_query_value(None, "schema_version", |r| r.get(0))
        .map_err(|e| e.to_string())?;
    let mut statement = db.prepare("SELECT type,name,tbl_name,CASE WHEN length(CAST(sql AS BLOB))<=65536 THEN sql END FROM sqlite_schema WHERE name NOT GLOB 'sqlite_*' ORDER BY type,name LIMIT 129").map_err(|e| e.to_string())?;
    let rows = statement
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, Option<String>>(3)?,
            ))
        })
        .map_err(|e| e.to_string())?;
    let mut objects = Vec::new();
    for row in rows {
        let (kind, name, table, sql) = row.map_err(|e| e.to_string())?;
        let sql = sql.ok_or("workspace registry contains an unsupported schema object")?;
        objects.push((kind, name, table, tokens(&sql)?));
        if objects.len() > 128 {
            return Err("workspace registry schema exceeds its bound".into());
        }
    }
    Ok(Inspection {
        version,
        cookie,
        objects,
    })
}

type Layout = Vec<(String, String, String, Vec<String>)>;
fn published_layouts(version: u32) -> Result<&'static (Layout, Layout), String> {
    static TWELVE: OnceLock<Result<(Layout, Layout), String>> = OnceLock::new();
    static THIRTEEN: OnceLock<Result<(Layout, Layout), String>> = OnceLock::new();
    static FOURTEEN: OnceLock<Result<(Layout, Layout), String>> = OnceLock::new();
    static FIFTEEN: OnceLock<Result<(Layout, Layout), String>> = OnceLock::new();
    static SIXTEEN: OnceLock<Result<(Layout, Layout), String>> = OnceLock::new();
    static SEVENTEEN: OnceLock<Result<(Layout, Layout), String>> = OnceLock::new();
    static EIGHTEEN: OnceLock<Result<(Layout, Layout), String>> = OnceLock::new();
    let published = match version {
        12 => &TWELVE,
        13 => &THIRTEEN,
        14 => &FOURTEEN,
        15 => &FIFTEEN,
        16 => &SIXTEEN,
        17 => &SEVENTEEN,
        18 => &EIGHTEEN,
        _ => return Err(format!("unsupported published registry layout {version}")),
    };
    published
        .get_or_init(|| {
            let mut reference = Connection::open_in_memory().map_err(|e| e.to_string())?;
            let tx = reference.transaction().map_err(|e| e.to_string())?;
            // Derive the baseline from the same published bodies, without future
            // DDL. The unversioned project-kind upgrade predates header admission.
            reference_migration(&tx, version)?;
            let complete = inspect(&tx)?.objects;
            tx.execute_batch("ALTER TABLE projects DROP COLUMN kind;")
                .map_err(|e| e.to_string())?;
            Ok((complete, inspect(&tx)?.objects))
        })
        .as_ref()
        .map_err(Clone::clone)
}
fn require_layout(db: &Connection) -> Result<(), String> {
    if inspect(db)?.objects != published_layouts(SCHEMA_VERSION)?.0 {
        return Err(format!(
            "workspace registry layout does not match published schema {SCHEMA_VERSION}"
        ));
    }
    Ok(())
}
fn require_open_layout(db: &Connection, version: u32) -> Result<(), String> {
    let actual = inspect(db)?.objects;
    let (complete, legacy_kind) = published_layouts(version)?;
    if &actual != complete && &actual != legacy_kind {
        return Err(format!(
            "workspace registry layout does not match published schema {version}"
        ));
    }
    Ok(())
}

/// The historical writer is unavailable in this tree. Admit the explicit
/// compatibility shape consumed by the existing conversion, rather than drop
/// unknown columns, indexes or triggers along with its three legacy tables.
pub(super) fn require_legacy_views(db: &Connection) -> Result<(), String> {
    let definitions = [
        (
            "workspace_views",
            "CREATE TABLE workspace_views(channel TEXT PRIMARY KEY REFERENCES channels(id),revision INTEGER NOT NULL,panes TEXT NOT NULL,retired TEXT NOT NULL) STRICT",
        ),
        (
            "workspace_view_panes",
            "CREATE TABLE workspace_view_panes(id TEXT PRIMARY KEY,channel TEXT NOT NULL REFERENCES workspace_views(channel)) STRICT",
        ),
        (
            "workspace_view_edits",
            "CREATE TABLE workspace_view_edits(operation TEXT PRIMARY KEY,channel TEXT NOT NULL REFERENCES workspace_views(channel)) STRICT",
        ),
    ];
    let mut expected = definitions
        .iter()
        .map(|(name, sql)| {
            Ok((
                "table".to_owned(),
                name.to_string(),
                name.to_string(),
                tokens(sql)?,
            ))
        })
        .collect::<Result<Layout, String>>()?;
    expected.sort();
    let actual = inspect(db)?
        .objects
        .into_iter()
        .filter(|o| definitions.iter().any(|(name, _)| o.2 == *name))
        .collect::<Layout>();
    if actual != expected {
        return Err(
            "unsupported legacy workspace-view layout; the original store was not converted".into(),
        );
    }
    Ok(())
}

pub(super) fn require_current(db: &Connection) -> Result<(), String> {
    let actual = inspect(db)?;
    if actual.version != SCHEMA_VERSION {
        return Err(format!(
            "workspace registry schema {} requires reopening before edits",
            actual.version
        ));
    }
    require_layout(db)
}

// Tokenize, rather than deleting substrings: a column named
// `instanceifnotexists` must never become an `instance` column in validation.
fn tokens(sql: &str) -> Result<Vec<String>, String> {
    let bytes = sql.as_bytes();
    let mut i = 0;
    let mut out = Vec::new();
    while i < bytes.len() {
        if bytes[i].is_ascii_whitespace() {
            i += 1;
            continue;
        }
        let start = i;
        if matches!(bytes[i], b'\'' | b'"' | b'`' | b'[') {
            let end = if bytes[i] == b'[' { b']' } else { bytes[i] };
            i += 1;
            loop {
                if i >= bytes.len() {
                    return Err("unterminated registry schema token".into());
                }
                if bytes[i] == end {
                    i += 1;
                    if end != b']' && i < bytes.len() && bytes[i] == end {
                        i += 1;
                    } else {
                        break;
                    }
                } else {
                    i += 1;
                }
            }
            out.push(sql[start..i].to_owned());
        } else if bytes[i].is_ascii_alphanumeric() || matches!(bytes[i], b'_' | b'$') {
            i += 1;
            while i < bytes.len()
                && (bytes[i].is_ascii_alphanumeric() || matches!(bytes[i], b'_' | b'$'))
            {
                i += 1;
            }
            out.push(sql[start..i].to_ascii_lowercase());
        } else {
            i += sql[i..]
                .chars()
                .next()
                .ok_or("invalid registry schema token")?
                .len_utf8();
            out.push(sql[start..i].to_owned());
        }
    }
    let position = if out.first().is_some_and(|v| v == "create") {
        match out.get(1).map(String::as_str) {
            Some("table" | "index") => Some(2),
            Some("unique") if out.get(2).is_some_and(|v| v == "index") => Some(3),
            _ => None,
        }
    } else {
        None
    };
    if let Some(p) = position {
        if out
            .get(p..p + 3)
            .is_some_and(|v| v == ["if", "not", "exists"])
        {
            out.drain(p..p + 3);
        }
    }
    while out.last().is_some_and(|v| v == ";") {
        out.pop();
    }
    Ok(out)
}

fn reference_migration(tx: &Transaction<'_>, target: u32) -> Result<(), String> {
    #[cfg(test)]
    let previous = FAIL_STEP.with(|s| s.replace(None));
    let result = super::migrate_through(tx, 0, target);
    #[cfg(test)]
    FAIL_STEP.with(|s| s.set(previous));
    result
}

pub(super) fn migration_checkpoint(_step: u32) -> Result<(), String> {
    #[cfg(test)]
    if FAIL_STEP.with(|s| s.get()) == Some(_step) {
        return Err(format!("injected migration failure at {_step}"));
    }
    Ok(())
}
fn checkpoint(_stage: &str) -> Result<(), String> {
    #[cfg(test)]
    HOOK.with(|h| {
        if let Some(h) = h.borrow_mut().as_mut() {
            h(_stage)?;
        }
        Ok::<_, String>(())
    })?;
    Ok(())
}
#[cfg(test)]
thread_local! {
    static FAIL_STEP: std::cell::Cell<Option<u32>> = const { std::cell::Cell::new(None) };
    static HOOK: std::cell::RefCell<Option<Box<dyn FnMut(&str)->Result<(),String>>>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
mod tests {
    use super::super::Registry;
    use super::*;
    use crate::{
        model::{ChannelId, ChannelTitleSource, ProjectKind},
        workspace_model::ModelEdit,
    };
    use std::{cell::Cell, rc::Rc, time::Duration};

    fn legacy(path: &Path, version: u32) -> (String, ChannelId) {
        let mut db = Connection::open(path).unwrap();
        let tx = db.transaction().unwrap();
        super::super::migrate_through(&tx, 0, version).unwrap();
        let identity = uuid::Uuid::new_v4().to_string();
        let channel = ChannelId::new();
        tx.execute("UPDATE metadata SET identity=?1,sequence=7,next_project_ordinal=11,next_channel_ordinal=12", [&identity]).unwrap();
        tx.execute("INSERT INTO projects(repository,name,ordinal,kind) VALUES('/kept','My project',4,'folder')", []).unwrap();
        if version >= 3 {
            tx.execute("INSERT INTO channels(id,repository,title,ordinal,title_source) VALUES(?1,'/kept','My channel',5,1)", [channel.to_string()]).unwrap();
        } else {
            tx.execute("INSERT INTO channels(id,repository,title,ordinal) VALUES(?1,'/kept','My channel',5)", [channel.to_string()]).unwrap();
        }
        if version >= 17 {
            super::super::channel_work::created_in(&tx, channel).unwrap();
        }
        tx.pragma_update(None, "user_version", version).unwrap();
        tx.commit().unwrap();
        (identity, channel)
    }
    fn bytes(path: &Path) -> Vec<u8> {
        std::fs::read(path).unwrap()
    }
    fn wal(path: &Path) -> std::path::PathBuf {
        path.with_file_name(format!(
            "{}-wal",
            path.file_name().unwrap().to_str().unwrap()
        ))
    }
    fn hook(f: impl FnMut(&str) -> Result<(), String> + 'static) {
        HOOK.with(|h| *h.borrow_mut() = Some(Box::new(f)));
    }
    struct Clear;
    impl Drop for Clear {
        fn drop(&mut self) {
            HOOK.with(|h| *h.borrow_mut() = None);
            FAIL_STEP.with(|s| s.set(None));
        }
    }

    fn refuse_damaged_twelve(damage: &str, object: &str) {
        let _clear = Clear;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("damaged12.sqlite3");
        legacy(&path, 12);
        let db = Connection::open(&path).unwrap();
        db.execute_batch(damage).unwrap();
        drop(db);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
        let before = bytes(&path);
        let reached = Rc::new(Cell::new(false));
        let observed = Rc::clone(&reached);
        hook(move |_| {
            observed.set(true);
            Ok(())
        });
        let error = Registry::at(&path)
            .err()
            .expect("published12 damage must refuse");
        assert!(error.contains("published schema 12"), "{error}");
        assert!(
            !reached.get(),
            "refusal must precede the pre-writer inspected checkpoint"
        );
        assert_eq!(
            bytes(&path),
            before,
            "must not recreate objects or mint an identity"
        );
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o640
        );
        assert!(
            !wal(&path).exists(),
            "refusal must precede persistent WAL conversion"
        );
        let db = connect(&path, true).unwrap();
        assert_eq!(
            db.pragma_query_value(None, "user_version", |r| r.get::<_, u32>(0))
                .unwrap(),
            12
        );
        assert_eq!(
            db.query_row(
                "SELECT count(*) FROM sqlite_schema WHERE name=?1",
                [object],
                |r| r.get::<_, usize>(0)
            )
            .unwrap(),
            0
        );
        assert_eq!(
            db.query_row(
                "SELECT count(*) FROM sqlite_schema WHERE name='shared_view_terminal_sources'",
                [],
                |r| r.get::<_, usize>(0)
            )
            .unwrap(),
            0
        );
    }

    #[test]
    fn published_twelve_missing_table_refuses_before_writable_admission() {
        refuse_damaged_twelve("DROP TABLE navigation_orders;", "navigation_orders");
    }

    #[test]
    fn published_twelve_missing_index_refuses_before_writable_admission() {
        refuse_damaged_twelve(
            "DROP INDEX channels_by_repository;",
            "channels_by_repository",
        );
    }

    #[test]
    fn published_twelve_missing_metadata_table_cannot_mint_a_new_identity() {
        refuse_damaged_twelve("DROP TABLE metadata;", "metadata");
    }

    #[test]
    fn published_twelve_wal_damage_preserves_primary_wal_and_permissions() {
        let _clear = Clear;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("damaged12-wal.sqlite3");
        legacy(&path, 12);
        let writer = connect(&path, false).unwrap();
        writer
            .execute_batch("PRAGMA journal_mode=WAL; DROP TABLE navigation_orders;")
            .unwrap();
        drop(writer);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
        let before = (bytes(&path), bytes(&wal(&path)));
        assert!(!before.1.is_empty());
        let error = Registry::at(&path)
            .err()
            .expect("published12 WAL damage must refuse");
        assert!(error.contains("published schema 12"), "{error}");
        assert_eq!((bytes(&path), bytes(&wal(&path))), before);
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o640
        );
    }

    #[test]
    fn published_twelve_migrates_to_current_without_historical_terminal_proof() {
        use crate::model::{
            CardId, CardKind, LaunchRecipe, TerminalContinuity, TerminalExit, TerminalId,
            TerminalSession,
        };
        use crate::workspace_model::views::{Pane, Scope};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nonempty12.sqlite3");
        let (identity, channel) = legacy(&path, 12);
        let scope = Scope::Channel(channel);
        let scope_key = serde_json::to_string(&scope).unwrap();
        let recipe = LaunchRecipe {
            program: "/bin/sh".into(),
            arguments: Vec::new(),
            working_directory: "/kept".into(),
            environment: Default::default(),
            unset_environment: Default::default(),
        };
        let pane = Pane {
            id: CardId::new(),
            title: "Historical ended terminal".into(),
            user_title: None,
            pending_terminal_start: false,
            width_fraction: 1.0,
            scope: Some("/kept".into()),
            kind: CardKind::Terminal(TerminalSession {
                id: TerminalId::new(),
                launch: recipe,
                transcript_path: None,
                resume_hint: None,
                owned_codex_listener: None,
                recovery_origin: None,
                predecessor: None,
                continuity: TerminalContinuity::Exited {
                    last_exit: TerminalExit {
                        code: Some(0),
                        signal: None,
                        occurred_at_unix_ms: 1,
                    },
                },
            }),
        };
        let panes = serde_json::to_string(&vec![pane.clone()]).unwrap();
        let db = Connection::open(&path).unwrap();
        db.execute(
            "INSERT INTO shared_view_scopes(scope,channel) VALUES(?1,?2)",
            rusqlite::params![scope_key, channel.to_string()],
        )
        .unwrap();
        db.execute(
            "INSERT INTO shared_views VALUES(?1,9,?2,'[]')",
            rusqlite::params![scope_key, panes],
        )
        .unwrap();
        db.execute(
            "INSERT INTO shared_view_panes VALUES(?1,?2)",
            rusqlite::params![pane.id.to_string(), scope_key],
        )
        .unwrap();
        drop(db);
        let registry = Registry::at(&path).unwrap();
        require_current(&registry.db).unwrap();
        let model = registry.model().unwrap();
        assert_eq!(model.identity.to_string(), identity);
        assert_eq!(model.sequence, 7);
        assert_eq!(model.channels[0].id, channel);
        let view = model.views.iter().find(|view| view.scope == scope).unwrap();
        assert_eq!(view.revision, 9);
        assert_eq!(view.panes, vec![pane]);
        assert_eq!(
            registry
                .db
                .query_row(
                    "SELECT count(*) FROM shared_view_terminal_sources",
                    [],
                    |r| r.get::<_, usize>(0)
                )
                .unwrap(),
            0,
            "old ended descriptors cannot invent source provenance"
        );
        assert_eq!(
            registry
                .db
                .pragma_query_value(None, "foreign_keys", |r| r.get::<_, i64>(0))
                .unwrap(),
            1
        );
        assert_eq!(
            registry
                .db
                .pragma_query_value(None, "synchronous", |r| r.get::<_, i64>(0))
                .unwrap(),
            2
        );
        drop(registry);
        assert_eq!(Registry::at(&path).unwrap().model().unwrap(), model);
    }

    #[test]
    fn published_thirteen_missing_provenance_is_not_recreated() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("damaged13.sqlite3");
        legacy(&path, 13);
        let db = Connection::open(&path).unwrap();
        db.execute_batch("DROP TABLE shared_view_terminal_sources;")
            .unwrap();
        drop(db);
        let before = bytes(&path);
        assert!(
            Registry::at(&path)
                .err()
                .unwrap()
                .contains("published schema 13")
        );
        assert_eq!(bytes(&path), before);
        assert!(!wal(&path).exists());
    }

    #[test]
    fn published_native_actor_layout_damage_refuses_before_writable_admission() {
        let _clear = Clear;
        for (version, damage) in [
            (14, "DROP TABLE native_actor_cursors;"),
            (
                14,
                "ALTER TABLE native_actor_cursors RENAME COLUMN digest TO changed_digest;",
            ),
            (15, "DROP TABLE native_actor_retirements;"),
            (16, "DROP TABLE automation_lifecycle;"),
            (16, "DROP INDEX automation_lifecycle_scoped;"),
            (17, "DROP TABLE channel_work_operations;"),
            (17, "DROP TABLE channel_work_entries;"),
            (17, "DROP TABLE channel_work_homes;"),
            (18, "DROP TABLE channel_work_placement_epochs;"),
            (18, "DROP TABLE channel_work_placement_clock;"),
            (
                18,
                "ALTER TABLE channel_work_placement_clock RENAME COLUMN last_epoch TO changed_epoch;",
            ),
            (
                18,
                "ALTER TABLE channel_work_placement_epochs RENAME COLUMN epoch TO changed_epoch;",
            ),
            (17, "DROP INDEX channel_work_entries_order;"),
            (
                17,
                "ALTER TABLE channel_work_entries RENAME COLUMN bucket TO wrong_bucket;",
            ),
            (
                15,
                "ALTER TABLE native_actor_ownerships ADD COLUMN foreign_payload TEXT;",
            ),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("damaged-native-actor.sqlite3");
            legacy(&path, version);
            Connection::open(&path)
                .unwrap()
                .execute_batch(damage)
                .unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
            let before = bytes(&path);
            let reached = Rc::new(Cell::new(false));
            let observed = Rc::clone(&reached);
            hook(move |_| {
                observed.set(true);
                Ok(())
            });
            let error = Registry::at(&path)
                .err()
                .expect("damaged published layout must refuse");
            HOOK.with(|h| *h.borrow_mut() = None);
            assert!(
                error.contains(&format!("published schema {version}")),
                "{error}"
            );
            assert!(!reached.get(), "damage reached writable admission");
            assert_eq!(bytes(&path), before);
            assert!(!wal(&path).exists());
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o640
            );
        }
    }

    #[test]
    fn native_actor_schema_generations_preserve_existing_receipts_on_upgrade_and_reopen() {
        for version in [13, 14, 15, 16, 17] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("native-actor-generations.sqlite3");
            legacy(&path, version);
            let db = Connection::open(&path).unwrap();
            let tables = inspect(&db).unwrap().objects;
            assert_eq!(
                tables
                    .iter()
                    .any(|object| object.1 == "native_actor_cursors"),
                version >= 14
            );
            assert_eq!(
                tables
                    .iter()
                    .any(|object| object.1 == "native_actor_ownerships"),
                version >= 15
            );
            assert_eq!(
                tables
                    .iter()
                    .any(|object| object.1 == "native_actor_retirements"),
                version >= 15
            );
            if version >= 14 {
                db.execute(
                    "INSERT INTO native_actor_cursors VALUES('actor','original owner',9,x'0001ff')",
                    [],
                )
                .unwrap();
            }
            if version >= 15 {
                db.execute(
                    "INSERT INTO native_actor_ownerships VALUES('actor','original ownership')",
                    [],
                )
                .unwrap();
                db.execute(
                    "INSERT INTO native_actor_retirements VALUES('actor','original retirement')",
                    [],
                )
                .unwrap();
            }
            drop(db);
            for _ in 0..2 {
                let registry = Registry::at(&path).unwrap();
                require_current(&registry.db).unwrap();
                if version >= 14 {
                    let receipt: (String, u64, Vec<u8>) = registry.db.query_row("SELECT owner,sequence,digest FROM native_actor_cursors WHERE actor='actor'", [], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?))).unwrap();
                    assert_eq!(receipt, ("original owner".into(), 9, vec![0, 1, 255]));
                }
                if version >= 15 {
                    let receipts: (String, String) = registry.db.query_row("SELECT ownership,receipt FROM native_actor_ownerships JOIN native_actor_retirements USING(actor) WHERE actor='actor'", [], |row| Ok((row.get(0)?, row.get(1)?))).unwrap();
                    assert_eq!(
                        receipts,
                        ("original ownership".into(), "original retirement".into())
                    );
                }
            }
        }
    }

    #[test]
    fn every_published_version_preserves_nonempty_model_identity_and_ordinals() {
        for version in 0..=SCHEMA_VERSION {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("registry.sqlite3");
            let (identity, channel) = legacy(&path, version);
            let registry = Registry::at(&path).unwrap_or_else(|e| panic!("version {version}: {e}"));
            let model = registry.model().unwrap();
            assert_eq!(model.identity.to_string(), identity);
            assert_eq!(model.sequence, 7);
            assert_eq!(model.projects[0].name, "My project");
            assert_eq!(model.projects[0].kind, ProjectKind::Folder);
            assert_eq!(model.projects[0].ordinal, 4);
            assert_eq!(model.channels[0].id, channel);
            assert_eq!(model.channels[0].title, "My channel");
            assert_eq!(model.channels[0].ordinal, 5);
            let ordinals: (i64, i64) = registry
                .db
                .query_row(
                    "SELECT next_project_ordinal,next_channel_ordinal FROM metadata",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .unwrap();
            assert_eq!(ordinals, (11, 12));
            require_current(&registry.db).unwrap();
            drop(registry);
            assert_eq!(Registry::at(&path).unwrap().model().unwrap(), model);
        }
    }

    #[test]
    fn published_sixteen_channel_work_upgrade_preserves_home_without_inferring_prs() {
        use super::super::channel_work::TrustedResolvedOrigin;
        use crate::channel_work::{Change, Home, Mutation, Origin, Page, Role, Target};
        use crate::github_identity::{GitHubRepoKey, PullRequestKey};

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("channel-work16.sqlite3");
        let (identity, channel) = legacy(&path, 16);
        // A historical title mentioning a PR is metadata, never work intent.
        let db = Connection::open(&path).unwrap();
        db.execute(
            "UPDATE channels SET title='Review github.com/owner/repo/pull/7' WHERE id=?1",
            [channel.to_string()],
        )
        .unwrap();
        drop(db);
        let mut registry = Registry::at(&path).unwrap();
        let before = registry.model().unwrap();
        assert_eq!(before.identity.to_string(), identity);
        assert_eq!(before.sequence, 7);
        assert_eq!(before.channel(channel).unwrap().ordinal, 5);
        assert_eq!(
            before.channel(channel).unwrap().title,
            "Review github.com/owner/repo/pull/7"
        );
        let home = Home {
            registry: before.identity,
            channel,
            creation_ordinal: 5,
        };
        let empty = registry.read_channel_work(&home, Page::default()).unwrap();
        assert_eq!(empty.revision, 0);
        assert!(empty.entries.is_empty() && empty.history.is_empty());
        let operations: u64 = registry
            .db
            .query_row("SELECT count(*) FROM channel_work_operations", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(
            operations, 0,
            "migration does not invent an attribution receipt"
        );
        let request = Mutation {
            home,
            expected_revision: 0,
            operation: uuid::Uuid::new_v4(),
            change: Change::Claim {
                target: Target::PullRequest(
                    PullRequestKey::new(
                        GitHubRepoKey::new("github.com", None, "owner", "repo").unwrap(),
                        7,
                    )
                    .unwrap(),
                ),
                role: Role::Steward,
            },
        };
        let reply = registry
            .apply_channel_work(
                &TrustedResolvedOrigin {
                    origin: Origin::User,
                    proof: None,
                },
                &request,
            )
            .unwrap();
        assert!(reply.receipt.changed);
        assert_eq!(reply.snapshot.entries.len(), 1);
        assert_eq!(registry.model().unwrap().sequence, 8);
        let after = registry.model().unwrap();
        assert_eq!(after.channels, before.channels);
        assert_eq!(after.projects, before.projects);
        assert_eq!(after.views, before.views);
        assert_eq!(after.agents, before.agents);
        drop(registry);
        let reopened = Registry::at(&path).unwrap();
        assert_eq!(reopened.model().unwrap(), after);
        assert_eq!(
            reopened.read_channel_work(&home, Page::default()).unwrap(),
            reply.snapshot
        );
    }

    #[test]
    fn legacy_view_conversion_preserves_consumed_rows_and_rejects_unknown_shapes() {
        let _clear = Clear;
        // Synthesized compatibility fixture for the actual version-eight
        // reader; the old version-seven CREATE writer is not in Git history.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("legacy7.sqlite3");
        let (_, channel) = legacy(&path, 6);
        let db = Connection::open(&path).unwrap();
        db.execute_batch("CREATE TABLE workspace_views(channel TEXT PRIMARY KEY REFERENCES channels(id),revision INTEGER NOT NULL,panes TEXT NOT NULL,retired TEXT NOT NULL) STRICT;
            CREATE TABLE workspace_view_panes(id TEXT PRIMARY KEY,channel TEXT NOT NULL REFERENCES workspace_views(channel)) STRICT;
            CREATE TABLE workspace_view_edits(operation TEXT PRIMARY KEY,channel TEXT NOT NULL REFERENCES workspace_views(channel)) STRICT;
            PRAGMA user_version=7;").unwrap();
        db.execute(
            "INSERT INTO workspace_views VALUES(?1,9,'[]','[\"00000000-0000-4000-8000-000000000071\"]')",
            [channel.to_string()],
        )
        .unwrap();
        db.execute(
            "INSERT INTO workspace_view_panes VALUES('pane',?1)",
            [channel.to_string()],
        )
        .unwrap();
        db.execute(
            "INSERT INTO workspace_view_edits VALUES('edit',?1)",
            [channel.to_string()],
        )
        .unwrap();
        drop(db);
        let original = bytes(&path);
        FAIL_STEP.with(|s| s.set(Some(8)));
        assert!(Registry::at(&path).err().unwrap().contains("failure at 8"));
        FAIL_STEP.with(|s| s.set(None));
        assert_eq!(
            bytes(&path),
            original,
            "converted legacy rows must roll back with the failed migration"
        );
        let registry = Registry::at(&path).unwrap();
        let scope = serde_json::json!({"Channel":channel}).to_string();
        let view: (i64, String, String) = registry
            .db
            .query_row(
                "SELECT revision,panes,retired FROM shared_views WHERE scope=?1",
                [&scope],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(
            view,
            (
                9,
                "[]".into(),
                "[\"00000000-0000-4000-8000-000000000071\"]".into()
            )
        );
        for (table, column, value) in [
            ("shared_view_panes", "id", "pane"),
            ("shared_view_edits", "operation", "edit"),
        ] {
            assert_eq!(
                registry
                    .db
                    .query_row(
                        &format!("SELECT scope FROM {table} WHERE {column}=?1"),
                        [value],
                        |r| r.get::<_, String>(0)
                    )
                    .unwrap(),
                scope
            );
        }
        assert!(
            !inspect(&registry.db)
                .unwrap()
                .objects
                .iter()
                .any(|o| o.1.starts_with("workspace_view"))
        );
        drop(registry);
        let malformed = dir.path().join("unknown7.sqlite3");
        legacy(&malformed, 7);
        Connection::open(&malformed)
            .unwrap()
            .execute_batch("CREATE TABLE future_shape(secret TEXT) STRICT;")
            .unwrap();
        let before = bytes(&malformed);
        assert!(Registry::at(&malformed).is_err());
        assert_eq!(bytes(&malformed), before);
    }

    #[test]
    fn unknown_legacy_view_columns_indexes_and_triggers_are_not_dropped() {
        for extra in [
            "ALTER TABLE workspace_views ADD COLUMN secret TEXT; UPDATE workspace_views SET secret='keep me';",
            "CREATE INDEX unknown_legacy_index ON workspace_views(revision);",
            "CREATE TRIGGER unknown_legacy_trigger BEFORE DELETE ON workspace_views BEGIN SELECT RAISE(ABORT, 'keep'); END;",
        ] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("unknown-legacy.sqlite3");
            let (_, channel) = legacy(&path, 6);
            let db = Connection::open(&path).unwrap();
            db.execute_batch("CREATE TABLE workspace_views(channel TEXT PRIMARY KEY REFERENCES channels(id),revision INTEGER NOT NULL,panes TEXT NOT NULL,retired TEXT NOT NULL) STRICT;
                CREATE TABLE workspace_view_panes(id TEXT PRIMARY KEY,channel TEXT NOT NULL REFERENCES workspace_views(channel)) STRICT;
                CREATE TABLE workspace_view_edits(operation TEXT PRIMARY KEY,channel TEXT NOT NULL REFERENCES workspace_views(channel)) STRICT;
                PRAGMA user_version=7;").unwrap();
            db.execute(
                "INSERT INTO workspace_views VALUES(?1,9,'[]','[]')",
                [channel.to_string()],
            )
            .unwrap();
            db.execute_batch(extra).unwrap();
            drop(db);
            let before = bytes(&path);
            assert!(
                Registry::at(&path)
                    .err()
                    .unwrap()
                    .contains("unsupported legacy")
            );
            assert_eq!(bytes(&path), before);
        }
    }

    #[test]
    fn each_actual_migration_failure_rolls_back_all_prior_steps_and_header() {
        let _clear = Clear;
        for step in [1, 2, 3, 4, 5, 6, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("rollback.sqlite3");
            legacy(&path, 0);
            let before = bytes(&path);
            FAIL_STEP.with(|s| s.set(Some(step)));
            let error = Registry::at(&path).err().expect("injected failure");
            FAIL_STEP.with(|s| s.set(None));
            assert!(error.contains(&format!("failure at {step}")), "{error}");
            assert_eq!(
                bytes(&path),
                before,
                "migration {step} published partial bytes"
            );
            let db = Connection::open(&path).unwrap();
            assert_eq!(inspect(&db).unwrap().version, 0);
            assert_eq!(
                db.query_row("SELECT name FROM projects", [], |r| r.get::<_, String>(0))
                    .unwrap(),
                "My project"
            );
        }
    }

    #[test]
    fn future_delete_and_wal_refusal_preserves_database_and_wal_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let delete = dir.path().join("future-delete.sqlite3");
        legacy(&delete, 12);
        let db = Connection::open(&delete).unwrap();
        db.pragma_update(None, "user_version", SCHEMA_VERSION + 1)
            .unwrap();
        drop(db);
        std::fs::set_permissions(&delete, std::fs::Permissions::from_mode(0o640)).unwrap();
        let before = bytes(&delete);
        assert!(Registry::at(&delete).err().unwrap().contains("newer"));
        assert_eq!(bytes(&delete), before);
        assert_eq!(
            std::fs::metadata(&delete).unwrap().permissions().mode() & 0o777,
            0o640
        );
        let path = dir.path().join("future-wal.sqlite3");
        let registry = Registry::at(&path).unwrap();
        let writer = connect(&path, false).unwrap();
        writer
            .execute_batch(&format!(
                "PRAGMA user_version={}; INSERT INTO projects VALUES('/future','Future',1,'git');",
                SCHEMA_VERSION + 1
            ))
            .unwrap();
        drop(registry);
        drop(writer);
        let original = bytes(&path);
        let original_wal = bytes(&wal(&path));
        assert!(!original_wal.is_empty());
        assert!(Registry::at(&path).err().unwrap().contains("newer"));
        assert_eq!(bytes(&path), original);
        assert_eq!(bytes(&wal(&path)), original_wal);
    }

    #[test]
    fn authoritative_writer_rejects_concurrent_header_and_layout_changes() {
        let _clear = Clear;
        for change in [
            format!("PRAGMA user_version={};", SCHEMA_VERSION + 1),
            "ALTER TABLE projects RENAME COLUMN name TO nameifnotexists;".into(),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("stale-wal.sqlite3");
            drop(Registry::at(&path).unwrap());
            let saved = Rc::new(std::cell::RefCell::new(None));
            let observed = Rc::clone(&saved);
            let target = path.clone();
            hook(move |stage| {
                if stage == "inspected" {
                    let writer = connect(&target, false)?;
                    writer.execute_batch(&change).map_err(|e| e.to_string())?;
                    drop(writer);
                    *observed.borrow_mut() = Some((bytes(&target), bytes(&wal(&target))));
                }
                Ok(())
            });
            let error = Registry::at(&path)
                .err()
                .expect("stale inspection must refuse");
            HOOK.with(|h| *h.borrow_mut() = None);
            assert!(
                error.contains("newer") || error.contains("changed"),
                "{error}"
            );
            let (original, original_wal) = saved.borrow_mut().take().unwrap();
            assert_eq!(bytes(&path), original);
            assert_eq!(bytes(&wal(&path)), original_wal);
        }
    }

    #[test]
    fn current_delete_writer_lock_spans_commit_and_persistent_wal_conversion() {
        let _clear = Clear;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("current-delete.sqlite3");
        legacy(&path, 12);
        let observed = Rc::new(Cell::new(0));
        let flag = Rc::clone(&observed);
        let target = path.clone();
        hook(move |stage| {
            if matches!(stage, "committed" | "normal_before_wal") {
                let writer = connect(&target, false)?;
                writer
                    .busy_timeout(Duration::ZERO)
                    .map_err(|e| e.to_string())?;
                let error = writer
                    .pragma_update(None, "user_version", SCHEMA_VERSION + 1)
                    .expect_err("future writer must remain excluded after COMMIT");
                assert!(
                    matches!(error,rusqlite::Error::SqliteFailure(ref code,_) if matches!(code.code,rusqlite::ErrorCode::DatabaseBusy|rusqlite::ErrorCode::DatabaseLocked)),
                    "{error}"
                );
                flag.set(flag.get() + 1);
            }
            Ok(())
        });
        let mut registry = Registry::at(&path).unwrap();
        HOOK.with(|h| *h.borrow_mut() = None);
        assert_eq!(
            observed.get(),
            2,
            "both conversion boundaries must exclude writers"
        );
        require_current(&registry.db).unwrap();
        assert_eq!(
            registry
                .db
                .pragma_query_value(None, "journal_mode", |r| r.get::<_, String>(0))
                .unwrap(),
            "wal"
        );
        // Exclusive mode was released after configuration, not held forever.
        let writer = connect(&path, false).unwrap();
        writer
            .pragma_update(None, "user_version", SCHEMA_VERSION + 1)
            .unwrap();
        assert!(
            registry
                .apply(&ModelEdit::OpenProject {
                    repository: "/late".into(),
                    name: "Late".into(),
                    kind: ProjectKind::Git
                })
                .is_err()
        );
    }

    #[test]
    fn wal_opener_has_no_persistent_configuration_after_migration_commit() {
        let _clear = Clear;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("current-wal.sqlite3");
        drop(Registry::at(&path).unwrap());
        let saved = Rc::new(std::cell::RefCell::new(None));
        let observed = Rc::clone(&saved);
        let target = path.clone();
        hook(move |stage| {
            if stage == "committed" {
                let writer = connect(&target, false)?;
                writer.execute_batch(&format!("PRAGMA user_version={}; INSERT INTO projects VALUES('/future','Future',1,'git');", SCHEMA_VERSION + 1)).map_err(|e|e.to_string())?;
                drop(writer);
                *observed.borrow_mut() = Some((bytes(&target), bytes(&wal(&target))));
            }
            Ok(())
        });
        // Admission linearizes at COMMIT. A later writer may advance it, but
        // this old handle must perform no persistent configuration afterwards.
        let mut registry = Registry::at(&path).unwrap();
        HOOK.with(|h| *h.borrow_mut() = None);
        let (original, original_wal) = saved.borrow_mut().take().unwrap();
        assert_eq!(bytes(&path), original);
        assert_eq!(bytes(&wal(&path)), original_wal);
        assert!(
            registry
                .apply(&ModelEdit::CloseProject {
                    repository: "/future".into()
                })
                .is_err()
        );
        drop(registry);
        assert_eq!(bytes(&path), original);
        assert_eq!(bytes(&wal(&path)), original_wal);
    }

    #[test]
    fn retained_outer_edits_refuse_future_or_unknown_layout_before_dml() {
        for change in [
            format!("PRAGMA user_version={};", SCHEMA_VERSION + 1),
            "ALTER TABLE projects RENAME COLUMN name TO nameifnotexists;".into(),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("outer.sqlite3");
            let (_, channel) = legacy(&path, 12);
            let mut registry = Registry::at(&path).unwrap();
            let other = connect(&path, false).unwrap();
            other.execute_batch(&change).unwrap();
            let edits = [
                ModelEdit::OpenProject {
                    repository: "/new".into(),
                    name: "New".into(),
                    kind: ProjectKind::Git,
                },
                ModelEdit::ReopenProject {
                    repository: "/kept".into(),
                    name: "Overwrite".into(),
                    kind: ProjectKind::Git,
                },
                ModelEdit::CloseProject {
                    repository: "/kept".into(),
                },
                ModelEdit::CreateChannel {
                    id: ChannelId::new(),
                    repository: "/kept".into(),
                    title: "New channel".into(),
                    title_source: ChannelTitleSource::Manual,
                },
            ];
            let changed = registry.db.total_changes();
            for edit in edits {
                assert!(registry.apply(&edit).is_err());
                assert_eq!(registry.db.total_changes(), changed);
            }
            assert_eq!(
                other
                    .query_row("SELECT sequence FROM metadata", [], |r| r.get::<_, i64>(0))
                    .unwrap(),
                7
            );
            assert_eq!(
                other
                    .query_row("SELECT count(*) FROM projects", [], |r| r.get::<_, i64>(0))
                    .unwrap(),
                1
            );
            assert_eq!(
                other
                    .query_row("SELECT id FROM channels", [], |r| r.get::<_, String>(0))
                    .unwrap(),
                channel.to_string()
            );
            assert_eq!(
                other
                    .query_row("SELECT count(*) FROM closed_projects", [], |r| r
                        .get::<_, i64>(0))
                    .unwrap(),
                0
            );
        }
    }

    #[test]
    fn current_edits_keep_cascade_sequence_and_reopen_semantics() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("current.sqlite3");
        let (_, channel) = legacy(&path, 12);
        let mut registry = Registry::at(&path).unwrap();
        let closed = registry
            .apply(&ModelEdit::CloseProject {
                repository: "/kept".into(),
            })
            .unwrap();
        assert_eq!(closed.model.sequence, 8);
        assert!(closed.model.channel(channel).is_none());
        let reopened = registry
            .apply(&ModelEdit::ReopenProject {
                repository: "/kept".into(),
                name: "Returned".into(),
                kind: ProjectKind::Git,
            })
            .unwrap();
        assert_eq!(reopened.model.sequence, 9);
        assert_eq!(reopened.model.projects[0].name, "Returned");
        drop(registry);
        assert_eq!(
            Registry::at(&path).unwrap().model().unwrap(),
            reopened.model
        );
    }

    #[test]
    fn current_header_preserves_the_unversioned_legacy_project_kind_upgrade() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("legacy-kind.sqlite3");
        legacy(&path, 12);
        let mut registry = Registry::at(&path).unwrap();
        registry
            .db
            .execute_batch("ALTER TABLE projects DROP COLUMN kind;")
            .unwrap();
        assert!(
            registry
                .apply(&ModelEdit::CloseProject {
                    repository: "/kept".into()
                })
                .is_err()
        );
        drop(registry);
        let restored = Registry::at(&path).unwrap();
        let model = restored.model().unwrap();
        assert_eq!(model.sequence, 7);
        assert_eq!(model.projects[0].name, "My project");
        assert_eq!(model.projects[0].kind, ProjectKind::Git);
        require_current(&restored.db).unwrap();
    }

    #[test]
    fn failed_opener_cannot_release_another_connections_actual_writer_lock() {
        use crate::process::{
            CancellationToken, CommandSpec, ProcessExecutor, SystemProcessExecutor,
        };
        let _clear = Clear;
        let dir = tempfile::Builder::new()
            .prefix("registry12-lock-")
            .tempdir()
            .unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = root.join("writer.sqlite3");
        let mut registry = Registry::at(&path).unwrap();
        assert_eq!(
            registry
                .db
                .query_row("PRAGMA journal_mode=DELETE", [], |r| r.get::<_, String>(0))
                .unwrap(),
            "delete"
        );
        let tx = registry
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap();
        tx.execute("INSERT INTO projects VALUES('/owner','Writer',1,'git')", [])
            .unwrap();
        let prove = |busy: bool| {
            let spec = CommandSpec::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "host::model::schema::tests::writer_lock_child",
                    "--nocapture",
                ])
                .env("CAGENTS_REGISTRY12_LOCK_CHILD", &root)
                .env("CAGENTS_REGISTRY12_LOCK_BUSY", if busy { "1" } else { "0" })
                .timeout(Duration::from_secs(20))
                .output_limits(8192, 8192);
            let out = SystemProcessExecutor
                .execute(&spec, &CancellationToken::new())
                .and_then(|out| out.require_success(&spec.program))
                .unwrap();
            assert!(
                String::from_utf8_lossy(&out.stdout).contains("1 passed; 0 failed"),
                "{}",
                String::from_utf8_lossy(&out.stdout)
            );
        };
        prove(true);
        hook(|stage| {
            if stage == "inspected" {
                Err("forced second-opener refusal".into())
            } else {
                Ok(())
            }
        });
        assert!(
            Registry::at(&path)
                .err()
                .unwrap()
                .contains("forced second-opener refusal")
        );
        HOOK.with(|h| *h.borrow_mut() = None);
        assert!(!tx.is_autocommit());
        prove(true);
        tx.rollback().unwrap();
        prove(false);
    }

    #[test]
    fn writer_lock_child() {
        let Some(root) = std::env::var_os("CAGENTS_REGISTRY12_LOCK_CHILD") else {
            return;
        };
        let root = std::path::PathBuf::from(root);
        assert!(root.is_absolute());
        assert_eq!(root.canonicalize().unwrap(), root);
        assert!(
            root.file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .starts_with("registry12-lock-")
        );
        assert_eq!(
            std::fs::metadata(&root).unwrap().permissions().mode() & 0o777,
            0o700
        );
        let mut db = connect(&root.join("writer.sqlite3"), false).unwrap();
        db.busy_timeout(Duration::ZERO).unwrap();
        match std::env::var("CAGENTS_REGISTRY12_LOCK_BUSY")
            .unwrap()
            .as_str()
        {
            "1" => {
                let error = db
                    .transaction_with_behavior(TransactionBehavior::Immediate)
                    .err()
                    .expect("the original writer must remain excluded");
                assert!(
                    matches!(error,rusqlite::Error::SqliteFailure(ref code,_) if matches!(code.code,rusqlite::ErrorCode::DatabaseBusy|rusqlite::ErrorCode::DatabaseLocked)),
                    "{error}"
                );
            }
            "0" => db
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .unwrap()
                .rollback()
                .unwrap(),
            _ => panic!("invalid child expectation"),
        }
    }

    #[test]
    fn schema_tokens_do_not_erase_identifiers_or_change_literal_meaning() {
        assert_ne!(
            tokens("CREATE TABLE t(instance TEXT)").unwrap(),
            tokens("CREATE TABLE t(instanceifnotexists TEXT)").unwrap()
        );
        assert_ne!(
            tokens("CREATE TABLE t(x TEXT DEFAULT 'A B')").unwrap(),
            tokens("CREATE TABLE t(x TEXT DEFAULT 'ab')").unwrap()
        );
        assert_eq!(
            tokens("CREATE TABLE IF NOT EXISTS t(x TEXT);").unwrap(),
            tokens("create table t (x text)").unwrap()
        );
        assert!(tokens("CREATE TABLE t(é TEXT)").is_ok());
    }

    #[test]
    fn hot_rollback_journal_refusal_does_not_recover_or_change_bytes() {
        use crate::process::{
            CancellationToken, CommandSpec, ProcessExecutor, ProcessFailureKind,
            SystemProcessExecutor,
        };
        let dir = tempfile::Builder::new()
            .prefix("registry12-hot-")
            .tempdir()
            .unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = root.join("hot.sqlite3");
        let spec = CommandSpec::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "host::model::schema::tests::hot_journal_child",
                "--nocapture",
            ])
            .env("CAGENTS_REGISTRY12_HOT_CHILD", &root)
            .timeout(Duration::from_secs(20))
            .output_limits(8192, 8192);
        let failure = SystemProcessExecutor
            .execute(&spec, &CancellationToken::new())
            .and_then(|out| out.require_success(&spec.program))
            .expect_err("child must die during uncommitted transaction");
        assert_eq!(failure.kind, ProcessFailureKind::Exit);
        assert_eq!(failure.signal, Some(libc::SIGKILL));
        let journal = path.with_file_name("hot.sqlite3-journal");
        let original = bytes(&path);
        let original_journal = bytes(&journal);
        assert!(original_journal.len() > 512);
        assert_ne!(&original_journal[..8], &[0; 8]);
        assert!(Registry::at(&path).is_err());
        assert_eq!(bytes(&path), original);
        assert_eq!(bytes(&journal), original_journal);
    }

    #[test]
    fn hot_journal_child() {
        let Some(root) = std::env::var_os("CAGENTS_REGISTRY12_HOT_CHILD") else {
            return;
        };
        let root = std::path::PathBuf::from(root);
        assert!(root.is_absolute());
        assert_eq!(root.canonicalize().unwrap(), root);
        assert!(
            root.file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .starts_with("registry12-hot-")
        );
        assert_eq!(
            std::fs::metadata(&root).unwrap().permissions().mode() & 0o777,
            0o700
        );
        let db = Connection::open(root.join("hot.sqlite3")).unwrap();
        db.execute_batch(&format!("PRAGMA journal_mode=DELETE; PRAGMA synchronous=FULL; CREATE TABLE future_payload(value BLOB); INSERT INTO future_payload VALUES(zeroblob(1048576)); PRAGMA user_version={}; PRAGMA cache_size=4; PRAGMA cache_spill=ON; BEGIN IMMEDIATE; UPDATE future_payload SET value=randomblob(1048576);", SCHEMA_VERSION + 1)).unwrap();
        assert!(root.join("hot.sqlite3-journal").is_file());
        // The bounded original-child executor owns this exact process and its
        // wait/cleanup. No numeric PID lookup or fixture killing Drop exists.
        unsafe {
            libc::raise(libc::SIGKILL);
        }
        panic!("SIGKILL returned");
    }
}
