//! Private recovery data for the interactive user's exact channel-work request.
//!
//! A record is not authority and never causes a send. Every loaded record is
//! ambiguous, including one published just before an application interruption.
//! The advisory lock coordinates cooperating windows; it is not a sandbox
//! against a malicious process with the same uid.
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Component, Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    channel_work::{Change, Mutation, Receipt, Target},
    channel_work_api::Request,
    model::HostSessionProof,
};

pub(crate) const MAX_RECORDS: usize = 16;
const MAX_BYTES: usize = 64 * 1024;
const LOCK: &str = ".journal-lock";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Profile {
    pub(crate) name: String,
    pub(crate) host: String,
    pub(crate) executable: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Destination {
    /// None is the local backend. A remote profile is compared as a whole.
    pub(crate) profile: Option<Profile>,
    pub(crate) socket: PathBuf,
    pub(crate) proof: HostSessionProof,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Record {
    version: u8,
    mutation: Mutation,
    selection: Option<Uuid>,
    destination: Destination,
    title: String,
    created_ms: i64,
}

impl Record {
    pub(crate) fn new(
        mutation: Mutation,
        selection: Option<Uuid>,
        destination: Destination,
        title: String,
        created_ms: i64,
    ) -> Result<Self, Error> {
        let record = Self {
            version: if selection.is_some() { 2 } else { 1 },
            mutation,
            selection,
            destination,
            title,
            created_ms,
        };
        record.validate()?;
        Ok(record)
    }

    pub(crate) fn validate(&self) -> Result<(), Error> {
        if self
            .mutation
            .canonicalized()
            .map_err(|_| Error::InvalidRecord)?
            != self.mutation
            || !self.destination.socket.is_absolute()
            || self.destination.socket.as_os_str().as_encoded_bytes().len() > 4096
            || !self.destination.proof.is_complete()
            || !text(&self.title, 1024, true)
            || self.created_ms < 0
        {
            return Err(Error::InvalidRecord);
        }
        for value in [
            self.destination.proof.host_session_id.as_str(),
            self.destination.proof.boot_id.as_str(),
            self.destination.proof.host_incarnation.as_str(),
        ] {
            if !text(value, 512, false) {
                return Err(Error::InvalidRecord);
            }
        }
        if let Some(profile) = &self.destination.profile {
            for value in [&profile.name, &profile.host, &profile.executable] {
                if !text(value, 1024, false) {
                    return Err(Error::InvalidRecord);
                }
            }
        }
        // Both formats describe only the interactive User: wire_request always
        // supplies caller=None and the journal carries no execution authority.
        // Version 1 selection=None remains readable without rewriting it. Its
        // private, unsupported selection=Some shape must never become a v2
        // Local request merely because a newer reader recognizes that nonce.
        match (self.version, self.selection, &self.mutation.change) {
            (1, None, _) => {}
            (
                2,
                Some(selection),
                Change::Claim {
                    target: Target::Local(id),
                    ..
                },
            ) if !selection.is_nil() && !id.is_nil() => {}
            _ => return Err(Error::InvalidRecord),
        }
        if serde_json::to_vec(self)
            .map_err(|_| Error::InvalidRecord)?
            .len()
            > MAX_BYTES
        {
            return Err(Error::InvalidRecord);
        }
        Ok(())
    }

    pub(crate) fn mutation(&self) -> &Mutation {
        &self.mutation
    }
    pub(crate) fn selection(&self) -> Option<Uuid> {
        self.selection
    }
    pub(crate) fn operation(&self) -> Uuid {
        self.mutation.operation
    }
    pub(crate) fn destination(&self) -> &Destination {
        &self.destination
    }
    pub(crate) fn title(&self) -> &str {
        &self.title
    }
    pub(crate) fn created_ms(&self) -> i64 {
        self.created_ms
    }

    pub(crate) fn wire_request(&self) -> Result<Request, Error> {
        self.validate()?;
        Ok(match self.selection {
            Some(selection) => Request::ApplySelection {
                selection,
                mutation: self.mutation.clone(),
                caller: None,
            },
            None => Request::Mutate {
                mutation: self.mutation.clone(),
                caller: None,
            },
        })
    }

    pub(crate) fn confirms_receipt(&self, receipt: &Receipt) -> bool {
        self.validate().is_ok() && mutation_confirms_receipt(&self.mutation, receipt)
    }
}

fn text(value: &str, limit: usize, empty: bool) -> bool {
    (empty || !value.is_empty()) && value.len() <= limit && !value.chars().any(char::is_control)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Error {
    InvalidRecord,
    UnsafeStorage,
    Busy,
    Full,
    Conflict,
    Unavailable,
    /// Publication may be visible but its directory synchronization failed.
    /// No mutation should be sent on this error; restoration remains Unknown.
    PublicationUncertain,
    /// Removal may be visible but its directory synchronization failed.
    SettlementUncertain,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::InvalidRecord => "The saved operation is invalid",
            Self::UnsafeStorage => "The recovery journal is not private safe storage",
            Self::Busy => "Another window is updating the recovery journal",
            Self::Full => "The recovery journal has unresolved operations or storage problems",
            Self::Conflict => "The original saved operation does not match",
            Self::Unavailable => "The recovery journal is unavailable",
            Self::PublicationUncertain => "The request may have been saved; no mutation was sent",
            Self::SettlementUncertain => {
                "The confirmed operation's journal cleanup is not confirmed"
            }
        })
    }
}

/// A positive receipt proves only this canonical original operation's outcome.
pub(crate) fn mutation_confirms_receipt(mutation: &Mutation, receipt: &Receipt) -> bool {
    mutation
        .canonicalized()
        .is_ok_and(|canonical| canonical == *mutation)
        && receipt.home == mutation.home
        && receipt.operation == mutation.operation
        && mutation
            .expected_revision
            .checked_add(u64::from(receipt.changed))
            == Some(receipt.revision)
}

impl std::error::Error for Error {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Reservation {
    Published,
    AlreadyPresent,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ProblemKind {
    InvalidName,
    UnsafeFile,
    InvalidRecord,
    Unavailable,
    Overflow,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Problem {
    /// Only a parsed UUID, never an arbitrary filename or file contents.
    pub(crate) operation: Option<Uuid>,
    pub(crate) kind: ProblemKind,
}

#[derive(Clone, Debug)]
pub(crate) struct Loaded {
    pub(crate) records: Vec<Record>,
    pub(crate) problems: Vec<Problem>,
    /// At most MAX_RECORDS + 1; a lower bound if the directory has more entries.
    pub(crate) occupied: usize,
    pub(crate) saturated: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct Journal {
    path: PathBuf,
    #[cfg(test)]
    fail_directory_sync: bool,
}

impl Journal {
    /// Select explicit application storage. This does not inspect or create it.
    pub(crate) fn at(path: PathBuf) -> Result<Self, Error> {
        if !path.is_absolute()
            || path.as_os_str().as_encoded_bytes().len() > 4096
            || path.ancestors().count() > 128
            || path
                .components()
                .any(|part| matches!(part, Component::ParentDir))
        {
            return Err(Error::UnsafeStorage);
        }
        Ok(Self {
            path,
            #[cfg(test)]
            fail_directory_sync: false,
        })
    }

    pub(crate) fn reserve(&self, record: &Record) -> Result<Reservation, Error> {
        record.validate()?;
        let (_lock, directory) = self.lock()?;
        let path = self.record_path(record.operation());
        match self.read(&path, record.operation()) {
            Ok((stored, _)) => {
                return if stored == *record {
                    // Also repair a prior uncertain directory sync before send.
                    self.sync_directory(&directory)
                        .map_err(|_| Error::PublicationUncertain)?;
                    Ok(Reservation::AlreadyPresent)
                } else {
                    Err(Error::Conflict)
                };
            }
            Err(ReadError::Missing) => {}
            Err(_) => return Err(Error::Conflict),
        }
        let loaded = self.scan()?;
        if loaded.saturated || !loaded.problems.is_empty() {
            return Err(Error::Full);
        }
        let bytes = serde_json::to_vec(record).map_err(|_| Error::InvalidRecord)?;
        let mut staging = tempfile::Builder::new()
            .prefix(".pending-")
            .tempfile_in(&self.path)
            .map_err(|_| Error::Unavailable)?;
        // Correct the mode on this newly created FD only: a restrictive umask
        // must not publish an unreadable request. Existing records are never
        // chmod'ed during inspection or recovery.
        staging
            .as_file()
            .set_permissions(fs::Permissions::from_mode(0o600))
            .map_err(|_| Error::Unavailable)?;
        safe_file(staging.as_file()).map_err(|_| Error::UnsafeStorage)?;
        staging
            .write_all(&bytes)
            .and_then(|_| staging.as_file().sync_all())
            .map_err(|_| Error::Unavailable)?;
        let published = staging
            .persist_noclobber(&path)
            .map_err(|_| Error::PublicationUncertain)?;
        verify_publication(&published, &path)?;
        self.sync_directory(&directory)
            .map_err(|_| Error::PublicationUncertain)?;
        Ok(Reservation::Published)
    }

    pub(crate) fn load(&self) -> Result<Loaded, Error> {
        let (_lock, _directory) = self.lock()?;
        self.scan()
    }

    pub(crate) fn settle(&self, record: &Record, receipt: &Receipt) -> Result<(), Error> {
        if !record.confirms_receipt(receipt) {
            return Err(Error::Conflict);
        }
        let (_lock, directory) = self.lock()?;
        let path = self.record_path(record.operation());
        let (stored, identity) = match self.read(&path, record.operation()) {
            Ok(stored) => stored,
            Err(ReadError::Missing) => {
                // A second window can finish the same receipt after the first
                // removed its record. The positive receipt above proves the
                // outcome; absence alone never establishes no effect.
                return self
                    .sync_directory(&directory)
                    .map_err(|_| Error::SettlementUncertain);
            }
            Err(_) => return Err(Error::Conflict),
        };
        if stored != *record {
            return Err(Error::Conflict);
        }
        let current = fs::symlink_metadata(&path).map_err(|_| Error::Conflict)?;
        if !current.is_file() || (current.dev(), current.ino()) != identity || current.nlink() != 1
        {
            return Err(Error::Conflict);
        }
        // Cooperating writers hold this lock. Same-uid malicious replacement
        // between this identity check and unlink is outside this storage scope.
        fs::remove_file(&path).map_err(|_| Error::Unavailable)?;
        self.sync_directory(&directory)
            .map_err(|_| Error::SettlementUncertain)
    }

    /// Finish a recordless in-process request only after an exact positive
    /// receipt, when another cooperating window may already have settled it.
    /// Never reconstruct a Record or remove/adopt any existing directory entry.
    pub(crate) fn settle_absent(
        &self,
        original: &Mutation,
        receipt: &Receipt,
    ) -> Result<(), Error> {
        if !mutation_confirms_receipt(original, receipt) {
            return Err(Error::Conflict);
        }
        let (_lock, directory) = self.lock()?;
        match fs::symlink_metadata(self.record_path(original.operation)) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => self
                .sync_directory(&directory)
                .map_err(|_| Error::SettlementUncertain),
            _ => Err(Error::Conflict),
        }
    }

    fn sync_directory(&self, directory: &File) -> io::Result<()> {
        #[cfg(test)]
        if self.fail_directory_sync {
            return Err(io::Error::other(
                "injected directory synchronization failure",
            ));
        }
        directory.sync_all()
    }

    fn lock(&self) -> Result<(crate::instance::OwnedLock, File), Error> {
        crate::secure_file::prepare_private_directory(&self.path)
            .map_err(|_| Error::UnsafeStorage)?;
        let directory = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&self.path)
            .map_err(|_| Error::UnsafeStorage)?;
        let lock_path = self.path.join(LOCK);
        let mut options = OpenOptions::new();
        options
            .read(true)
            .write(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC);
        let file = match options.clone().create_new(true).open(&lock_path) {
            Ok(file) => {
                file.set_permissions(fs::Permissions::from_mode(0o600))
                    .and_then(|_| file.sync_all())
                    .map_err(|_| Error::Unavailable)?;
                file
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                options.open(&lock_path).map_err(|_| Error::UnsafeStorage)?
            }
            Err(_) => return Err(Error::UnsafeStorage),
        };
        safe_file(&file).map_err(|_| Error::UnsafeStorage)?;
        let lock = crate::instance::owned_file_lock(file, libc::LOCK_EX | libc::LOCK_NB).map_err(
            |error| {
                if error.kind() == io::ErrorKind::WouldBlock {
                    Error::Busy
                } else {
                    Error::Unavailable
                }
            },
        )?;
        // prepare_private_directory may have created more than the final
        // component. Persist every bounded parent link bottom-up, including on
        // a retry after a prior unsuccessful first initialization. No ancestor
        // permissions/configuration are changed, and no RPC occurs here.
        directory.sync_all().map_err(|_| Error::Unavailable)?;
        for ancestor in self.path.ancestors().skip(1) {
            OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
                .open(ancestor)
                .and_then(|parent| parent.sync_all())
                .map_err(|_| Error::Unavailable)?;
        }
        Ok((lock, directory))
    }

    fn record_path(&self, operation: Uuid) -> PathBuf {
        self.path.join(format!("{operation}.json"))
    }

    fn read(&self, path: &Path, operation: Uuid) -> Result<(Record, (u64, u64)), ReadError> {
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(path)
            .map_err(|error| {
                if error.kind() == io::ErrorKind::NotFound {
                    ReadError::Missing
                } else {
                    ReadError::Unsafe
                }
            })?;
        let metadata = safe_file(&file).map_err(|_| ReadError::Unsafe)?;
        if metadata.len() > MAX_BYTES as u64 {
            return Err(ReadError::Invalid);
        }
        let mut bytes = Vec::new();
        file.take(MAX_BYTES as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| ReadError::Unavailable)?;
        if bytes.len() > MAX_BYTES {
            return Err(ReadError::Invalid);
        }
        let value: serde_json::Value =
            serde_json::from_slice(&bytes).map_err(|_| ReadError::Invalid)?;
        let record: Record = serde_json::from_slice(&bytes).map_err(|_| ReadError::Invalid)?;
        if serde_json::to_value(&record).map_err(|_| ReadError::Invalid)? != value
            || record.operation() != operation
            || record.validate().is_err()
        {
            return Err(ReadError::Invalid);
        }
        Ok((record, (metadata.dev(), metadata.ino())))
    }

    fn scan(&self) -> Result<Loaded, Error> {
        let mut loaded = Loaded {
            records: Vec::new(),
            problems: Vec::new(),
            occupied: 0,
            saturated: false,
        };
        for entry in fs::read_dir(&self.path).map_err(|_| Error::Unavailable)? {
            let entry = entry.map_err(|_| Error::Unavailable)?;
            if entry.file_name() == LOCK {
                continue;
            }
            loaded.occupied += 1;
            if loaded.occupied > MAX_RECORDS {
                loaded.problems.push(Problem {
                    operation: None,
                    kind: ProblemKind::Overflow,
                });
                break;
            }
            let name = entry.file_name();
            let operation = name
                .to_str()
                .and_then(|name| name.strip_suffix(".json"))
                .and_then(|name| Uuid::parse_str(name).ok())
                .filter(|id| format!("{id}.json") == name.to_string_lossy());
            let Some(operation) = operation else {
                loaded.problems.push(Problem {
                    operation: None,
                    kind: ProblemKind::InvalidName,
                });
                continue;
            };
            match self.read(&entry.path(), operation) {
                Ok((record, _)) => loaded.records.push(record),
                Err(error) => loaded.problems.push(Problem {
                    operation: Some(operation),
                    kind: match error {
                        ReadError::Unsafe => ProblemKind::UnsafeFile,
                        ReadError::Invalid => ProblemKind::InvalidRecord,
                        ReadError::Missing | ReadError::Unavailable => ProblemKind::Unavailable,
                    },
                }),
            }
        }
        loaded.saturated = loaded.occupied >= MAX_RECORDS;
        loaded
            .records
            .sort_by_key(|record| (record.created_ms(), record.operation()));
        Ok(loaded)
    }
}

fn safe_file(file: &File) -> io::Result<fs::Metadata> {
    let metadata = file.metadata()?;
    // SAFETY: geteuid is a scalar query with no preconditions.
    if !metadata.is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o077 != 0
        || metadata.nlink() != 1
    {
        return Err(io::Error::from(io::ErrorKind::PermissionDenied));
    }
    Ok(metadata)
}

fn verify_publication(file: &File, path: &Path) -> Result<(), Error> {
    // tempfile's non-clobber fallback can successfully hard-link the target
    // even if unlinking the staging name fails. Never admit a send for a
    // published inode that our later private-file reader would reject.
    let actual = safe_file(file).map_err(|_| Error::PublicationUncertain)?;
    let named = fs::symlink_metadata(path).map_err(|_| Error::PublicationUncertain)?;
    if actual.mode() & 0o777 != 0o600
        || !named.is_file()
        || (actual.dev(), actual.ino()) != (named.dev(), named.ino())
    {
        return Err(Error::PublicationUncertain);
    }
    Ok(())
}

enum ReadError {
    Missing,
    Unsafe,
    Invalid,
    Unavailable,
}

#[cfg(test)]
mod tests;
