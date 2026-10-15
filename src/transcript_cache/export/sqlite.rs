//! Read-only SQLite snapshot mechanics. These private constructors are used
//! only after actual Store/FD/account admission; paths and profile DTOs alone
//! are not exported read grants.
use super::{Error, Result, FileIdentity, SourceReadProofData, fs::Directory, lifetime::RevocationProbe};
use rusqlite::{Connection, OpenFlags, ffi};
use std::{ffi::CStr, fs::File, os::unix::{ffi::OsStrExt, fs::MetadataExt}, time::{Duration, Instant}};
use uuid::Uuid;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Coverage { Known { partial_history: bool }, Unknown }
/// Only this module can construct a context for an owned live read snapshot.
/// Consumers shape PRIVATE bounded pages. They cannot claim publication until
/// the owner explicitly releases the snapshot and its final authority checks.
pub(crate) struct VerifiedReadContext<'a> {
    db: &'a Connection,
    incarnation: Uuid,
    coverage: Coverage,
}
impl VerifiedReadContext<'_> {
    pub(crate) fn connection(&self) -> &Connection { self.db }
    pub(crate) fn incarnation(&self) -> Uuid { self.incarnation }
    pub(crate) fn coverage(&self) -> Coverage { self.coverage }
}
fn deadline(deadline: Instant) -> Result<()> {
    if Instant::now() >= deadline { Err(Error::Deadline) } else { Ok(()) }
}
fn id(file: &File) -> Result<FileIdentity> {
    let metadata = file.metadata()?;
    Ok(FileIdentity { device: metadata.dev(), inode: metadata.ino() })
}
fn control(db: &Connection, operation: i32) -> Result<i32> {
    let mut value = 0i32;
    // The borrowed Connection is not shared concurrently. SQLite accepts a
    // live sqlite3*, a static schema name and the operation's documented int*.
    let result = unsafe { ffi::sqlite3_file_control(db.handle(), c"main".as_ptr(), operation, (&mut value as *mut i32).cast()) };
    if result != ffi::SQLITE_OK { return Err(Error::Unsupported("SQLite export file control is unavailable")); }
    Ok(value)
}
fn supported_vfs(db: &Connection) -> Result<()> {
    let mut name: *mut libc::c_char = std::ptr::null_mut();
    let result = unsafe { ffi::sqlite3_file_control(db.handle(), c"main".as_ptr(), ffi::SQLITE_FCNTL_VFSNAME, (&mut name as *mut *mut libc::c_char).cast()) };
    if result != ffi::SQLITE_OK || name.is_null() { return Err(Error::Unsupported("SQLite export VFS proof is unavailable")); }
    let supported = unsafe { CStr::from_ptr(name).to_bytes() == b"unix" };
    unsafe { ffi::sqlite3_free(name.cast()); }
    if supported { Ok(()) } else { Err(Error::Unsupported("SQLite export requires the supported unix VFS")) }
}
/// HAS_MOVED compares the pathname to SQLite's OWN opened inode. Together with
/// held cooperative N/reader/directory interlocks and the original DB pin, this
/// avoids treating a second unrelated stat as proof of what SQLite opened.
pub(super) fn verify_open_connection(db: &Connection, directory: &Directory, database: &File, reading: bool) -> Result<()> {
    directory.verify()?;
    let path = directory.current_path()?.join("transcript.sqlite");
    let current = directory.open_file(std::ffi::OsStr::new("transcript.sqlite"), false)?;
    if id(&current)? != id(database)? { return Err(Error::Incomplete("Export database pin changed")); }
    let opened_path = unsafe { ffi::sqlite3_db_filename(db.handle(), c"main".as_ptr()) };
    if opened_path.is_null() { return Err(Error::Unsupported("SQLite export has no physical main database")); }
    let opened_path = unsafe { CStr::from_ptr(opened_path).to_bytes() };
    if std::path::Path::new(std::ffi::OsStr::from_bytes(opened_path)) != path {
        return Err(Error::Incomplete("SQLite export opened a different locator"));
    }
    supported_vfs(db)?;
    if control(db, ffi::SQLITE_FCNTL_HAS_MOVED)? != 0 { return Err(Error::Incomplete("SQLite's opened export inode moved")); }
    let lock = control(db, ffi::SQLITE_FCNTL_LOCKSTATE)?;
    if reading && lock < ffi::SQLITE_LOCK_SHARED || !reading && lock != ffi::SQLITE_LOCK_NONE {
        return Err(Error::Incomplete("SQLite export snapshot lock state is not proven"));
    }
    if id(&directory.open_file(std::ffi::OsStr::new("transcript.sqlite"), false)?)? != id(database)? {
        return Err(Error::Incomplete("Export database identity changed after SQLite proof"));
    }
    Ok(())
}
pub(super) fn guard(db: &Connection, proof: &SourceReadProofData) -> Result<()> {
    if proof.raw_schema != 1 || proof.reset_schema != 1 || proof.text_schema != 1
        || proof.source_incarnation.is_nil() || proof.raw_owner_nonce.is_nil() {
        return Err(Error::Unsupported("Export source profile is unsupported"));
    }
    let (raw, nonce): (i64, String) = db.query_row("SELECT version,nonce FROM raw_source_schema WHERE singleton=1", [], |row| Ok((row.get(0)?, row.get(1)?)))?;
    let (reset, pending): (i64, i64) = db.query_row("SELECT version,pending FROM transcript_reset_schema WHERE singleton=1", [], |row| Ok((row.get(0)?, row.get(1)?)))?;
    let text: i64 = db.query_row("SELECT version FROM text_source_schema WHERE singleton=1", [], |row| row.get(0))?;
    if raw != 1 || reset != 1 || text != 1 || pending != 0 || nonce != proof.raw_owner_nonce.to_string() {
        return Err(Error::Revoked);
    }
    Ok(())
}

unsafe extern "C" fn sql_deadline(context: *mut libc::c_void) -> libc::c_int {
    // ReadSnapshot owns the stable Box until this handler is explicitly
    // removed before connection/box destruction. No concurrent connection use.
    let expires = unsafe { &*context.cast::<Instant>() };
    i32::from(Instant::now() >= *expires)
}

/// The owner MUST retain actual namespace/reader/attachment publication guards
/// outside this snapshot until its private page is written or suppressed.
/// No method here can issue a grant, send a public page or authorize disposal.
pub(super) struct ReadSnapshot<'a> {
    db: Connection,
    directory: &'a Directory,
    database: &'a File,
    proof: &'a SourceReadProofData,
    probe: &'a RevocationProbe,
    coverage: Coverage,
    deadline: Instant,
    _progress_deadline: Box<Instant>,
}
impl<'a> ReadSnapshot<'a> {
    pub(super) fn begin(directory: &'a Directory, database: &'a File,
        proof: &'a SourceReadProofData, probe: &'a RevocationProbe,
        coverage: Coverage, expires: Instant) -> Result<Self> {
        deadline(expires)?; probe.check()?;
        if probe.incarnation() != proof.source_incarnation { return Err(Error::Revoked); }
        let path = directory.current_path()?.join("transcript.sqlite");
        if id(&directory.open_file(std::ffi::OsStr::new("transcript.sqlite"), false)?)? != id(database)? {
            return Err(Error::Incomplete("Export database changed before SQLite open"));
        }
        // Use a REAL canonical locator, not /proc/self/fd with NOFOLLOW. Normal
        // sibling rollback-journal discovery remains enabled; no immutable=1,
        // recovery writes, Store creation or source initialization is allowed.
        let mut progress_deadline = Box::new(expires);
        let db = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX | OpenFlags::SQLITE_OPEN_NOFOLLOW)?;
        db.busy_timeout(Duration::ZERO)?;
        unsafe { ffi::sqlite3_progress_handler(db.handle(), 1024, Some(sql_deadline),
            (&mut *progress_deadline as *mut Instant).cast()); }
        let journal: String = db.pragma_query_value(None, "journal_mode", |row| row.get(0))?;
        if journal != "delete" { return Err(Error::Unsupported("Export requires the canonical DELETE journal profile")); }
        db.execute_batch("BEGIN DEFERRED")?;
        let snapshot = Self { db, directory, database, proof, probe, coverage,
            deadline: expires, _progress_deadline: progress_deadline };
        guard(&snapshot.db, proof)?; // First actual read establishes the SH lock.
        verify_open_connection(&snapshot.db, directory, database, true)?;
        probe.check()?; deadline(expires)?;
        Ok(snapshot)
    }
    pub(super) fn context(&self) -> Result<VerifiedReadContext<'_>> {
        deadline(self.deadline)?; self.probe.check()?;
        if self.db.is_autocommit() { return Err(Error::Incomplete("Export snapshot was released before query")); }
        Ok(VerifiedReadContext { db: &self.db, incarnation: self.proof.source_incarnation, coverage: self.coverage })
    }
    /// Final explicit release is part of authority. Error/drop closes and
    /// retires this connection; it is never returned to a reusable pool.
    pub(super) fn finish(self) -> Result<()> {
        deadline(self.deadline)?; self.probe.check()?;
        if self.db.is_autocommit() { return Err(Error::Incomplete("Export snapshot ended outside its owner")); }
        guard(&self.db, self.proof)?;
        verify_open_connection(&self.db, self.directory, self.database, true)?;
        self.db.execute_batch("ROLLBACK")?;
        if !self.db.is_autocommit() { return Err(Error::Incomplete("Export snapshot release was not confirmed")); }
        verify_open_connection(&self.db, self.directory, self.database, false)?;
        self.probe.check()?; deadline(self.deadline)
    }
}
impl Drop for ReadSnapshot<'_> {
    fn drop(&mut self) {
        unsafe { ffi::sqlite3_progress_handler(self.db.handle(), 0, None, std::ptr::null_mut()); }
        if !self.db.is_autocommit() { let _ = self.db.execute_batch("ROLLBACK"); }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::lifetime::{self, ReaderHold, RevocationEpoch};
    use std::{ffi::{OsStr, OsString}, os::{fd::AsRawFd, unix::ffi::OsStringExt}};
    struct Fixture {
        _root: tempfile::TempDir, directory: Directory, database: File,
        proof: SourceReadProofData, epoch: RevocationEpoch, _reader: ReaderHold,
    }
    fn expires() -> Instant { Instant::now() + Duration::from_secs(5) }
    impl Fixture {
        // A synthetic supported source-marker DB tests SQLite mechanics only.
        // It does not construct ActualStore/account/authenticated read grants.
        fn new(non_utf8: bool) -> Self {
            let root = tempfile::tempdir_in("/var/tmp").unwrap();
            let parent = Directory::open(root.path()).unwrap();
            let name = if non_utf8 { OsString::from_vec(b"cache-\xff".to_vec()) } else { "cache".into() };
            let directory = parent.create_dir(&name).unwrap();
            directory.create_lock(OsStr::new("transcript.sqlite")).unwrap();
            let path = directory.current_path().unwrap();
            let proof = SourceReadProofData { source_incarnation: Uuid::new_v4(), raw_owner_nonce: Uuid::new_v4(),
                raw_schema: 1, reset_schema: 1, text_schema: 1 };
            let db = Connection::open(path.join("transcript.sqlite")).unwrap();
            db.execute_batch("PRAGMA journal_mode=DELETE;
                CREATE TABLE raw_source_schema(singleton INTEGER PRIMARY KEY,version INTEGER NOT NULL,nonce TEXT NOT NULL);
                CREATE TABLE transcript_reset_schema(singleton INTEGER PRIMARY KEY,version INTEGER NOT NULL,pending INTEGER NOT NULL);
                CREATE TABLE text_source_schema(singleton INTEGER PRIMARY KEY,version INTEGER NOT NULL);
                CREATE TABLE payload(value TEXT NOT NULL);
                INSERT INTO payload VALUES('private body');
                INSERT INTO transcript_reset_schema VALUES(1,1,0);
                INSERT INTO text_source_schema VALUES(1,1);").unwrap();
            db.execute("INSERT INTO raw_source_schema VALUES(1,1,?1)", [proof.raw_owner_nonce.to_string()]).unwrap(); drop(db);
            let database = directory.open_file(OsStr::new("transcript.sqlite"), false).unwrap();
            lifetime::initialize(&path).unwrap();
            let reader = ReaderHold::acquire(&path, expires()).unwrap();
            let epoch = RevocationEpoch::new(proof.source_incarnation).unwrap();
            Self { _root: root, directory, database, proof, epoch, _reader: reader }
        }
        fn writer(&self) -> Connection {
            let writer = Connection::open(self.directory.current_path().unwrap().join("transcript.sqlite")).unwrap();
            writer.busy_timeout(Duration::ZERO).unwrap(); writer
        }
    }
    #[test]
    fn actual_readonly_snapshot_proves_inode_lock_and_explicit_release_including_unix_bytes() {
        for non_utf8 in [false,true] {
            let fixture = Fixture::new(non_utf8); let probe = fixture.epoch.probe().unwrap();
            let snapshot = ReadSnapshot::begin(&fixture.directory, &fixture.database, &fixture.proof, &probe,
                Coverage::Known { partial_history: true }, expires()).unwrap();
            let context = snapshot.context().unwrap();
            assert_eq!(context.incarnation(), fixture.proof.source_incarnation);
            assert_eq!(context.coverage(), Coverage::Known { partial_history: true });
            let private_body: String = context.connection().query_row("SELECT value FROM payload", [], |row| row.get(0)).unwrap();
            assert_eq!(private_body, "private body");
            assert!(context.connection().execute("UPDATE payload SET value='forbidden'", []).is_err());
            let writer = fixture.writer();
            assert!(writer.execute_batch("BEGIN IMMEDIATE; UPDATE payload SET value='new'; COMMIT").is_err(), "actual read lock must block commit");
            writer.execute_batch("ROLLBACK").unwrap();
            snapshot.finish().unwrap();
            writer.execute("UPDATE payload SET value='after release'", []).unwrap();
        }
    }
    #[test]
    fn revoked_reset_or_changed_owner_and_future_text_refuse_before_context() {
        let fixture = Fixture::new(false); let probe = fixture.epoch.probe().unwrap();
        let writer = fixture.writer();
        for mutation in ["UPDATE transcript_reset_schema SET pending=1", "UPDATE raw_source_schema SET nonce='00000000-0000-0000-0000-000000000000'", "UPDATE text_source_schema SET version=2"] {
            writer.execute_batch(mutation).unwrap();
            assert!(ReadSnapshot::begin(&fixture.directory, &fixture.database, &fixture.proof, &probe, Coverage::Unknown, expires()).is_err());
            writer.execute("UPDATE raw_source_schema SET nonce=?1", [fixture.proof.raw_owner_nonce.to_string()]).unwrap();
            writer.execute_batch("UPDATE transcript_reset_schema SET pending=0; UPDATE text_source_schema SET version=1").unwrap();
        }
    }
    #[test]
    fn actual_replacement_and_kernel_revocation_prevent_private_page_release() {
        for replace in [false,true] {
            let fixture = Fixture::new(false); let probe = fixture.epoch.probe().unwrap();
            let snapshot = ReadSnapshot::begin(&fixture.directory, &fixture.database, &fixture.proof, &probe, Coverage::Unknown, expires()).unwrap();
            let context = snapshot.context().unwrap();
            assert_eq!(context.coverage(), Coverage::Unknown, "no durable completeness claim is invented");
            let _: String = context.connection().query_row("SELECT value FROM payload", [], |row| row.get(0)).unwrap();
            if replace {
                let path = fixture.directory.current_path().unwrap();
                std::fs::rename(path.join("transcript.sqlite"), path.join("old.sqlite")).unwrap();
                fixture.directory.create_lock(OsStr::new("transcript.sqlite")).unwrap();
            } else { fixture.epoch.revoke().unwrap(); }
            assert!(snapshot.finish().is_err(), "caller must suppress its still-private page");
        }
    }
    #[test]
    fn bundled_nofollow_rejects_proc_route_and_owner_detects_early_release() {
        let fixture = Fixture::new(false);
        let proc_path = format!("/proc/self/fd/{}/transcript.sqlite", fixture.directory.fd().as_raw_fd());
        assert!(Connection::open_with_flags(proc_path, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NOFOLLOW).is_err());
        let probe = fixture.epoch.probe().unwrap();
        let snapshot = ReadSnapshot::begin(&fixture.directory, &fixture.database, &fixture.proof, &probe, Coverage::Unknown, expires()).unwrap();
        snapshot.context().unwrap().connection().execute_batch("ROLLBACK").unwrap();
        assert!(snapshot.finish().is_err(), "query code cannot claim release on the owner's behalf");
    }
    #[test]
    fn original_absolute_deadline_interrupts_query_work_without_renewal() {
        let fixture = Fixture::new(false); let probe = fixture.epoch.probe().unwrap();
        let expires = Instant::now() + Duration::from_millis(150);
        let snapshot = ReadSnapshot::begin(&fixture.directory, &fixture.database, &fixture.proof, &probe, Coverage::Unknown, expires).unwrap();
        let result = snapshot.context().unwrap().connection().query_row(
            "WITH RECURSIVE growing(n) AS (VALUES(1) UNION ALL SELECT n+1 FROM growing WHERE n<100000000) SELECT sum(n) FROM growing", [], |row| row.get::<_, i64>(0));
        assert!(matches!(result, Err(rusqlite::Error::SqliteFailure(error, _)) if error.code == rusqlite::ErrorCode::OperationInterrupted));
        assert!(snapshot.finish().is_err());
        // A private query timeout must still release the actual SQLite lock.
        fixture.writer().execute("UPDATE payload SET value='after timed-out read'", []).unwrap();
    }

}
