//! Cache disposal only. This disposition never proves provider/helper death,
//! authorizes history deletion, or substitutes for the native retirement receipt.
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
};

const JOURNAL: &str = "actor-journal.sqlite";
const IDENTITY: &str = "actor-journal.identity";
const DISPOSITION: &str = "actor-journal.drained";
const MAX_DISPOSITION_BYTES: usize = 4096;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Identity {
    version: u32,
    nonce: uuid::Uuid,
    directory: (u64, u64),
    journal: (u64, u64),
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
struct FileVersion {
    bytes: u64,
    modified: (i64, i64),
    changed: (i64, i64),
}
impl FileVersion {
    fn of(metadata: &fs::Metadata) -> Self {
        Self {
            bytes: metadata.len(),
            modified: (metadata.mtime(), metadata.mtime_nsec()),
            changed: (metadata.ctime(), metadata.ctime_nsec()),
        }
    }
}

#[derive(Serialize, Deserialize)]
struct Disposition {
    identity: Identity,
    journal: FileVersion,
    semantic_drained: bool,
}

pub(crate) struct ActorJournalRetention {
    directory: PathBuf,
    identity: Identity,
}

impl ActorJournalRetention {
    /// Called after exclusive journal creation, before SQLite initialization or
    /// actor startup. Any failure/partial file defaults to preservation.
    pub(crate) fn create(directory: &Path, journal: &File) -> io::Result<Self> {
        crate::secure_file::validate_owned_directory(directory)?;
        let metadata = journal.metadata()?;
        let identity = Identity {
            version: 1,
            nonce: uuid::Uuid::new_v4(),
            directory: directory_identity(directory)?,
            journal: (metadata.dev(), metadata.ino()),
        };
        write_new(&directory.join(IDENTITY), &identity)?;
        File::open(directory)?.sync_all()?;
        Ok(Self {
            directory: directory.to_owned(),
            identity,
        })
    }

    /// The Journal owner has permanently fenced future semantic appends and
    /// proved its exact ACK/loss state under the same mutex. The caller writes
    /// the authenticated native retirement receipt before this cache permit.
    pub(crate) fn allow_removal(&self) -> io::Result<()> {
        self.allow_removal_with_sync(|| File::open(&self.directory)?.sync_all())
    }

    fn allow_removal_with_sync(
        &self,
        sync_directory: impl FnOnce() -> io::Result<()>,
    ) -> io::Result<()> {
        let identity: Identity = read_private(&self.directory.join(IDENTITY))?;
        if identity != self.identity || directory_identity(&self.directory)? != identity.directory {
            return Err(io::Error::other("Retained journal cache identity changed"));
        }
        let journal =
            crate::secure_file::open_existing_private_file_readonly(&self.directory.join(JOURNAL))?;
        journal.sync_all()?;
        let metadata = journal.metadata()?;
        if (metadata.dev(), metadata.ino()) != identity.journal {
            return Err(io::Error::other("Retained journal file identity changed"));
        }
        let disposition = Disposition {
            identity,
            journal: FileVersion::of(&metadata),
            semantic_drained: true,
        };
        let temporary = self
            .directory
            .join(format!(".actor-journal-drained-{}", uuid::Uuid::new_v4()));
        let result = (|| {
            write_new(&temporary, &disposition)?;
            fs::rename(&temporary, self.directory.join(DISPOSITION))?;
            sync_directory()
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
            let _ = fs::remove_file(self.directory.join(DISPOSITION));
            let _ = File::open(&self.directory).and_then(|directory| directory.sync_all());
        }
        result
    }
}

fn directory_identity(directory: &Path) -> io::Result<(u64, u64)> {
    crate::secure_file::validate_owned_directory(directory)?;
    let metadata = fs::symlink_metadata(directory)?;
    Ok((metadata.dev(), metadata.ino()))
}

fn write_new<T: Serialize>(path: &Path, value: &T) -> io::Result<()> {
    let bytes = serde_json::to_vec(value).map_err(io::Error::other)?;
    if bytes.len() > MAX_DISPOSITION_BYTES {
        return Err(io::Error::other("Oversized journal cache disposition"));
    }
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)?;
    file.write_all(&bytes)?;
    file.sync_all()
}

fn read_private<T: serde::de::DeserializeOwned>(path: &Path) -> io::Result<T> {
    let file = crate::secure_file::open_existing_private_file_readonly(path)?;
    let mut bytes = Vec::new();
    file.take((MAX_DISPOSITION_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_DISPOSITION_BYTES {
        return Err(io::Error::other("Oversized journal cache disposition"));
    }
    serde_json::from_slice(&bytes).map_err(io::Error::other)
}

/// No SQLite opens, liveness guesses, or unbounded directory/database reads.
/// Legacy/partial/malformed journals remain retained for explicit recovery.
pub(super) fn reclaimable(directory: &Path) -> io::Result<bool> {
    let journal = match fs::symlink_metadata(directory.join(JOURNAL)) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            for name in [
                IDENTITY,
                DISPOSITION,
                "actor-journal.sqlite-journal",
                "actor-journal.sqlite-wal",
                "actor-journal.sqlite-shm",
            ] {
                match fs::symlink_metadata(directory.join(name)) {
                    Ok(_) => return Ok(false),
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error),
                }
            }
            return Ok(true);
        }
        Err(error) => return Err(error),
    };
    if !journal.is_file() || journal.uid() != crate::daemon::current_uid() || journal.nlink() != 1 {
        return Ok(false);
    }
    let identity: Identity = match read_private(&directory.join(IDENTITY)) {
        Ok(value) => value,
        Err(_) => return Ok(false),
    };
    let disposition: Disposition = match read_private(&directory.join(DISPOSITION)) {
        Ok(value) => value,
        Err(_) => return Ok(false),
    };
    Ok(identity.version == 1
        && !identity.nonce.is_nil()
        && disposition.semantic_drained
        && identity == disposition.identity
        && directory_identity(directory)? == identity.directory
        && (journal.dev(), journal.ino()) == identity.journal
        && FileVersion::of(&journal) == disposition.journal)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn journal(directory: &Path) -> File {
        OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(directory.join(JOURNAL))
            .unwrap()
    }

    #[test]
    fn dead_owner_sweep_retains_partial_and_undrained_journals_until_exact_disposition() {
        let root = tempfile::tempdir().unwrap();
        let (directory, lease) = crate::transcript_cache::create(root.path()).unwrap();
        let mut file = journal(&directory);
        file.write_all(b"unacknowledged semantic evidence").unwrap();
        drop(lease); // Owner gone is not semantic ACK or cache-disposal authority.
        assert_eq!(
            crate::transcript_cache::sweep(root.path()).unwrap().count,
            0
        );
        assert_eq!(
            fs::read(directory.join(JOURNAL)).unwrap(),
            b"unacknowledged semantic evidence"
        );
        let retention = ActorJournalRetention::create(&directory, &file).unwrap();
        assert_eq!(
            crate::transcript_cache::sweep(root.path()).unwrap().count,
            0
        );
        retention.allow_removal().unwrap();
        assert_eq!(
            crate::transcript_cache::sweep(root.path()).unwrap().count,
            1
        );
        assert!(!directory.exists());
    }

    #[test]
    fn changed_journal_and_cross_cache_or_malformed_dispositions_never_authorize_removal() {
        let root = tempfile::tempdir().unwrap();
        let (first, _first_lease) = crate::transcript_cache::create(root.path()).unwrap();
        let (second, _second_lease) = crate::transcript_cache::create(root.path()).unwrap();
        let mut first_file = journal(&first);
        let second_file = journal(&second);
        let first_retention = ActorJournalRetention::create(&first, &first_file).unwrap();
        let _second_retention = ActorJournalRetention::create(&second, &second_file).unwrap();
        first_retention.allow_removal().unwrap();
        fs::copy(first.join(DISPOSITION), second.join(DISPOSITION)).unwrap();
        assert!(!reclaimable(&second).unwrap());
        fs::write(second.join(DISPOSITION), b"partial").unwrap();
        assert!(!reclaimable(&second).unwrap());
        first_file
            .write_all(b"late append invalidates the old file version")
            .unwrap();
        assert!(!reclaimable(&first).unwrap());
        assert!(!crate::transcript_cache::remove_if_reclaimable(&first).unwrap());
        assert!(first.join(JOURNAL).exists());
    }

    #[test]
    fn recreated_journal_and_interrupted_disposition_publication_preserve_evidence() {
        let root = tempfile::tempdir().unwrap();
        let (directory, _lease) = crate::transcript_cache::create(root.path()).unwrap();
        let file = journal(&directory);
        let retention = ActorJournalRetention::create(&directory, &file).unwrap();
        retention.allow_removal().unwrap();
        fs::remove_file(directory.join(JOURNAL)).unwrap();
        let replacement = journal(&directory);
        assert!(
            !reclaimable(&directory).unwrap(),
            "old disposition cannot authorize another journal inode"
        );
        drop(replacement);
        let (failed, _failed_lease) = crate::transcript_cache::create(root.path()).unwrap();
        let failed_file = journal(&failed);
        let failed_retention = ActorJournalRetention::create(&failed, &failed_file).unwrap();
        assert!(
            failed_retention
                .allow_removal_with_sync(|| Err(io::Error::other(
                    "injected directory fsync failure"
                )))
                .is_err()
        );
        assert!(!reclaimable(&failed).unwrap());
        assert!(!failed.join(DISPOSITION).exists());
    }
}
