//! Private discovery evidence shared by terminal and native-provider holders.
//! Only process identities go to disk; recipes and environment stay in memory.

use std::{
    fs::{self, OpenOptions},
    io::{self, Read, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

use crate::model::{BootId, TerminalId};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum Kind {
    #[default]
    Terminal,
    Native,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Process {
    pub pid: i32,
    pub start: u64,
}

impl Process {
    pub fn read(pid: i32) -> io::Result<Self> {
        let stat = fs::read(format!("/proc/{pid}/stat"))?;
        let (_, start) = crate::model::agent_recovery::process_identity(&stat)
            .ok_or_else(|| io::Error::other("invalid process identity"))?;
        Ok(Self { pid, start })
    }
    pub fn is_current(&self) -> bool {
        Self::read(self.pid).is_ok_and(|actual| actual == *self)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Record {
    pub version: u32,
    /// Older terminal records predate native holders. Their existing private
    /// directory and absent kind continue to identify them as terminals.
    #[serde(default)]
    pub kind: Kind,
    pub application: String,
    pub boot: BootId,
    pub terminal: TerminalId,
    pub holder: Process,
    pub root: Process,
}

pub(crate) fn directory(kind: Kind) -> Result<PathBuf, String> {
    crate::daemon::RuntimePaths::discover()
        .map(|paths| {
            paths.directory.join(match kind {
                Kind::Terminal => "holders",
                Kind::Native => "native-holders",
            })
        })
        .map_err(|error| error.to_string())
}

pub(crate) fn socket(directory: &Path, terminal: TerminalId) -> PathBuf {
    directory.join(terminal.to_string())
}

/// A Unix address includes its terminating NUL. Filesystem paths have no such
/// small limit; Linux can resolve a long endpoint through a held directory FD.
pub(crate) fn socket_fits(path: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    // SAFETY: sockaddr_un is plain data and only its field length is read.
    let capacity = unsafe { std::mem::zeroed::<libc::sockaddr_un>() }
        .sun_path
        .len();
    path.as_os_str().as_bytes().len() < capacity
}

fn with_socket_address<T>(
    path: &Path,
    operation: impl FnOnce(&Path) -> io::Result<T>,
) -> io::Result<T> {
    use std::os::fd::AsRawFd;
    if socket_fits(path) {
        return operation(path);
    }
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "holder endpoint has no directory",
            )
        })?;
    let name = path.file_name().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "holder endpoint has no filename",
        )
    })?;
    crate::secure_file::reject_symlink_components(parent)?;
    let directory = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(parent)?;
    let metadata = directory.metadata()?;
    if !metadata.is_dir()
        || metadata.uid() != crate::daemon::current_uid()
        || metadata.mode() & 0o077 != 0
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "unsafe holder socket directory",
        ));
    }
    // Hold the FD until bind/connect completes. The endpoint remains at its
    // original filesystem path, so discovery, authentication, and inode-based
    // retirement retain their existing identity and survive daemon upgrades.
    let address = PathBuf::from(format!("/proc/self/fd/{}", directory.as_raw_fd())).join(name);
    operation(&address)
}

pub(crate) fn bind(path: &Path) -> io::Result<std::os::unix::net::UnixListener> {
    with_socket_address(path, |address| {
        std::os::unix::net::UnixListener::bind(address)
    })
}

pub(crate) fn connect(path: &Path) -> io::Result<std::os::unix::net::UnixStream> {
    with_socket_address(path, |address| {
        std::os::unix::net::UnixStream::connect(address)
    })
}

impl Record {
    fn supported_version(&self) -> bool {
        match self.kind {
            Kind::Terminal => matches!(self.version, 3 | 4),
            Kind::Native => self.version == 3,
        }
    }

    fn path(&self, directory: &Path) -> PathBuf {
        directory.join(format!("{}.json", self.terminal))
    }

    pub fn create(&self, directory: &Path) -> io::Result<()> {
        let path = self.path(directory);
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&path)?;
        let identity = file.metadata()?;
        let result = (|| {
            serde_json::to_writer(&mut file, self).map_err(io::Error::other)?;
            file.flush()?;
            file.sync_all()
        })();
        if result.is_err()
            && fs::symlink_metadata(&path).is_ok_and(|actual| {
                actual.dev() == identity.dev() && actual.ino() == identity.ino()
            })
        {
            let _ = fs::remove_file(path);
        }
        result
    }

    pub fn remove(&self, directory: &Path) {
        // A stale cleanup can never erase the evidence of a replacement.
        if read(&self.path(directory)).is_ok_and(|record| record == *self) {
            let _ = fs::remove_file(self.path(directory));
            let _ = fs::remove_file(socket(directory, self.terminal));
        }
    }

    /// Retire discovery after the owner has independently unlinked its exact
    /// socket inode. Never touch a successor endpoint published in between.
    pub fn remove_record(&self, directory: &Path) -> io::Result<()> {
        let path = self.path(directory);
        match read(&path) {
            Ok(record) if record == *self => match fs::remove_file(path) {
                Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
                result => result,
            },
            Ok(_) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }
}

fn read(path: &Path) -> io::Result<Record> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)?;
    read_file(file, false)
}

fn read_file(file: std::fs::File, allow_unlinked: bool) -> io::Result<Record> {
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || metadata.uid() != crate::daemon::current_uid()
        || metadata.mode() & 0o077 != 0
        || (metadata.nlink() != 1 && !(allow_unlinked && metadata.nlink() == 0))
        || metadata.len() > 4096
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "unsafe holder discovery record",
        ));
    }
    let mut bytes = Vec::new();
    file.take(4097).read_to_end(&mut bytes)?;
    serde_json::from_slice(&bytes).map_err(io::Error::other)
}

/// Check discovery retirement only after the caller authenticated an exact
/// teardown receipt. A just-opened record may already be unlinked when fstat
/// runs; reading that held inode still reports pending until the next poll.
/// It never grants adoption authority or turns unsafe evidence into absence.
pub(crate) fn retirement_pending(expected: &Record) -> io::Result<bool> {
    let path = expected.path(&directory(expected.kind).map_err(io::Error::other)?);
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    read_file(file, true).map(|actual| actual == *expected)
}

pub(crate) fn evidence(socket: &Path, kind: Kind) -> Result<Option<Record>, String> {
    let path = socket.with_extension("json");
    let record = match read(&path) {
        Ok(record) => record,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.to_string()),
    };
    let boot = crate::daemon::read_boot_id().map_err(|error| error.to_string())?;
    if !record.supported_version()
        || record.kind != kind
        || record.application != crate::instance::APPLICATION_ID
        || record.boot != boot
        || socket.file_name() != Some(std::ffi::OsStr::new(&record.terminal.to_string()))
    {
        return Err("holder discovery evidence does not match this endpoint".into());
    }
    Ok(Some(record))
}

pub(crate) fn discover(directory: &Path, boot: &BootId, kind: Kind) -> Result<Vec<Record>, String> {
    crate::secure_file::prepare_private_directory(directory).map_err(|error| error.to_string())?;
    let mut records = Vec::new();
    for (index, entry) in fs::read_dir(directory)
        .map_err(|error| error.to_string())?
        .enumerate()
    {
        if index >= 8192 {
            return Err("holder directory exceeds discovery limit".into());
        }
        let path = entry.map_err(|error| error.to_string())?.path();
        if path.extension().is_none_or(|extension| extension != "json") {
            continue;
        }
        let record = match read(&path) {
            Ok(record) => record,
            Err(error) => {
                tracing::warn!(%error, path = %path.display(), "ignored unsafe holder record");
                continue;
            }
        };
        if !record.supported_version()
            || record.kind != kind
            || record.application != crate::instance::APPLICATION_ID
            || record.path(directory) != path
            || &record.boot != boot
        {
            tracing::warn!(path = %path.display(), "ignored unrelated holder record");
            continue;
        }
        if !record.holder.is_current() {
            record.remove(directory);
            continue;
        }
        records.push(record);
        if records.len() > 4096 {
            return Err("holder inventory exceeds session limit".into());
        }
    }
    Ok(records)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn record() -> Record {
        let process = Process::read(std::process::id() as i32).unwrap();
        Record {
            version: 3,
            kind: Kind::Terminal,
            application: crate::instance::APPLICATION_ID.into(),
            boot: crate::daemon::read_boot_id().unwrap(),
            terminal: TerminalId::new(),
            holder: process.clone(),
            root: process,
        }
    }

    fn endpoint_with_byte_length(root: &Path, length: usize) -> PathBuf {
        use std::os::unix::ffi::OsStrExt;
        let prefix = root.as_os_str().as_bytes().len() + 1;
        // A multibyte component makes byte accounting distinct from characters.
        let directory = root.join(format!("é{}", "x".repeat(length - prefix - 2 - 1 - 36)));
        crate::secure_file::prepare_private_directory(&directory).unwrap();
        let endpoint = socket(&directory, TerminalId::new());
        assert_eq!(endpoint.as_os_str().as_bytes().len(), length);
        endpoint
    }

    #[test]
    fn unix_address_boundary_preserves_full_endpoint_and_discovery_identity() {
        let root = tempfile::tempdir_in("/var/tmp").unwrap();
        // SAFETY: plain data, only the array length is used.
        let capacity = unsafe { std::mem::zeroed::<libc::sockaddr_un>() }
            .sun_path
            .len();
        for length in [capacity - 1, capacity, capacity + 1, capacity + 100] {
            let endpoint = endpoint_with_byte_length(root.path(), length);
            assert_eq!(socket_fits(&endpoint), length < capacity);
            let listener = bind(&endpoint).unwrap();
            assert!(std::os::unix::fs::FileTypeExt::is_socket(
                &fs::symlink_metadata(&endpoint).unwrap().file_type()
            ));
            let client = connect(&endpoint).unwrap();
            let (server, _) = listener.accept().unwrap();
            assert!(client.peer_addr().unwrap().as_pathname().is_some());
            drop((client, server, listener));
            let mut proof = record();
            proof.terminal = endpoint
                .file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .parse()
                .unwrap();
            proof.create(endpoint.parent().unwrap()).unwrap();
            assert_eq!(
                evidence(&endpoint, Kind::Terminal).unwrap(),
                Some(proof.clone())
            );
            assert_eq!(
                discover(endpoint.parent().unwrap(), &proof.boot, Kind::Terminal).unwrap(),
                vec![proof.clone()]
            );
            proof.remove(endpoint.parent().unwrap());
            assert!(!endpoint.exists());
        }
    }

    #[test]
    fn long_endpoints_reject_public_or_symlinked_directories() {
        let root = tempfile::tempdir_in("/var/tmp").unwrap();
        let endpoint = endpoint_with_byte_length(root.path(), 160);
        let parent = endpoint.parent().unwrap();
        fs::set_permissions(parent, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(
            bind(&endpoint).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700)).unwrap();
        let link = root.path().join("l".repeat(100));
        std::os::unix::fs::symlink(parent, &link).unwrap();
        assert_eq!(
            bind(&link.join(endpoint.file_name().unwrap()))
                .unwrap_err()
                .kind(),
            io::ErrorKind::PermissionDenied
        );
    }

    #[test]
    fn authenticated_retirement_tolerates_only_a_just_unlinked_record() {
        let directory = tempfile::tempdir().unwrap();
        let proof = record();
        proof.create(directory.path()).unwrap();
        let path = proof.path(directory.path());
        let held = fs::File::open(&path).unwrap();
        // Deterministically retire after open and before metadata inspection.
        fs::remove_file(&path).unwrap();
        assert_eq!(held.metadata().unwrap().nlink(), 0);
        assert_eq!(
            read_file(held.try_clone().unwrap(), false)
                .unwrap_err()
                .kind(),
            io::ErrorKind::PermissionDenied
        );
        assert_eq!(read_file(held, true).unwrap(), proof);
        proof.create(directory.path()).unwrap();
        fs::hard_link(&path, directory.path().join("extra-link")).unwrap();
        assert_eq!(
            read_file(fs::File::open(&path).unwrap(), true)
                .unwrap_err()
                .kind(),
            io::ErrorKind::PermissionDenied
        );
        fs::remove_file(directory.path().join("extra-link")).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        let unsafe_held = fs::File::open(&path).unwrap();
        fs::remove_file(path).unwrap();
        assert_eq!(
            read_file(unsafe_held, true).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
    }

    #[test]
    fn record_only_retirement_preserves_replacement_endpoint_and_evidence() {
        let directory = tempfile::tempdir().unwrap();
        let old = record();
        old.create(directory.path()).unwrap();
        let endpoint = socket(directory.path(), old.terminal);
        fs::write(&endpoint, "replacement endpoint").unwrap();
        old.remove_record(directory.path()).unwrap();
        assert_eq!(
            fs::read_to_string(&endpoint).unwrap(),
            "replacement endpoint"
        );
        let mut successor = old.clone();
        successor.holder.start += 1;
        successor.create(directory.path()).unwrap();
        old.remove_record(directory.path()).unwrap();
        assert_eq!(read(&successor.path(directory.path())).unwrap(), successor);
        assert!(endpoint.exists());
    }

    #[test]
    fn terminal_receipt_protocol_is_discovered_without_widening_native_versions() {
        let directory = tempfile::tempdir().unwrap();
        let mut terminal = record();
        terminal.version = 4;
        terminal.create(directory.path()).unwrap();
        assert_eq!(
            evidence(&socket(directory.path(), terminal.terminal), Kind::Terminal).unwrap(),
            Some(terminal.clone())
        );
        assert_eq!(
            discover(directory.path(), &terminal.boot, Kind::Terminal).unwrap(),
            vec![terminal.clone()]
        );
        let mut native = record();
        native.kind = Kind::Native;
        native.version = 4;
        native.create(directory.path()).unwrap();
        assert!(evidence(&socket(directory.path(), native.terminal), Kind::Native).is_err());
        assert!(
            discover(directory.path(), &native.boot, Kind::Native)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn discovery_rejects_foreign_unsafe_and_reused_process_evidence() {
        let directory = tempfile::tempdir().unwrap();
        let valid = record();
        valid.create(directory.path()).unwrap();
        let mut foreign = record();
        foreign.application = "another-installation".into();
        foreign.create(directory.path()).unwrap();
        let mut old_boot = record();
        old_boot.boot = BootId::new("previous-boot");
        old_boot.create(directory.path()).unwrap();
        let unsafe_record = record();
        unsafe_record.create(directory.path()).unwrap();
        fs::set_permissions(
            unsafe_record.path(directory.path()),
            fs::Permissions::from_mode(0o666),
        )
        .unwrap();
        let mut stale = record();
        stale.holder.start += 1;
        stale.create(directory.path()).unwrap();
        fs::write(socket(directory.path(), stale.terminal), "stale endpoint").unwrap();
        let target = directory.path().join("untouched");
        fs::write(&target, "foreign file").unwrap();
        let link = directory.path().join(format!("{}.json", TerminalId::new()));
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert_eq!(
            discover(directory.path(), &valid.boot, Kind::Terminal).unwrap(),
            vec![valid]
        );
        assert!(foreign.path(directory.path()).exists());
        assert!(old_boot.path(directory.path()).exists());
        assert!(!stale.path(directory.path()).exists());
        assert!(!socket(directory.path(), stale.terminal).exists());
        assert_eq!(fs::read_to_string(target).unwrap(), "foreign file");
    }

    #[test]
    fn discovery_keeps_native_and_legacy_terminal_ownership_separate() {
        let directory = tempfile::tempdir().unwrap();
        let terminal = record();
        let mut legacy = serde_json::to_value(&terminal).unwrap();
        legacy.as_object_mut().unwrap().remove("kind");
        let path = terminal.path(directory.path());
        fs::write(&path, serde_json::to_vec(&legacy).unwrap()).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        let mut native = record();
        native.kind = Kind::Native;
        native.create(directory.path()).unwrap();
        assert_eq!(
            discover(directory.path(), &terminal.boot, Kind::Terminal).unwrap(),
            vec![terminal.clone()]
        );
        assert_eq!(
            discover(directory.path(), &terminal.boot, Kind::Native).unwrap(),
            vec![native.clone()]
        );
        assert!(evidence(&socket(directory.path(), terminal.terminal), Kind::Native).is_err());
        assert!(evidence(&socket(directory.path(), native.terminal), Kind::Terminal).is_err());
        assert!(native.path(directory.path()).exists());
        assert!(terminal.path(directory.path()).exists());
    }
}
