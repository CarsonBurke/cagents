//! Bounded, metadata-only intent and publication evidence for staged imports.
//!
//! The caller owns the account lifecycle/Store lease before entering this leaf.
//! This leaf never opens the Store, launches a command, or reconstructs creation
//! authority from a serialized receipt. Its own nonblocking lock is always the
//! innermost lock. Same-UID writers must honor that lock; hostile same-UID/root
//! replacement is outside the protected-directory filesystem contract.

use std::{
    ffi::OsStr,
    fs::{self, Metadata, OpenOptions},
    io::{self, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Component, Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{ImportPlan, PublicationReceipt};
use crate::{
    fs_safety::{self, Root},
    instance,
};

pub(crate) const MAX_RECORDS: usize = 16;
pub(crate) const MAX_RECORD_BYTES: u64 = 64 * 1024;
const VERSION: u32 = 1;
const LOCK: &str = ".journal-lock";

/// These are observations of completed local stages, never execution readiness.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Phase {
    Reserved,
    Prepared,
    Published,
    Registered,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Record {
    version: u32,
    plan: ImportPlan,
    receipt: Option<PublicationReceipt>,
    phase: Phase,
    uncertain: bool,
}

impl Record {
    pub fn plan(&self) -> &ImportPlan {
        &self.plan
    }
    pub fn receipt(&self) -> Option<&PublicationReceipt> {
        self.receipt.as_ref()
    }
    pub fn phase(&self) -> Phase {
        self.phase
    }
    pub fn uncertain(&self) -> bool {
        self.uncertain
    }

    fn validate(&self) -> Result<(), Error> {
        if self.version > VERSION {
            return Err(Error::Newer);
        }
        if self.version != VERSION || self.plan.validate().is_err() {
            return Err(Error::Corrupt);
        }
        if let Some(receipt) = &self.receipt {
            if receipt.validate().is_err()
                || receipt.operation() != self.plan.operation
                || receipt.size() != self.plan.observation.size()
                || self.phase == Phase::Reserved
            {
                return Err(Error::Corrupt);
            }
        } else if self.phase != Phase::Reserved {
            return Err(Error::Corrupt);
        }
        Ok(())
    }
}

/// Deliberately contains no OS errors, paths, credential bytes or provider text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub(crate) enum Error {
    #[error("The import intent or receipt is invalid")]
    Invalid,
    #[error("The import journal is unavailable or unsafe; preserve it for inspection")]
    UnsafeStorage,
    #[error("Another import journal operation is in progress")]
    Busy,
    #[error("The import journal is at capacity; existing evidence was retained")]
    Full,
    #[error("The import operation conflicts with its original intent or stage")]
    Conflict,
    #[error("The import journal contains invalid or unknown evidence; it was retained")]
    Corrupt,
    #[error("The import journal was written by a newer application; it was retained")]
    Newer,
    #[error("The import journal publication is uncertain; keep the original operation")]
    PublicationUncertain,
}

#[derive(Debug, Clone)]
pub(crate) struct Journal {
    path: PathBuf,
}

impl Journal {
    /// Pure selection. Only a final child of an existing private installation
    /// directory can be created; no arbitrary ancestor is created or repaired.
    pub fn at(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// Reserve the immutable original before any home/cache/Store publication.
    /// Repeating an identical intent returns its existing evidence, not a new
    /// operation and not authority to repeat any external stage.
    pub fn begin(&self, plan: &ImportPlan) -> Result<Record, Error> {
        plan.validate().map_err(|_| Error::Invalid)?;
        let directory = self.directory(true)?.ok_or(Error::UnsafeStorage)?;
        let _lock = lock(&directory)?;
        let records = scan(&directory)?;
        if let Some(record) = records.iter().find(|r| r.plan.operation == plan.operation) {
            if &record.plan != plan {
                return Err(Error::Conflict);
            }
            // Confirm even an idempotent retry following an uncertain dir sync.
            fs_safety::sync_directory(&directory).map_err(|_| Error::PublicationUncertain)?;
            return Ok(record.clone());
        }
        if records.len() >= MAX_RECORDS {
            return Err(Error::Full);
        }
        let record = Record {
            version: VERSION,
            plan: plan.clone(),
            receipt: None,
            phase: Phase::Reserved,
            uncertain: false,
        };
        persist(&directory, &record, None)?;
        Ok(record)
    }

    /// Accept only the actual prepared inode receipt. This is durable BEFORE
    /// pending registration and auth.json publication; it grants no inode owner.
    pub fn record_prepared(
        &self,
        plan: &ImportPlan,
        receipt: &PublicationReceipt,
    ) -> Result<Record, Error> {
        if receipt.validate().is_err()
            || receipt.operation() != plan.operation
            || receipt.size() != plan.observation.size()
        {
            return Err(Error::Invalid);
        }
        self.update(plan, |record| {
            if let Some(previous) = &record.receipt {
                if previous != receipt {
                    return Err(Error::Conflict);
                }
                return Ok(());
            }
            if record.phase != Phase::Reserved {
                return Err(Error::Conflict);
            }
            record.receipt = Some(receipt.clone());
            record.phase = Phase::Prepared;
            Ok(())
        })
    }

    pub fn mark_published(&self, plan: &ImportPlan) -> Result<Record, Error> {
        self.advance(plan, Phase::Prepared, Phase::Published)
    }

    pub fn mark_registered(&self, plan: &ImportPlan) -> Result<Record, Error> {
        self.advance(plan, Phase::Published, Phase::Registered)
    }

    /// Keep the highest completed stage and original evidence. This never
    /// claims that a failed publication rolled back or authorizes an auto-retry.
    pub fn mark_uncertain(&self, plan: &ImportPlan) -> Result<Record, Error> {
        self.update(plan, |record| {
            record.uncertain = true;
            Ok(())
        })
    }

    /// Read-only: no lock creation, mkdir, chmod, fsync, repair or cleanup.
    /// None means no record was observed, NOT proof that no external effect ran.
    pub fn status(&self, operation: Uuid) -> Result<Option<Record>, Error> {
        if operation.is_nil() {
            return Err(Error::Invalid);
        }
        let Some(directory) = self.directory(false)? else {
            return Ok(None);
        };
        read_record(&directory, operation).map(|record| record.map(|(record, _)| record))
    }

    /// A bounded read-only snapshot, not a transaction across concurrent phase
    /// updates. A malformed/newer/unknown entry refuses the entire snapshot.
    pub fn load(&self) -> Result<Vec<Record>, Error> {
        let Some(directory) = self.directory(false)? else {
            return Ok(Vec::new());
        };
        scan(&directory)
    }

    fn advance(&self, plan: &ImportPlan, from: Phase, to: Phase) -> Result<Record, Error> {
        self.update(plan, |record| {
            if record.phase == to {
                return Ok(());
            }
            if record.phase != from || record.receipt.is_none() {
                return Err(Error::Conflict);
            }
            record.phase = to;
            Ok(())
        })
    }

    fn update(
        &self,
        plan: &ImportPlan,
        change: impl FnOnce(&mut Record) -> Result<(), Error>,
    ) -> Result<Record, Error> {
        plan.validate().map_err(|_| Error::Invalid)?;
        let directory = self.directory(false)?.ok_or(Error::Conflict)?;
        let _lock = lock(&directory)?;
        // Unknown records block mutation, never get silently evicted/adopted.
        scan(&directory)?;
        let (mut record, original) =
            read_record(&directory, plan.operation)?.ok_or(Error::Conflict)?;
        if &record.plan != plan {
            return Err(Error::Conflict);
        }
        let before = record.clone();
        change(&mut record)?;
        record.validate()?;
        if record != before {
            persist(&directory, &record, Some(&original))?;
        } else {
            fs_safety::sync_directory(&directory).map_err(|_| Error::PublicationUncertain)?;
        }
        Ok(record)
    }

    fn directory(&self, create: bool) -> Result<Option<Root>, Error> {
        if !self.path.is_absolute()
            || self.path.as_os_str().as_encoded_bytes().len() > 4096
            || self.path.components().count() > 128
            || self
                .path
                .components()
                .any(|c| matches!(c, Component::ParentDir | Component::CurDir))
        {
            return Err(Error::Invalid);
        }
        let relative = self.path.strip_prefix("/").map_err(|_| Error::Invalid)?;
        let anchor = Root::open(Path::new("/")).map_err(|_| Error::UnsafeStorage)?;
        let (parent, name) = match fs_safety::open_parent_beneath(&anchor, relative) {
            Ok(value) => value,
            Err(error) if !create && error.is_not_found() => return Ok(None),
            Err(_) => return Err(Error::UnsafeStorage),
        };
        safe_directory(&parent)?;
        let directory = match fs_safety::open_directory_beneath(&parent, Path::new(&name)) {
            Ok(directory) => directory,
            Err(error) if error.is_not_found() && !create => return Ok(None),
            Err(error) if error.is_not_found() => {
                let creation =
                    fs_safety::create_directory_beneath(&parent, Path::new(&name), 0o700)
                        .map_err(|_| Error::UnsafeStorage)?;
                match &creation {
                    fs_safety::DirectoryCreation::Published(receipt) => {
                        // Only this actual fresh-directory receipt permits chmod.
                        fs_safety::set_directory_permissions(receipt.directory(), 0o700)
                            .map_err(|_| Error::PublicationUncertain)?;
                        if receipt.publication_error().is_some() {
                            return Err(Error::PublicationUncertain);
                        }
                    }
                    fs_safety::DirectoryCreation::Existing(_) => {}
                    fs_safety::DirectoryCreation::Unverified { .. } => {
                        return Err(Error::UnsafeStorage);
                    }
                }
                let directory = creation.into_directory();
                safe_directory(&directory)?;
                fs_safety::sync_directory(&directory)
                    .and_then(|()| fs_safety::sync_directory(&parent))
                    .map_err(|_| Error::PublicationUncertain)?;
                directory
            }
            Err(_) => return Err(Error::UnsafeStorage),
        };
        safe_directory(&directory)?;
        if create {
            fs_safety::sync_directory(&parent).map_err(|_| Error::PublicationUncertain)?;
        }
        Ok(Some(directory))
    }
}

fn safe_directory(root: &Root) -> Result<(), Error> {
    let meta = root.metadata().map_err(|_| Error::UnsafeStorage)?;
    if !meta.is_dir() || meta.uid() != unsafe { libc::geteuid() } || meta.mode() & 0o7777 != 0o700 {
        return Err(Error::UnsafeStorage);
    }
    Ok(())
}

fn safe_file(meta: &Metadata) -> Result<(), Error> {
    if !meta.is_file()
        || meta.uid() != unsafe { libc::geteuid() }
        || meta.mode() & 0o7777 != 0o600
        || meta.nlink() != 1
    {
        return Err(Error::UnsafeStorage);
    }
    Ok(())
}

fn same_file(a: &Metadata, b: &Metadata) -> bool {
    (
        a.dev(),
        a.ino(),
        a.len(),
        a.mtime(),
        a.mtime_nsec(),
        a.ctime(),
        a.ctime_nsec(),
    ) == (
        b.dev(),
        b.ino(),
        b.len(),
        b.mtime(),
        b.mtime_nsec(),
        b.ctime(),
        b.ctime_nsec(),
    )
}

fn name(operation: Uuid) -> String {
    format!("{operation}.json")
}

fn lock(directory: &Root) -> Result<instance::OwnedLock, Error> {
    let path = directory.process_path().join(LOCK);
    let file = match OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(&path)
    {
        Ok(file) => {
            file.set_permissions(fs::Permissions::from_mode(0o600))
                .map_err(|_| Error::UnsafeStorage)?;
            file
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            fs_safety::open_beneath_writable(directory, Path::new(LOCK))
                .map_err(|_| Error::UnsafeStorage)?
                .ok_or(Error::UnsafeStorage)?
        }
        Err(_) => return Err(Error::UnsafeStorage),
    };
    let held = file.metadata().map_err(|_| Error::UnsafeStorage)?;
    safe_file(&held)?;
    let guard =
        instance::owned_file_lock(file, libc::LOCK_EX | libc::LOCK_NB).map_err(|error| {
            if error.kind() == io::ErrorKind::WouldBlock {
                Error::Busy
            } else {
                Error::UnsafeStorage
            }
        })?;
    let named = fs_safety::metadata_beneath(directory, Path::new(LOCK))
        .map_err(|_| Error::UnsafeStorage)?;
    safe_file(&named)?;
    if !same_file(&held, &named) {
        return Err(Error::UnsafeStorage);
    }
    Ok(guard)
}

fn read_record(directory: &Root, operation: Uuid) -> Result<Option<(Record, Metadata)>, Error> {
    let filename = name(operation);
    let mut file = match fs_safety::open_beneath(directory, Path::new(&filename)) {
        Ok(file) => file,
        Err(error) if error.is_not_found() => return Ok(None),
        Err(_) => return Err(Error::UnsafeStorage),
    };
    let before = file.metadata().map_err(|_| Error::UnsafeStorage)?;
    safe_file(&before)?;
    if before.len() > MAX_RECORD_BYTES {
        return Err(Error::Corrupt);
    }
    let bytes =
        fs_safety::read_bounded(&mut file, MAX_RECORD_BYTES).map_err(|_| Error::UnsafeStorage)?;
    let after = file.metadata().map_err(|_| Error::UnsafeStorage)?;
    let named = fs_safety::metadata_beneath(directory, Path::new(&filename))
        .map_err(|_| Error::UnsafeStorage)?;
    safe_file(&after)?;
    safe_file(&named)?;
    if !same_file(&before, &after)
        || !same_file(&after, &named)
        || bytes.len() as u64 != before.len()
    {
        return Err(Error::UnsafeStorage);
    }
    if bytes.len() as u64 > MAX_RECORD_BYTES {
        return Err(Error::Corrupt);
    }
    #[derive(Deserialize)]
    struct Header {
        version: u32,
    }
    let header: Header = serde_json::from_slice(&bytes).map_err(|_| Error::Corrupt)?;
    if header.version > VERSION {
        return Err(Error::Newer);
    }
    let record: Record = serde_json::from_slice(&bytes).map_err(|_| Error::Corrupt)?;
    // Some shared nested metadata DTOs predate deny_unknown_fields. Do not
    // silently discard a future field and then rewrite the reduced intent.
    let stored: serde_json::Value = serde_json::from_slice(&bytes).map_err(|_| Error::Corrupt)?;
    let recognized = serde_json::to_value(&record).map_err(|_| Error::Corrupt)?;
    if stored != recognized {
        return Err(Error::Corrupt);
    }
    record.validate()?;
    if record.plan.operation != operation {
        return Err(Error::Corrupt);
    }
    Ok(Some((record, after)))
}

fn scan(directory: &Root) -> Result<Vec<Record>, Error> {
    let names = fs_safety::directory_entries_beneath(directory, Path::new(""), MAX_RECORDS + 1)
        .map_err(|_| Error::Full)?;
    let mut records = Vec::new();
    for filename in names {
        if filename == OsStr::new(LOCK) {
            let meta = fs_safety::metadata_beneath(directory, Path::new(&filename))
                .map_err(|_| Error::UnsafeStorage)?;
            safe_file(&meta)?;
            continue;
        }
        let operation = filename
            .to_str()
            .and_then(|value| value.strip_suffix(".json"))
            .and_then(|value| Uuid::parse_str(value).ok())
            .filter(|id| !id.is_nil())
            .ok_or(Error::Corrupt)?;
        if filename != OsStr::new(&name(operation)) {
            return Err(Error::Corrupt);
        }
        let (record, _) = read_record(directory, operation)?.ok_or(Error::Corrupt)?;
        records.push(record);
        if records.len() > MAX_RECORDS {
            return Err(Error::Full);
        }
    }
    records.sort_by_key(|record| record.plan.operation);
    Ok(records)
}

fn persist(directory: &Root, record: &Record, previous: Option<&Metadata>) -> Result<(), Error> {
    persist_with_sync(directory, record, previous, fs_safety::sync_directory)
}

fn persist_with_sync(
    directory: &Root,
    record: &Record,
    previous: Option<&Metadata>,
    sync: impl FnOnce(&Root) -> io::Result<()>,
) -> Result<(), Error> {
    let bytes = serde_json::to_vec(record).map_err(|_| Error::Invalid)?;
    if bytes.len() as u64 > MAX_RECORD_BYTES {
        return Err(Error::Invalid);
    }
    let mut file =
        fs_safety::create_anonymous_in(directory, 0o600).map_err(|_| Error::UnsafeStorage)?;
    file.set_permissions(fs::Permissions::from_mode(0o600))
        .map_err(|_| Error::UnsafeStorage)?;
    file.write_all(&bytes)
        .and_then(|()| file.sync_all())
        .map_err(|_| Error::UnsafeStorage)?;
    let filename = name(record.plan.operation);
    if let Some(previous) = previous {
        let current = fs_safety::metadata_beneath(directory, Path::new(&filename))
            .map_err(|_| Error::Conflict)?;
        safe_file(&current)?;
        if !same_file(previous, &current) {
            return Err(Error::Conflict);
        }
        let staging = format!(".update-{}", Uuid::new_v4());
        fs_safety::link_anonymous_new(directory, &file, OsStr::new(&staging))
            .map_err(|_| Error::UnsafeStorage)?;
        // After linking, any incomplete step retains evidence and stops. Never
        // erase an unknown staging record or report a rollback after rename.
        fs_safety::rename_within(directory, OsStr::new(&staging), OsStr::new(&filename))
            .map_err(|_| Error::PublicationUncertain)?;
    } else {
        fs_safety::link_anonymous_new(directory, &file, OsStr::new(&filename)).map_err(
            |error| {
                if error.is_already_exists() {
                    Error::Conflict
                } else {
                    Error::UnsafeStorage
                }
            },
        )?;
    }
    sync(directory).map_err(|_| Error::PublicationUncertain)?;
    let held = file.metadata().map_err(|_| Error::PublicationUncertain)?;
    let named = fs_safety::metadata_beneath(directory, Path::new(&filename))
        .map_err(|_| Error::PublicationUncertain)?;
    safe_file(&held)
        .and_then(|()| safe_file(&named))
        .map_err(|_| Error::PublicationUncertain)?;
    if !same_file(&held, &named) {
        return Err(Error::PublicationUncertain);
    }
    Ok(())
}

#[cfg(test)]
mod tests;
