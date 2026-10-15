//! Read-only SQLite snapshot mechanics. These private constructors are used
//! only after actual Store/FD/account admission; paths and profile DTOs alone
//! are not exported read grants.
use super::{Error, Result, FileIdentity, SourceReadProofData, fs::Directory, lifetime::RevocationProbe};
use rusqlite::{Connection, OpenFlags, ffi};
use std::{ffi::CStr, fs::File, os::unix::fs::MetadataExt, time::{Duration, Instant}};
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
    let opened_path = db.path().ok_or(Error::Unsupported("SQLite export has no physical main database"))?;
    if std::path::Path::new(opened_path) != path {
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
fn guard(db: &Connection, proof: &SourceReadProofData) -> Result<()> {
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
        let db = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX | OpenFlags::SQLITE_OPEN_NOFOLLOW)?;
        db.busy_timeout(Duration::ZERO)?;
        let journal: String = db.pragma_query_value(None, "journal_mode", |row| row.get(0))?;
        if journal != "delete" { return Err(Error::Unsupported("Export requires the canonical DELETE journal profile")); }
        db.execute_batch("BEGIN DEFERRED")?;
        let snapshot = Self { db, directory, database, proof, probe, coverage, deadline: expires };
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
        if !self.db.is_autocommit() { let _ = self.db.execute_batch("ROLLBACK"); }
    }
}
