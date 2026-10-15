//! Admission for the published registry format. This module owns no runtime
//! authority and does not extend the schema: the supported format remains 12.
//!
//! Admission serializes cooperating SQLite connections to the existing file.
//! The read transaction is not a physical identity proof for a separately
//! opened connection if an external actor replaces the database pathname.

use std::{fs::{File, OpenOptions}, os::unix::fs::{OpenOptionsExt, PermissionsExt}, path::Path, sync::OnceLock};
use rusqlite::{config::DbConfig, Connection, OpenFlags, Transaction, TransactionBehavior};
use super::{BUSY_TIMEOUT, FILE_MODE, SCHEMA_VERSION};

#[derive(Clone, Debug, PartialEq, Eq)]
struct Inspection {
    version: u32,
    cookie: i64,
    objects: Vec<(String, String, String, Vec<String>)>,
}

fn connect(path: &Path, readonly: bool) -> Result<Connection, String> {
    let access = if readonly { OpenFlags::SQLITE_OPEN_READ_ONLY } else { OpenFlags::SQLITE_OPEN_READ_WRITE };
    let db = Connection::open_with_flags(path, access | OpenFlags::SQLITE_OPEN_NO_MUTEX | OpenFlags::SQLITE_OPEN_NOFOLLOW)
        .map_err(|e| e.to_string())?;
    // Set before the first SQL, including refusals. A retained handle can later
    // become stale, so do not re-enable checkpoint-on-close after admission.
    db.set_db_config(DbConfig::SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE, true).map_err(|e| e.to_string())?;
    db.busy_timeout(BUSY_TIMEOUT).map_err(|e| e.to_string())?;
    Ok(db)
}

pub(super) fn open(path: &Path) -> Result<Connection, String> {
    if let Some(parent) = path.parent() {
        super::create_private_directory(parent).map_err(|e| e.to_string())?;
    }
    let file = match OpenOptions::new().write(true).create_new(true).mode(FILE_MODE)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC).open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => OpenOptions::new().read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC).open(path).map_err(|e| e.to_string())?,
        Err(e) => return Err(e.to_string()),
    };
    if !file.metadata().map_err(|e| e.to_string())?.is_file() { return Err("workspace registry is not a regular file".into()); }
    open_existing(path, &file)
}

fn open_existing(path: &Path, file: &File) -> Result<Connection, String> {
    // A read transaction pins rollback-journal inspection until writer
    // admission. A hot journal must fail here, before a RW connection can
    // recover it. WAL writers may still advance; the writer reread detects it.
    let mut reader = connect(path, true)?;
    let pin = reader.transaction().map_err(|e| e.to_string())?;
    let inspected = inspect(&pin)?;
    if inspected.version == SCHEMA_VERSION { require_open_layout(&pin)?; }
    checkpoint("inspected")?;
    let mut db = connect(path, false)?;
    db.execute_batch("PRAGMA foreign_keys=ON; PRAGMA synchronous=FULL;").map_err(|e| e.to_string())?;
    if db.pragma_query_value(None, "foreign_keys", |r| r.get::<_, i64>(0)).map_err(|e| e.to_string())? != 1 {
        return Err("SQLite foreign key enforcement could not be enabled".into());
    }
    if db.pragma_query_value(None, "synchronous", |r| r.get::<_, i64>(0)).map_err(|e|e.to_string())? != 2 {
        return Err("SQLite synchronous mode could not be set to FULL".into());
    }
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate).map_err(|e| e.to_string())?;
    if inspect(&tx)? != inspected { return Err("workspace registry changed during admission".into()); }
    let journal: String = tx.pragma_query_value(None, "journal_mode", |r| r.get(0)).map_err(|e| e.to_string())?;
    let convert = !journal.eq_ignore_ascii_case("wal");
    if convert {
        // Reserved writer ownership precedes releasing the reader. Once a
        // write takes EXCLUSIVE, exclusive locking retains that database lock
        // across COMMIT and the persistent journal-mode conversion below.
        tx.pragma_update(None, "locking_mode", "EXCLUSIVE").map_err(|e| e.to_string())?;
    }
    pin.commit().map_err(|e| e.to_string())?;
    drop(reader);
    checkpoint("admitted")?;
    super::migrate_through(&tx, inspected.version, SCHEMA_VERSION)?;
    require_layout(&tx)?;
    // Also forces the rollback-mode EXCLUSIVE write before commit for an
    // already-current store. No future writer can enter the conversion gap.
    tx.pragma_update(None, "user_version", SCHEMA_VERSION).map_err(|e| e.to_string())?;
    file.set_permissions(std::fs::Permissions::from_mode(FILE_MODE)).map_err(|e| e.to_string())?;
    tx.commit().map_err(|e| e.to_string())?;
    checkpoint("committed")?;
    if convert {
        let mode: String = db.query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0)).map_err(|e| e.to_string())?;
        if !mode.eq_ignore_ascii_case("wal") { return Err(format!("SQLite journal mode is `{mode}`, but WAL is required")); }
        db.pragma_update(None, "locking_mode", "NORMAL").map_err(|e| e.to_string())?;
        // End the exclusive-lock lifetime only after persistent configuration.
        let release = db.transaction().map_err(|e| e.to_string())?;
        require_current(&release)?;
        release.commit().map_err(|e| e.to_string())?;
    }
    Ok(db)
}

fn inspect(db: &Connection) -> Result<Inspection, String> {
    let version: u32 = db.pragma_query_value(None, "user_version", |r| r.get(0)).map_err(|e| e.to_string())?;
    if version > SCHEMA_VERSION {
        return Err(format!("workspace registry schema {version} is newer than supported schema {SCHEMA_VERSION}"));
    }
    let cookie = db.pragma_query_value(None, "schema_version", |r| r.get(0)).map_err(|e| e.to_string())?;
    let mut statement = db.prepare("SELECT type,name,tbl_name,CASE WHEN length(CAST(sql AS BLOB))<=65536 THEN sql END FROM sqlite_schema WHERE name NOT GLOB 'sqlite_*' ORDER BY type,name LIMIT 129").map_err(|e| e.to_string())?;
    let rows = statement.query_map([], |r| Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,Option<String>>(3)?))).map_err(|e| e.to_string())?;
    let mut objects = Vec::new();
    for row in rows {
        let (kind,name,table,sql) = row.map_err(|e| e.to_string())?;
        let sql = sql.ok_or("workspace registry contains an unsupported schema object")?;
        objects.push((kind,name,table,tokens(&sql)?));
        if objects.len() > 128 { return Err("workspace registry schema exceeds its bound".into()); }
    }
    Ok(Inspection { version, cookie, objects })
}

type Layout = Vec<(String, String, String, Vec<String>)>;
fn published_layouts() -> Result<&'static (Layout, Layout), String> {
    static CURRENT: OnceLock<Result<(Layout, Layout), String>> = OnceLock::new();
    CURRENT.get_or_init(|| {
        let mut reference = Connection::open_in_memory().map_err(|e| e.to_string())?;
        let tx = reference.transaction().map_err(|e| e.to_string())?;
        // Derive the baseline from the same published bodies, without future
        // DDL. The unversioned project-kind upgrade predates header admission.
        reference_migration(&tx)?;
        let complete = inspect(&tx)?.objects;
        tx.execute_batch("ALTER TABLE projects DROP COLUMN kind;").map_err(|e|e.to_string())?;
        Ok((complete, inspect(&tx)?.objects))
    }).as_ref().map_err(Clone::clone)
}
fn require_layout(db: &Connection) -> Result<(), String> {
    if inspect(db)?.objects != published_layouts()?.0 { return Err("workspace registry layout does not match published schema 12".into()); }
    Ok(())
}
fn require_open_layout(db: &Connection) -> Result<(), String> {
    let actual = inspect(db)?.objects;
    let (complete, legacy_kind) = published_layouts()?;
    if &actual != complete && &actual != legacy_kind { return Err("workspace registry layout does not match published schema 12".into()); }
    Ok(())
}

/// The historical writer is unavailable in this tree. Admit the explicit
/// compatibility shape consumed by the existing conversion, rather than drop
/// unknown columns, indexes or triggers along with its three legacy tables.
pub(super) fn require_legacy_views(db: &Connection) -> Result<(), String> {
    let definitions = [
        ("workspace_views", "CREATE TABLE workspace_views(channel TEXT PRIMARY KEY REFERENCES channels(id),revision INTEGER NOT NULL,panes TEXT NOT NULL,retired TEXT NOT NULL) STRICT"),
        ("workspace_view_panes", "CREATE TABLE workspace_view_panes(id TEXT PRIMARY KEY,channel TEXT NOT NULL REFERENCES workspace_views(channel)) STRICT"),
        ("workspace_view_edits", "CREATE TABLE workspace_view_edits(operation TEXT PRIMARY KEY,channel TEXT NOT NULL REFERENCES workspace_views(channel)) STRICT"),
    ];
    let mut expected = definitions.iter().map(|(name,sql)| Ok(("table".to_owned(),name.to_string(),name.to_string(),tokens(sql)?)))
        .collect::<Result<Layout,String>>()?;
    expected.sort();
    let actual = inspect(db)?.objects.into_iter().filter(|o| definitions.iter().any(|(name,_)|o.2==*name)).collect::<Layout>();
    if actual != expected { return Err("unsupported legacy workspace-view layout; the original store was not converted".into()); }
    Ok(())
}

pub(super) fn require_current(db: &Connection) -> Result<(), String> {
    let actual = inspect(db)?;
    if actual.version != SCHEMA_VERSION { return Err(format!("workspace registry schema {} requires reopening before edits", actual.version)); }
    require_layout(db)
}

// Tokenize, rather than deleting substrings: a column named
// `instanceifnotexists` must never become an `instance` column in validation.
fn tokens(sql: &str) -> Result<Vec<String>,String> {
    let bytes=sql.as_bytes(); let mut i=0; let mut out=Vec::new();
    while i<bytes.len() {
        if bytes[i].is_ascii_whitespace() { i+=1; continue; }
        let start=i;
        if matches!(bytes[i], b'\''|b'"'|b'`'|b'[') {
            let end=if bytes[i]==b'[' { b']' } else { bytes[i] }; i+=1;
            loop {
                if i>=bytes.len() { return Err("unterminated registry schema token".into()); }
                if bytes[i]==end { i+=1; if end!=b']' && i<bytes.len() && bytes[i]==end { i+=1; } else { break; } } else { i+=1; }
            }
            out.push(sql[start..i].to_owned());
        } else if bytes[i].is_ascii_alphanumeric() || matches!(bytes[i],b'_'|b'$') {
            i+=1; while i<bytes.len() && (bytes[i].is_ascii_alphanumeric() || matches!(bytes[i],b'_'|b'$')) { i+=1; }
            out.push(sql[start..i].to_ascii_lowercase());
        } else { i+=sql[i..].chars().next().ok_or("invalid registry schema token")?.len_utf8(); out.push(sql[start..i].to_owned()); }
    }
    let position = if out.first().is_some_and(|v| v=="create") {
        match out.get(1).map(String::as_str) { Some("table"|"index")=>Some(2),Some("unique") if out.get(2).is_some_and(|v|v=="index")=>Some(3),_=>None }
    } else { None };
    if let Some(p)=position { if out.get(p..p+3).is_some_and(|v|v==["if","not","exists"]) { out.drain(p..p+3); } }
    while out.last().is_some_and(|v|v==";") { out.pop(); }
    Ok(out)
}

fn reference_migration(tx: &Transaction<'_>) -> Result<(), String> {
    #[cfg(test)]
    let previous = FAIL_STEP.with(|s| s.replace(None));
    let result = super::migrate_through(tx, 0, SCHEMA_VERSION);
    #[cfg(test)]
    FAIL_STEP.with(|s| s.set(previous));
    result
}

pub(super) fn migration_checkpoint(_step: u32) -> Result<(),String> {
    #[cfg(test)]
    if FAIL_STEP.with(|s|s.get())==Some(_step) { return Err(format!("injected migration failure at {_step}")); }
    Ok(())
}
fn checkpoint(_stage: &str) -> Result<(),String> {
    #[cfg(test)]
    HOOK.with(|h| { if let Some(h)=h.borrow_mut().as_mut() { h(_stage)?; } Ok::<_,String>(()) })?;
    Ok(())
}
#[cfg(test)]
thread_local! {
    static FAIL_STEP: std::cell::Cell<Option<u32>> = const { std::cell::Cell::new(None) };
    static HOOK: std::cell::RefCell<Option<Box<dyn FnMut(&str)->Result<(),String>>>> = const { std::cell::RefCell::new(None) };
}


#[cfg(test)]
mod tests {
    use super::*;
    use super::super::Registry;
    use crate::{model::{ChannelId, ChannelTitleSource, ProjectKind}, workspace_model::ModelEdit};
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
        tx.pragma_update(None, "user_version", version).unwrap();
        tx.commit().unwrap();
        (identity, channel)
    }
    fn bytes(path: &Path) -> Vec<u8> { std::fs::read(path).unwrap() }
    fn wal(path: &Path) -> std::path::PathBuf { path.with_file_name(format!("{}-wal",path.file_name().unwrap().to_str().unwrap())) }
    fn hook(f: impl FnMut(&str)->Result<(),String>+'static) { HOOK.with(|h| *h.borrow_mut()=Some(Box::new(f))); }
    struct Clear;
    impl Drop for Clear { fn drop(&mut self) { HOOK.with(|h|*h.borrow_mut()=None); FAIL_STEP.with(|s|s.set(None)); } }

    #[test]
    fn every_published_version_preserves_nonempty_model_identity_and_ordinals() {
        for version in 0..=12 {
            let dir=tempfile::tempdir().unwrap(); let path=dir.path().join("registry.sqlite3");
            let (identity, channel)=legacy(&path,version);
            let registry=Registry::at(&path).unwrap_or_else(|e|panic!("version {version}: {e}"));
            let model=registry.model().unwrap();
            assert_eq!(model.identity.to_string(),identity); assert_eq!(model.sequence,7);
            assert_eq!(model.projects[0].name,"My project"); assert_eq!(model.projects[0].kind,ProjectKind::Folder);
            assert_eq!(model.projects[0].ordinal,4); assert_eq!(model.channels[0].id,channel);
            assert_eq!(model.channels[0].title,"My channel"); assert_eq!(model.channels[0].ordinal,5);
            let ordinals:(i64,i64)=registry.db.query_row("SELECT next_project_ordinal,next_channel_ordinal FROM metadata",[],|r|Ok((r.get(0)?,r.get(1)?))).unwrap();
            assert_eq!(ordinals,(11,12));
            require_current(&registry.db).unwrap();
            drop(registry);
            assert_eq!(Registry::at(&path).unwrap().model().unwrap(),model);
        }
    }

    #[test]
    fn legacy_view_conversion_preserves_consumed_rows_and_rejects_unknown_shapes() {
        // Synthesized compatibility fixture for the actual version-eight
        // reader; the old version-seven CREATE writer is not in Git history.
        let dir=tempfile::tempdir().unwrap(); let path=dir.path().join("legacy7.sqlite3");
        let (_,channel)=legacy(&path,6);
        let db=Connection::open(&path).unwrap();
        db.execute_batch("CREATE TABLE workspace_views(channel TEXT PRIMARY KEY REFERENCES channels(id),revision INTEGER NOT NULL,panes TEXT NOT NULL,retired TEXT NOT NULL) STRICT;
            CREATE TABLE workspace_view_panes(id TEXT PRIMARY KEY,channel TEXT NOT NULL REFERENCES workspace_views(channel)) STRICT;
            CREATE TABLE workspace_view_edits(operation TEXT PRIMARY KEY,channel TEXT NOT NULL REFERENCES workspace_views(channel)) STRICT;
            PRAGMA user_version=7;").unwrap();
        db.execute("INSERT INTO workspace_views VALUES(?1,9,'[]','[\"kept\"]')",[channel.to_string()]).unwrap();
        db.execute("INSERT INTO workspace_view_panes VALUES('pane',?1)",[channel.to_string()]).unwrap();
        db.execute("INSERT INTO workspace_view_edits VALUES('edit',?1)",[channel.to_string()]).unwrap();
        drop(db);
        let registry=Registry::at(&path).unwrap();
        let scope=serde_json::json!({"Channel":channel}).to_string();
        let view:(i64,String,String)=registry.db.query_row("SELECT revision,panes,retired FROM shared_views WHERE scope=?1",[&scope],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).unwrap();
        assert_eq!(view,(9,"[]".into(),"[\"kept\"]".into()));
        for (table,column,value) in [("shared_view_panes","id","pane"),("shared_view_edits","operation","edit")] {
            assert_eq!(registry.db.query_row(&format!("SELECT scope FROM {table} WHERE {column}=?1"),[value],|r|r.get::<_,String>(0)).unwrap(),scope);
        }
        assert!(!inspect(&registry.db).unwrap().objects.iter().any(|o|o.1.starts_with("workspace_view")));
        drop(registry);
        let malformed=dir.path().join("unknown7.sqlite3");legacy(&malformed,7);
        Connection::open(&malformed).unwrap().execute_batch("CREATE TABLE future_shape(secret TEXT) STRICT;").unwrap();
        let before=bytes(&malformed);assert!(Registry::at(&malformed).is_err());assert_eq!(bytes(&malformed),before);
    }

    #[test]
    fn unknown_legacy_view_columns_and_indexes_are_not_dropped() {
        for extra in ["ALTER TABLE workspace_views ADD COLUMN secret TEXT; UPDATE workspace_views SET secret='keep me';", "CREATE INDEX unknown_legacy_index ON workspace_views(revision);"] {
            let dir=tempfile::tempdir().unwrap();let path=dir.path().join("unknown-legacy.sqlite3");let (_,channel)=legacy(&path,6);
            let db=Connection::open(&path).unwrap();
            db.execute_batch("CREATE TABLE workspace_views(channel TEXT PRIMARY KEY REFERENCES channels(id),revision INTEGER NOT NULL,panes TEXT NOT NULL,retired TEXT NOT NULL) STRICT;
                CREATE TABLE workspace_view_panes(id TEXT PRIMARY KEY,channel TEXT NOT NULL REFERENCES workspace_views(channel)) STRICT;
                CREATE TABLE workspace_view_edits(operation TEXT PRIMARY KEY,channel TEXT NOT NULL REFERENCES workspace_views(channel)) STRICT;
                PRAGMA user_version=7;").unwrap();
            db.execute("INSERT INTO workspace_views VALUES(?1,9,'[]','[]')",[channel.to_string()]).unwrap();db.execute_batch(extra).unwrap();drop(db);
            let before=bytes(&path);assert!(Registry::at(&path).err().unwrap().contains("unsupported legacy"));assert_eq!(bytes(&path),before);
        }
    }

    #[test]
    fn each_actual_migration_failure_rolls_back_all_prior_steps_and_header() {
        let _clear=Clear;
        for step in [1,2,3,4,5,6,8,9,10,11,12] {
            let dir=tempfile::tempdir().unwrap();let path=dir.path().join("rollback.sqlite3");legacy(&path,0);
            let before=bytes(&path);
            FAIL_STEP.with(|s|s.set(Some(step)));
            let error=Registry::at(&path).err().expect("injected failure");
            FAIL_STEP.with(|s|s.set(None));
            assert!(error.contains(&format!("failure at {step}")),"{error}");
            assert_eq!(bytes(&path),before,"migration {step} published partial bytes");
            let db=Connection::open(&path).unwrap();assert_eq!(inspect(&db).unwrap().version,0);
            assert_eq!(db.query_row("SELECT name FROM projects",[],|r|r.get::<_,String>(0)).unwrap(),"My project");
        }
    }

    #[test]
    fn future_delete_and_wal_refusal_preserves_database_and_wal_bytes() {
        let dir=tempfile::tempdir().unwrap();
        let delete=dir.path().join("future-delete.sqlite3");legacy(&delete,12);
        let db=Connection::open(&delete).unwrap();db.pragma_update(None,"user_version",13).unwrap();drop(db);
        std::fs::set_permissions(&delete,std::fs::Permissions::from_mode(0o640)).unwrap();
        let before=bytes(&delete);assert!(Registry::at(&delete).err().unwrap().contains("newer"));
        assert_eq!(bytes(&delete),before);assert_eq!(std::fs::metadata(&delete).unwrap().permissions().mode()&0o777,0o640);
        let path=dir.path().join("future-wal.sqlite3");let registry=Registry::at(&path).unwrap();
        let writer=connect(&path,false).unwrap();writer.execute_batch("PRAGMA user_version=13; INSERT INTO projects VALUES('/future','Future',1,'git');").unwrap();
        drop(registry);drop(writer);
        let original=bytes(&path);let original_wal=bytes(&wal(&path));assert!(!original_wal.is_empty());
        assert!(Registry::at(&path).err().unwrap().contains("newer"));
        assert_eq!(bytes(&path),original);assert_eq!(bytes(&wal(&path)),original_wal);
    }

    #[test]
    fn authoritative_writer_rejects_concurrent_header_and_layout_changes() {
        let _clear=Clear;
        for change in ["PRAGMA user_version=13;", "ALTER TABLE projects RENAME COLUMN name TO nameifnotexists;"] {
            let dir=tempfile::tempdir().unwrap();let path=dir.path().join("stale-wal.sqlite3");drop(Registry::at(&path).unwrap());
            let saved=Rc::new(std::cell::RefCell::new(None));let observed=Rc::clone(&saved);let target=path.clone();
            hook(move|stage| { if stage=="inspected" {
                let writer=connect(&target,false)?;writer.execute_batch(change).map_err(|e|e.to_string())?;drop(writer);
                *observed.borrow_mut()=Some((bytes(&target),bytes(&wal(&target))));
            } Ok(()) });
            let error=Registry::at(&path).err().expect("stale inspection must refuse");
            HOOK.with(|h|*h.borrow_mut()=None);
            assert!(error.contains("newer")||error.contains("changed"),"{error}");
            let (original,original_wal)=saved.borrow_mut().take().unwrap();
            assert_eq!(bytes(&path),original);assert_eq!(bytes(&wal(&path)),original_wal);
        }
    }

    #[test]
    fn current_delete_writer_lock_spans_commit_and_persistent_wal_conversion() {
        let _clear=Clear;
        let dir=tempfile::tempdir().unwrap();let path=dir.path().join("current-delete.sqlite3");legacy(&path,12);
        let observed=Rc::new(Cell::new(false));let flag=Rc::clone(&observed);let target=path.clone();
        hook(move|stage| { if stage=="committed" {
            let writer=connect(&target,false)?;writer.busy_timeout(Duration::ZERO).map_err(|e|e.to_string())?;
            let error=writer.execute_batch("PRAGMA user_version=13;").expect_err("future writer must remain excluded after COMMIT");
            assert!(matches!(error,rusqlite::Error::SqliteFailure(ref code,_) if matches!(code.code,rusqlite::ErrorCode::DatabaseBusy|rusqlite::ErrorCode::DatabaseLocked)),"{error}");
            flag.set(true);
        } Ok(()) });
        let mut registry=Registry::at(&path).unwrap();HOOK.with(|h|*h.borrow_mut()=None);
        assert!(observed.get());require_current(&registry.db).unwrap();
        assert_eq!(registry.db.pragma_query_value(None,"journal_mode",|r|r.get::<_,String>(0)).unwrap(),"wal");
        // Exclusive mode was released after configuration, not held forever.
        let writer=connect(&path,false).unwrap();writer.pragma_update(None,"user_version",13).unwrap();
        assert!(registry.apply(&ModelEdit::OpenProject{repository:"/late".into(),name:"Late".into(),kind:ProjectKind::Git}).is_err());
    }

    #[test]
    fn wal_opener_has_no_persistent_configuration_after_migration_commit() {
        let _clear=Clear;
        let dir=tempfile::tempdir().unwrap();let path=dir.path().join("current-wal.sqlite3");drop(Registry::at(&path).unwrap());
        let saved=Rc::new(std::cell::RefCell::new(None));let observed=Rc::clone(&saved);let target=path.clone();
        hook(move|stage| { if stage=="committed" {
            let writer=connect(&target,false)?;writer.execute_batch("PRAGMA user_version=13; INSERT INTO projects VALUES('/future','Future',1,'git');").map_err(|e|e.to_string())?;drop(writer);
            *observed.borrow_mut()=Some((bytes(&target),bytes(&wal(&target))));
        } Ok(()) });
        // Admission linearizes at COMMIT. A later writer may advance it, but
        // this old handle must perform no persistent configuration afterwards.
        let mut registry=Registry::at(&path).unwrap();HOOK.with(|h|*h.borrow_mut()=None);
        let (original,original_wal)=saved.borrow_mut().take().unwrap();
        assert_eq!(bytes(&path),original);assert_eq!(bytes(&wal(&path)),original_wal);
        assert!(registry.apply(&ModelEdit::CloseProject{repository:"/future".into()}).is_err());drop(registry);
        assert_eq!(bytes(&path),original);assert_eq!(bytes(&wal(&path)),original_wal);
    }

    #[test]
    fn retained_outer_edits_refuse_future_or_unknown_layout_before_dml() {
        for change in ["PRAGMA user_version=13;", "ALTER TABLE projects RENAME COLUMN name TO nameifnotexists;"] {
            let dir=tempfile::tempdir().unwrap();let path=dir.path().join("outer.sqlite3");let (_,channel)=legacy(&path,12);
            let mut registry=Registry::at(&path).unwrap();let other=connect(&path,false).unwrap();other.execute_batch(change).unwrap();
            let edits=[ModelEdit::OpenProject{repository:"/new".into(),name:"New".into(),kind:ProjectKind::Git},
                ModelEdit::ReopenProject{repository:"/kept".into(),name:"Overwrite".into(),kind:ProjectKind::Git},
                ModelEdit::CloseProject{repository:"/kept".into()},
                ModelEdit::CreateChannel{id:ChannelId::new(),repository:"/kept".into(),title:"New channel".into(),title_source:ChannelTitleSource::Manual}];
            let changed=registry.db.total_changes();
            for edit in edits { assert!(registry.apply(&edit).is_err());assert_eq!(registry.db.total_changes(),changed); }
            assert_eq!(other.query_row("SELECT sequence FROM metadata",[],|r|r.get::<_,i64>(0)).unwrap(),7);
            assert_eq!(other.query_row("SELECT count(*) FROM projects",[],|r|r.get::<_,i64>(0)).unwrap(),1);
            assert_eq!(other.query_row("SELECT id FROM channels",[],|r|r.get::<_,String>(0)).unwrap(),channel.to_string());
            assert_eq!(other.query_row("SELECT count(*) FROM closed_projects",[],|r|r.get::<_,i64>(0)).unwrap(),0);
        }
    }

    #[test]
    fn current_edits_keep_cascade_sequence_and_reopen_semantics() {
        let dir=tempfile::tempdir().unwrap();let path=dir.path().join("current.sqlite3");let (_,channel)=legacy(&path,12);
        let mut registry=Registry::at(&path).unwrap();
        let closed=registry.apply(&ModelEdit::CloseProject{repository:"/kept".into()}).unwrap();assert_eq!(closed.model.sequence,8);assert!(closed.model.channel(channel).is_none());
        let reopened=registry.apply(&ModelEdit::ReopenProject{repository:"/kept".into(),name:"Returned".into(),kind:ProjectKind::Git}).unwrap();
        assert_eq!(reopened.model.sequence,9);assert_eq!(reopened.model.projects[0].name,"Returned");
        drop(registry);assert_eq!(Registry::at(&path).unwrap().model().unwrap(),reopened.model);
    }

    #[test]
    fn current_header_preserves_the_unversioned_legacy_project_kind_upgrade() {
        let dir=tempfile::tempdir().unwrap();let path=dir.path().join("legacy-kind.sqlite3");legacy(&path,12);
        let mut registry=Registry::at(&path).unwrap();
        registry.db.execute_batch("ALTER TABLE projects DROP COLUMN kind;").unwrap();
        assert!(registry.apply(&ModelEdit::CloseProject{repository:"/kept".into()}).is_err());
        drop(registry);
        let restored=Registry::at(&path).unwrap();let model=restored.model().unwrap();
        assert_eq!(model.sequence,7);assert_eq!(model.projects[0].name,"My project");assert_eq!(model.projects[0].kind,ProjectKind::Git);
        require_current(&restored.db).unwrap();
    }

    #[test]
    fn schema_tokens_do_not_erase_identifiers_or_change_literal_meaning() {
        assert_ne!(tokens("CREATE TABLE t(instance TEXT)").unwrap(),tokens("CREATE TABLE t(instanceifnotexists TEXT)").unwrap());
        assert_ne!(tokens("CREATE TABLE t(x TEXT DEFAULT 'A B')").unwrap(),tokens("CREATE TABLE t(x TEXT DEFAULT 'ab')").unwrap());
        assert_eq!(tokens("CREATE TABLE IF NOT EXISTS t(x TEXT);").unwrap(),tokens("create table t (x text)").unwrap());
        assert!(tokens("CREATE TABLE t(é TEXT)").is_ok());
    }

    #[test]
    fn hot_rollback_journal_refusal_does_not_recover_or_change_bytes() {
        use crate::process::{ProcessExecutor,CommandSpec,CancellationToken,SystemProcessExecutor,ProcessFailureKind};
        let dir=tempfile::Builder::new().prefix("registry12-hot-").tempdir().unwrap();
        let root=dir.path().canonicalize().unwrap();std::fs::set_permissions(&root,std::fs::Permissions::from_mode(0o700)).unwrap();
        let path=root.join("hot.sqlite3");
        let spec=CommandSpec::new(std::env::current_exe().unwrap()).args(["--exact","host::model::schema::tests::hot_journal_child","--nocapture"])
            .env("CAGENTS_REGISTRY12_HOT_CHILD",&root).timeout(Duration::from_secs(20)).output_limits(8192,8192);
        let failure=SystemProcessExecutor.execute(&spec,&CancellationToken::new()).and_then(|out|out.require_success(&spec.program)).expect_err("child must die during uncommitted transaction");
        assert_eq!(failure.kind,ProcessFailureKind::Exit);assert_eq!(failure.signal,Some(libc::SIGKILL));
        let journal=path.with_file_name("hot.sqlite3-journal");let original=bytes(&path);let original_journal=bytes(&journal);
        assert!(original_journal.len()>512);assert_ne!(&original_journal[..8],&[0;8]);
        assert!(Registry::at(&path).is_err());assert_eq!(bytes(&path),original);assert_eq!(bytes(&journal),original_journal);
    }

    #[test]
    fn hot_journal_child() {
        let Some(root)=std::env::var_os("CAGENTS_REGISTRY12_HOT_CHILD") else { return; };
        let root=std::path::PathBuf::from(root);assert!(root.is_absolute());assert_eq!(root.canonicalize().unwrap(),root);
        assert!(root.file_name().unwrap().to_str().unwrap().starts_with("registry12-hot-"));
        assert_eq!(std::fs::metadata(&root).unwrap().permissions().mode()&0o777,0o700);
        let db=Connection::open(root.join("hot.sqlite3")).unwrap();
        db.execute_batch("PRAGMA journal_mode=DELETE; PRAGMA synchronous=FULL; CREATE TABLE future_payload(value BLOB); INSERT INTO future_payload VALUES(zeroblob(1048576)); PRAGMA user_version=13; PRAGMA cache_size=4; PRAGMA cache_spill=ON; BEGIN IMMEDIATE; UPDATE future_payload SET value=randomblob(1048576);").unwrap();
        assert!(root.join("hot.sqlite3-journal").is_file());
        // The bounded original-child executor owns this exact process and its
        // wait/cleanup. No numeric PID lookup or fixture killing Drop exists.
        unsafe { libc::raise(libc::SIGKILL); }
        panic!("SIGKILL returned");
    }
}
