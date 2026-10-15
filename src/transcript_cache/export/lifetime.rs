//! Independent cache-reader disposal interlock and poll-only Reset epoch.
//! Neither an eventfd nor a readers flock grants source/account read authority.
use super::{Error, Result, FileIdentity, fs::{Directory, Locked, Mode}, types::VERSION};
use serde::{Deserialize, Serialize};
use std::{ffi::OsStr, fs::File, io, os::fd::{AsRawFd, FromRawFd}, path::Path, time::Instant};
use uuid::Uuid;

const LOCK: &str = "readers.lock";
const HEADER: &str = "export-lifetime.json";
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Header { format: u32, nonce: Uuid, directory: FileIdentity, readers: FileIdentity }
fn identity(file: &File) -> Result<FileIdentity> {
    use std::os::unix::fs::MetadataExt;
    let metadata = file.metadata()?;
    Ok(FileIdentity { device: metadata.dev(), inode: metadata.ino() })
}

/// Initialization must run under the actual Store/creation owner, before any
/// exported lease. A missing half of a previous initialization is not repaired.
pub(super) fn initialize(directory: &Path) -> Result<()> {
    let dir = Directory::open(directory)?;
    dir.create_lock(OsStr::new(LOCK))?;
    let readers = identity(&dir.open_file(OsStr::new(LOCK), false)?)?;
    dir.atomic(OsStr::new(HEADER), &Header { format: VERSION, nonce: Uuid::new_v4(),
        directory: dir.identity(), readers }, false)
}
fn header(dir: &Directory) -> Result<Header> {
    let (header, _): (Header, _) = dir.read(OsStr::new(HEADER))?;
    if header.format != VERSION || header.nonce.is_nil() || header.directory != dir.identity()
        || header.readers != identity(&dir.open_file(OsStr::new(LOCK), false)?)? {
        return Err(Error::Incomplete("Export reader lifetime version or physical identity changed"));
    }
    Ok(header)
}
pub(super) struct ReaderHold { _directory: Directory, _lock: Locked, header: Header }
impl ReaderHold {
    pub(super) fn acquire(directory: &Path, deadline: Instant) -> Result<Self> {
        let dir = Directory::open(directory)?;
        let before = header(&dir)?;
        let lock = dir.lock(OsStr::new(LOCK), Mode::Shared, deadline)?;
        if header(&dir)? != before { return Err(Error::Incomplete("Export reader lifetime changed during admission")); }
        Ok(Self { _directory: dir, _lock: lock, header: before })
    }
    pub(super) fn cache_nonce(&self) -> Uuid { self.header.nonce }
    pub(super) fn duplicate_for_private_transfer(&self) -> Result<File> {
        self._lock.duplicate_for_private_transfer()
    }
}

/// A FRESH EX OFD held through removal; never upgrade a reader's duplicated SH.
/// The caller must ALSO own creation/writer exclusion and the exact semantic
/// journal disposition fence. This guard is not process/purge/retirement proof.
pub(super) struct DisposalHold { _directory: Directory, _lock: Locked }
pub(super) fn disposal(directory: &Path, deadline: Instant) -> Result<DisposalHold> {
    let dir = Directory::open(directory)?;
    let before = header(&dir)?;
    let lock = dir.lock(OsStr::new(LOCK), Mode::Exclusive, deadline)?;
    if header(&dir)? != before { return Err(Error::Incomplete("Export disposal lifetime changed during admission")); }
    Ok(DisposalHold { _directory: dir, _lock: lock })
}

/// One actual Store source generation owns this descriptor. A new generation
/// gets a NEW eventfd; the old one is never drained or reauthorized after Reset.
/// This kernel epoch has no From<UUID/path/JSON> read-capability conversion.
pub(crate) struct RevocationEpoch { incarnation: Uuid, signal: File }
pub(crate) struct RevocationProbe { incarnation: Uuid, signal: File }
impl RevocationEpoch {
    pub(crate) fn new(incarnation: Uuid) -> Result<Self> {
        if incarnation.is_nil() { return Err(Error::InvalidMetadata("Source revocation epoch has no Store incarnation")); }
        let fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
        if fd < 0 { return Err(io::Error::last_os_error().into()); }
        Ok(Self { incarnation, signal: unsafe { File::from_raw_fd(fd) } })
    }
    pub(crate) fn probe(&self) -> Result<RevocationProbe> {
        Ok(RevocationProbe { incarnation: self.incarnation, signal: self.signal.try_clone()? })
    }
    /// MUST succeed before Reset intent/body mutation. If signaling fails the
    /// Store must refuse that mutation; an in-RAM revoked flag cannot protect a
    /// reader whose only surviving authority is this transferred kernel FD.
    pub(crate) fn revoke(&self) -> Result<()> {
        let one = 1u64.to_ne_bytes();
        loop {
            let result = unsafe { libc::write(self.signal.as_raw_fd(), one.as_ptr().cast(), one.len()) };
            if result == one.len() as isize { return Ok(()); }
            let error = io::Error::last_os_error();
            if result < 0 && error.kind() == io::ErrorKind::Interrupted { continue; }
            if result < 0 && error.kind() == io::ErrorKind::WouldBlock {
                // A saturated eventfd is already permanently readable. Never
                // read it to make room: all readers rely on poll-only state.
                return match self.probe()?.check() {
                    Err(Error::Revoked) => Ok(()),
                    Err(error) => Err(error),
                    Ok(()) => Err(Error::Incomplete("Saturated source epoch was not revoked")),
                };
            }
            return Err(if result >= 0 { Error::Incomplete("Source revocation signal was a partial write") } else { error.into() });
        }
    }
}
impl RevocationProbe {
    pub(crate) fn incarnation(&self) -> Uuid { self.incarnation }
    pub(super) fn duplicate_for_private_transfer(&self) -> Result<File> { Ok(self.signal.try_clone()?) }
    pub(crate) fn check(&self) -> Result<()> {
        let mut fd = libc::pollfd { fd: self.signal.as_raw_fd(), events: libc::POLLIN, revents: 0 };
        loop {
            let result = unsafe { libc::poll(&mut fd, 1, 0) };
            if result < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted { continue; }
                return Err(error.into());
            }
            if result == 0 { return Ok(()); }
            if fd.revents & libc::POLLIN != 0 { return Err(Error::Revoked); }
            return Err(Error::Incomplete("Source revocation descriptor is unavailable"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    fn deadline() -> Instant { Instant::now() + Duration::from_millis(40) }
    #[test]
    fn reset_epoch_is_poll_only_sticky_and_survives_issuer_drop() {
        let token = Uuid::new_v4();
        let epoch = RevocationEpoch::new(token).unwrap();
        let first = epoch.probe().unwrap(); let second = epoch.probe().unwrap();
        assert_eq!(first.incarnation(), token); first.check().unwrap();
        epoch.revoke().unwrap(); epoch.revoke().unwrap(); drop(epoch);
        for _ in 0..4 {
            assert!(matches!(first.check(), Err(Error::Revoked)));
            assert!(matches!(second.check(), Err(Error::Revoked)));
        }
        let successor = RevocationEpoch::new(Uuid::new_v4()).unwrap();
        successor.probe().unwrap().check().unwrap();
        assert!(matches!(first.check(), Err(Error::Revoked)));
    }
    #[test]
    fn actual_shared_reader_blocks_fresh_ex_disposal_until_last_holder_drops() {
        let root = tempfile::tempdir_in("/var/tmp").unwrap(); initialize(root.path()).unwrap();
        let first = ReaderHold::acquire(root.path(), deadline()).unwrap();
        let second = ReaderHold::acquire(root.path(), deadline()).unwrap();
        assert_eq!(first.cache_nonce(), second.cache_nonce());
        assert!(matches!(disposal(root.path(), deadline()), Err(Error::Deadline)));
        drop(first); assert!(matches!(disposal(root.path(), deadline()), Err(Error::Deadline)));
        drop(second); let exclusive = disposal(root.path(), deadline()).unwrap();
        assert!(matches!(ReaderHold::acquire(root.path(), deadline()), Err(Error::Deadline)));
        drop(exclusive); ReaderHold::acquire(root.path(), deadline()).unwrap();
    }
    #[test]
    fn missing_or_replaced_reader_metadata_is_never_legacy_disposal_proof() {
        let root = tempfile::tempdir_in("/var/tmp").unwrap();
        assert!(disposal(root.path(), deadline()).is_err());
        initialize(root.path()).unwrap();
        std::fs::remove_file(root.path().join(HEADER)).unwrap();
        assert!(ReaderHold::acquire(root.path(), deadline()).is_err());
        assert!(disposal(root.path(), deadline()).is_err());
        assert!(initialize(root.path()).is_err(), "partial lifetime must not be silently repaired");
    }
}
