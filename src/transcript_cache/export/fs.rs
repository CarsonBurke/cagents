//! Descriptor-relative private metadata I/O. Directory pins are filesystem
//! evidence, not account/source-read capabilities.
use super::{Error, FileIdentity, Result, types::MAX_RECORD};
use serde::{Serialize, de::DeserializeOwned};
use std::{ffi::{CString, OsStr, OsString}, fs::{File, OpenOptions}, io::{Read, Write},
    os::{fd::{AsRawFd, FromRawFd}, unix::{ffi::{OsStrExt, OsStringExt}, fs::{MetadataExt, OpenOptionsExt}}},
    path::{Path, PathBuf}, time::{Duration, Instant}};

pub(super) struct Directory { file: File, identity: FileIdentity }
pub(super) struct Locked { _file: File }
pub(super) enum Mode { Shared, Exclusive }

fn identity(metadata: &std::fs::Metadata) -> FileIdentity {
    FileIdentity { device: metadata.dev(), inode: metadata.ino() }
}
fn uid() -> u32 { unsafe { libc::geteuid() } }
fn name(value: &OsStr) -> Result<CString> {
    let bytes = value.as_bytes();
    if bytes.is_empty() || bytes == b"." || bytes == b".." || bytes.contains(&b'/') {
        return Err(Error::InvalidMetadata("Export metadata name is not a direct child"));
    }
    CString::new(bytes).map_err(|_| Error::InvalidMetadata("Export metadata name contains NUL"))
}
fn private_file(file: &File) -> Result<()> {
    let m = file.metadata()?;
    if !m.is_file() || m.uid() != uid() || m.mode() & 0o777 != 0o600 || m.nlink() != 1 {
        return Err(Error::Incomplete("Export metadata file has an unsafe owner, mode, type, or link count"));
    }
    Ok(())
}
impl Directory {
    pub(super) fn open(path: &Path) -> Result<Self> {
        let file = OpenOptions::new().read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC).open(path)?;
        Self::from_file(file)
    }
    pub(super) fn from_file(file: File) -> Result<Self> {
        let m = file.metadata()?;
        if !m.is_dir() || m.uid() != uid() || m.mode() & 0o777 != 0o700 || m.nlink() == 0 {
            return Err(Error::Incomplete("Export metadata directory must be owned, private, and linked"));
        }
        Ok(Self { identity: identity(&m), file })
    }
    pub(super) fn identity(&self) -> FileIdentity { self.identity }
    pub(super) fn fd(&self) -> &File { &self.file }
    pub(super) fn current_path(&self) -> Result<PathBuf> {
        let path = std::fs::read_link(format!("/proc/self/fd/{}", self.file.as_raw_fd()))?;
        if !path.is_absolute() || std::fs::canonicalize(&path)? != path
            || identity(&std::fs::metadata(&path)?) != self.identity {
            return Err(Error::Incomplete("Export metadata directory has no exact current locator"));
        }
        Ok(path)
    }
    pub(super) fn create_dir(&self, child: &OsStr) -> Result<Self> {
        let child = name(child)?;
        if unsafe { libc::mkdirat(self.file.as_raw_fd(), child.as_ptr(), 0o700) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let directory = self.open_dir(OsStr::from_bytes(child.to_bytes()))?;
        directory.file.sync_all()?;
        self.file.sync_all()?;
        Ok(directory)
    }
    pub(super) fn open_dir(&self, child: &OsStr) -> Result<Self> {
        let child = name(child)?;
        let fd = unsafe { libc::openat(self.file.as_raw_fd(), child.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC) };
        if fd < 0 { return Err(std::io::Error::last_os_error().into()); }
        Self::from_file(unsafe { File::from_raw_fd(fd) })
    }
    pub(super) fn open_file(&self, child: &OsStr, write: bool) -> Result<File> {
        let child = name(child)?;
        let access = if write { libc::O_RDWR } else { libc::O_RDONLY };
        let fd = unsafe { libc::openat(self.file.as_raw_fd(), child.as_ptr(), access | libc::O_NOFOLLOW | libc::O_CLOEXEC) };
        if fd < 0 { return Err(std::io::Error::last_os_error().into()); }
        let file = unsafe { File::from_raw_fd(fd) };
        private_file(&file)?;
        Ok(file)
    }
    fn create_file(&self, child: &OsStr) -> Result<File> {
        let child = name(child)?;
        let fd = unsafe { libc::openat(self.file.as_raw_fd(), child.as_ptr(), libc::O_RDWR | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC, 0o600) };
        if fd < 0 { return Err(std::io::Error::last_os_error().into()); }
        let file = unsafe { File::from_raw_fd(fd) };
        private_file(&file)?;
        Ok(file)
    }
    pub(super) fn create_lock(&self, child: &OsStr) -> Result<()> {
        self.create_file(child)?.sync_all()?;
        self.file.sync_all()?;
        Ok(())
    }
    pub(super) fn lock(&self, child: &OsStr, mode: Mode, deadline: Instant) -> Result<Locked> {
        // A fresh open-file description, never an upgrade of a duplicated SH.
        if Instant::now() >= deadline { return Err(Error::Deadline); }
        let file = self.open_file(child, true)?;
        let original = identity(&file.metadata()?);
        let operation = match mode { Mode::Shared => libc::LOCK_SH, Mode::Exclusive => libc::LOCK_EX };
        loop {
            if unsafe { libc::flock(file.as_raw_fd(), operation | libc::LOCK_NB) } == 0 { break; }
            let error = std::io::Error::last_os_error();
            if error.kind() != std::io::ErrorKind::WouldBlock && error.kind() != std::io::ErrorKind::Interrupted { return Err(error.into()); }
            if Instant::now() >= deadline { return Err(Error::Deadline); }
            std::thread::sleep(Duration::from_millis(2));
        }
        let named = self.open_file(child, false)?;
        if identity(&named.metadata()?) != original {
            return Err(Error::Incomplete("Export lock was replaced during admission"));
        }
        Ok(Locked { _file: file })
    }
    pub(super) fn read<T: DeserializeOwned>(&self, child: &OsStr) -> Result<(T, usize)> {
        let file = self.open_file(child, false)?;
        let original = identity(&file.metadata()?);
        let mut data = Vec::new();
        file.take((MAX_RECORD + 1) as u64).read_to_end(&mut data)?;
        if data.len() > MAX_RECORD { return Err(Error::Capacity); }
        let named = self.open_file(child, false)?;
        if identity(&named.metadata()?) != original { return Err(Error::Incomplete("Export record was replaced during read")); }
        Ok((serde_json::from_slice(&data)?, data.len()))
    }
    pub(super) fn names(&self) -> Result<Vec<OsString>> {
        let mut names = Vec::new();
        // Enumeration uses procfs only as a descriptor locator; each entry is
        // subsequently opened NOFOLLOW relative to this actual directory FD.
        for entry in std::fs::read_dir(format!("/proc/self/fd/{}", self.file.as_raw_fd()))? {
            let entry = entry?;
            names.push(entry.file_name());
            if names.len() > super::types::MAX_RECORDS + 8 { return Err(Error::Capacity); }
        }
        names.sort();
        Ok(names)
    }
    pub(super) fn atomic<T: Serialize>(&self, child: &OsStr, value: &T, replace: bool) -> Result<()> {
        let destination = name(child)?;
        let temp_name = OsString::from_vec(format!(".prepared-{}", uuid::Uuid::new_v4()).into_bytes());
        let temp = name(&temp_name)?;
        let mut file = self.create_file(&temp_name)?;
        let original = identity(&file.metadata()?);
        let result = (|| {
            let mut encoded = Limited { data: Vec::new() };
            serde_json::to_writer(&mut encoded, value)?;
            file.write_all(&encoded.data)?;
            file.sync_all()?;
            let flags = if replace { 0 } else { libc::RENAME_NOREPLACE };
            if unsafe { libc::renameat2(self.file.as_raw_fd(), temp.as_ptr(), self.file.as_raw_fd(), destination.as_ptr(), flags) } != 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            self.file.sync_all()?;
            Ok(())
        })();
        if result.is_err()
            && self.open_file(&temp_name, false).ok().and_then(|f| f.metadata().ok()).is_some_and(|m| identity(&m) == original) {
            // Remove only this operation's exact newly-created temporary file.
            unsafe { libc::unlinkat(self.file.as_raw_fd(), temp.as_ptr(), 0); }
        }
        result
    }
}
struct Limited { data: Vec<u8> }
impl Write for Limited {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > MAX_RECORD.saturating_sub(self.data.len()) { return Err(std::io::Error::from_raw_os_error(libc::EFBIG)); }
        self.data.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> { Ok(()) }
}
