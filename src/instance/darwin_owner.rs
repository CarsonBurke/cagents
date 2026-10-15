//! Private v2 record-lock mechanism. This does not authenticate a product,
//! process incarnation, boot, or inventory completeness. The caller must add
//! those proofs before any protected operation. It never proves BSD flock
//! ownership. No production caller is enabled by this module alone.
use std::{
    ffi::{CString, OsStr},
    fs::File,
    io::{self, Read, Write},
    os::{fd::{AsRawFd, FromRawFd}, unix::{ffi::OsStrExt, fs::MetadataExt}},
    sync::{Arc, Mutex},
};

const MAX_RECORD_BYTES: usize = 4096;
#[derive(Default)]
struct Mode { owner: bool, readers: usize }
static MODE: Mutex<Mode> = Mutex::new(Mode { owner: false, readers: 0 });

fn busy() -> io::Error {
    io::Error::new(io::ErrorKind::WouldBlock, "this process already owns or is inspecting a v2 owner witness")
}
fn mode() -> io::Result<std::sync::MutexGuard<'static, Mode>> {
    MODE.lock().map_err(|_| io::Error::other("v2 owner witness mode is uncertain"))
}
struct OwnerMode { process: u32 }
impl OwnerMode {
    fn acquire() -> io::Result<Self> {
        let mut state = mode()?;
        if state.owner || state.readers != 0 { return Err(busy()); }
        state.owner = true;
        Ok(Self { process: std::process::id() })
    }
}
impl Drop for OwnerMode {
    fn drop(&mut self) {
        if self.process == std::process::id() && let Ok(mut state) = MODE.lock() {
            state.owner = false;
        }
    }
}

/// POSIX closes release all this process's locks on an inode. A census must
/// therefore be excluded BEFORE it can open any witness, even on an error or
/// reentrant call. Keep this guard until every inspected descriptor is gone.
pub(super) struct Census { process: u32 }
impl Census {
    pub(super) fn begin() -> io::Result<Self> {
        let mut state = mode()?;
        if state.owner { return Err(busy()); }
        state.readers = state.readers.checked_add(1).ok_or_else(busy)?;
        Ok(Self { process: std::process::id() })
    }
    pub(super) fn inspect<'a>(&'a self, directory: &'a File, entry: &OsStr) -> io::Result<Option<Observation<'a>>> {
        if self.process != std::process::id() { return Err(io::Error::other("a fork cannot inherit witness census authority")); }
        validate_directory(directory)?;
        let entry = entry_name(entry)?;
        let mut file = open_at(directory, &entry, libc::O_RDONLY)?;
        let before = version(&file)?;
        let Some(owner) = lock_owner(&file)? else { return Ok(None); };
        if owner == std::process::id() { return Err(io::Error::other("self-owned record lock is not an external census proof")); }
        let mut bytes = Vec::new();
        (&mut file).take((MAX_RECORD_BYTES + 1) as u64).read_to_end(&mut bytes)?;
        if bytes.is_empty() || bytes.len() > MAX_RECORD_BYTES { return Err(io::Error::other("invalid v2 owner record size")); }
        let observation = Observation { file, directory, entry, owner, bytes, version: before, _census: self };
        observation.revalidate()?;
        Ok(Some(observation))
    }
}
impl Drop for Census {
    fn drop(&mut self) {
        if self.process == std::process::id() && let Ok(mut state) = MODE.lock() {
            state.readers = state.readers.saturating_sub(1);
        }
    }
}

/// One private original FD: no Deref, clone, descriptor export, or reopening.
/// The mode stays owned until the original descriptor has actually closed.
pub(super) struct RecordLease {
    file: Option<File>,
    directory: Arc<File>,
    entry: CString,
    mode: OwnerMode,
}
impl RecordLease {
    pub(super) fn create(directory: Arc<File>, entry: &OsStr, bytes: &[u8]) -> io::Result<Self> {
        if bytes.is_empty() || bytes.len() > MAX_RECORD_BYTES { return Err(io::Error::other("invalid v2 owner record size")); }
        let entry = entry_name(entry)?;
        let mode = OwnerMode::acquire()?;
        validate_directory(&directory)?;
        let file = open_at(&directory, &entry, libc::O_RDWR | libc::O_CREAT | libc::O_EXCL)?;
        let mut lease = Self { file: Some(file), directory, entry, mode };
        let file = lease.file.as_mut().expect("original descriptor remains owned");
        version(file)?;
        let mut lock: libc::flock = unsafe { std::mem::zeroed() };
        lock.l_type = libc::F_WRLCK as _;
        lock.l_whence = libc::SEEK_SET as _;
        lock.l_start = 0;
        lock.l_len = 1;
        // SAFETY: only this original private FD is exposed to F_SETLK.
        if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETLK, &lock) } == -1 {
            return Err(io::Error::last_os_error());
        }
        file.write_all(bytes)?;
        file.sync_all()?;
        lease.directory.sync_all()?;
        Ok(lease)
    }
}
impl Drop for RecordLease {
    fn drop(&mut self) {
        if let Some(file) = self.file.take() {
            // A fork inherits a descriptor, not its parent's POSIX lock. It
            // cannot unlink the parent's witness or alter its mode guard.
            if self.mode.process == std::process::id()
                && version(&file).is_ok()
                && named_matches(&self.directory, &self.entry, &file).unwrap_or(false)
            {
                // Only private coordination metadata is removed, never a chat,
                // provider, cache or account path. A failed unlink stays stale.
                unsafe { libc::unlinkat(self.directory.as_raw_fd(), self.entry.as_ptr(), 0); }
            }
            drop(file); // MUST precede OwnerMode::drop, including failure paths.
        }
    }
}

#[derive(PartialEq, Eq)]
struct Version { device: u64, inode: u64, bytes: u64, modified: (i64, i64), changed: (i64, i64) }
fn version(file: &File) -> io::Result<Version> {
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.nlink() != 1 || metadata.mode() & 0o077 != 0
    { return Err(io::Error::other("v2 witness is not a private owned regular file")); }
    Ok(Version {device: metadata.dev(), inode: metadata.ino(), bytes: metadata.len(),
        modified:(metadata.mtime(),metadata.mtime_nsec()), changed:(metadata.ctime(),metadata.ctime_nsec())})
}
fn validate_directory(directory: &File) -> io::Result<()> {
    let metadata = directory.metadata()?;
    if !metadata.is_dir() || metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o077 != 0 {
        return Err(io::Error::other("v2 witness directory is not private and owned"));
    }
    Ok(())
}
fn entry_name(entry: &OsStr) -> io::Result<CString> {
    let bytes = entry.as_bytes();
    if bytes.is_empty() || bytes.len() > 200 || bytes == b"." || bytes == b".." || bytes.contains(&b'/') {
        return Err(io::Error::other("invalid v2 witness directory entry"));
    }
    CString::new(bytes).map_err(|_| io::Error::other("invalid v2 witness directory entry"))
}
fn open_at(directory: &File, entry: &CString, flags: i32) -> io::Result<File> {
    // O_NONBLOCK precedes fstat: a replaced FIFO must never block a census.
    let raw = unsafe { libc::openat(directory.as_raw_fd(), entry.as_ptr(),
        flags | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK, 0o600 as libc::c_uint) };
    if raw < 0 { return Err(io::Error::last_os_error()); }
    Ok(unsafe { File::from_raw_fd(raw) })
}
fn lock_owner(file: &File) -> io::Result<Option<u32>> {
    let mut lock: libc::flock = unsafe { std::mem::zeroed() };
    lock.l_type = libc::F_WRLCK as _;
    lock.l_whence = libc::SEEK_SET as _;
    lock.l_start = 0;
    lock.l_len = 1;
    if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETLK, &mut lock) } == -1 {
        return Err(io::Error::last_os_error());
    }
    if lock.l_type == libc::F_UNLCK as _ { return Ok(None); }
    if lock.l_type != libc::F_WRLCK as _ || lock.l_whence != libc::SEEK_SET as _
        || lock.l_start != 0 || lock.l_len != 1 || lock.l_pid <= 1
    { return Err(io::Error::other("v2 owner record has no exact exclusive process-lock owner")); }
    Ok(Some(lock.l_pid as u32))
}
fn named_matches(directory: &File, entry: &CString, file: &File) -> io::Result<bool> {
    let expected = file.metadata()?;
    let mut actual: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstatat(directory.as_raw_fd(), entry.as_ptr(), &mut actual, libc::AT_SYMLINK_NOFOLLOW) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(actual.st_dev as u64 == expected.dev() && actual.st_ino as u64 == expected.ino()
        && actual.st_mode & libc::S_IFMT == libc::S_IFREG && actual.st_uid == expected.uid()
        && actual.st_nlink == 1)
}

/// A mechanism observation, NOT role/lifetime/cleanup authority. The caller
/// must authenticate its payload against an actual retained process identity,
/// boot and product, and perform a complete relevant-process inventory.
pub(super) struct Observation<'a> {
    file: File,
    directory: &'a File,
    entry: CString,
    owner: u32,
    bytes: Vec<u8>,
    version: Version,
    _census: &'a Census,
}
impl Observation<'_> {
    pub(super) fn pid(&self) -> u32 { self.owner }
    pub(super) fn bytes(&self) -> &[u8] { &self.bytes }
    pub(super) fn revalidate(&self) -> io::Result<()> {
        if self._census.process != std::process::id() || version(&self.file)? != self.version
            || !named_matches(self.directory, &self.entry, &self.file)?
            || lock_owner(&self.file)? != Some(self.owner)
        { return Err(io::Error::other("v2 owner witness changed during census")); }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    static SERIAL: Mutex<()> = Mutex::new(());
    fn directory() -> (tempfile::TempDir, Arc<File>) {
        let root = tempfile::tempdir().unwrap();
        let directory = Arc::new(File::open(root.path()).unwrap());
        (root, directory)
    }
    #[test]
    fn owner_and_all_reentrant_censuses_exclude_each_other_before_open() {
        let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
        let (root, directory) = directory();
        let owner = RecordLease::create(directory.clone(), OsStr::new("owner.v2"), b"private payload").unwrap();
        assert!(Census::begin().is_err());
        assert!(RecordLease::create(directory.clone(), OsStr::new("other.v2"), b"other").is_err());
        assert!(!root.path().join("other.v2").exists());
        drop(owner);
        assert!(!root.path().join("owner.v2").exists());
        let first = Census::begin().unwrap();
        let second = Census::begin().unwrap();
        assert!(RecordLease::create(directory.clone(), OsStr::new("other.v2"), b"other").is_err());
        drop(first);
        assert!(RecordLease::create(directory.clone(), OsStr::new("other.v2"), b"other").is_err());
        drop(second);
        let _owner = RecordLease::create(directory, OsStr::new("owner.v2"), b"new payload").unwrap();
    }
    #[test]
    fn malformed_nodes_and_plain_open_files_grant_no_owner() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
        let (root, directory) = directory();
        std::fs::write(root.path().join("plain.v2"), b"unlocked").unwrap();
        std::fs::set_permissions(root.path().join("plain.v2"), std::fs::Permissions::from_mode(0o600)).unwrap();
        symlink(root.path().join("plain.v2"), root.path().join("symlink.v2")).unwrap();
        let fifo = CString::new(root.path().join("fifo.v2").as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        let census = Census::begin().unwrap();
        assert!(census.inspect(&directory, OsStr::new("plain.v2")).unwrap().is_none());
        assert!(census.inspect(&directory, OsStr::new("symlink.v2")).is_err());
        let started = std::time::Instant::now();
        assert!(census.inspect(&directory, OsStr::new("fifo.v2")).is_err());
        assert!(started.elapsed() < std::time::Duration::from_millis(200));
        assert!(census.inspect(&directory, OsStr::new("../plain.v2")).is_err());
        assert!(root.path().join("plain.v2").exists());
    }
}
