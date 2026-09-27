//! Read-only recovery data. A cache never contains review mutation authority.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::{
    github::{PullRequest, PullRequestKey, pull_request_number},
    repository::{DiffEntry, PullRequestRange, RepoPath},
};

pub const MAX_CACHED_REVIEWS: usize = 32;
pub const MAX_CACHED_FILES: usize = 10_000;
pub const MAX_CACHE_BYTES: u64 = 32 * 1024 * 1024;

pub(crate) fn write_epoch() -> &'static str {
    static EPOCH: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    EPOCH.get_or_init(|| uuid::Uuid::new_v4().to_string())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct File {
    pub entry: DiffEntry,
    pub viewed: bool,
    /// An optimistic mark whose acknowledgement was not saved before exit.
    pub unverified: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    pub key: PullRequestKey,
    pub viewer: String,
    pub pull_request: PullRequest,
    pub range: PullRequestRange,
    pub files: Vec<File>,
    pub selected: Option<RepoPath>,
    pub saved_ms: u64,
}

impl Snapshot {
    pub fn validate(&self) -> Result<(), String> {
        self.key.validate().map_err(|error| error.to_string())?;
        if self.viewer.is_empty()
            || self.viewer.len() > 256
            || self.pull_request.number != self.key.number
            || self.range.number != self.key.number
            || self.pull_request.head_ref_oid != self.range.head.as_str()
            || pull_request_number(&self.pull_request.url, &self.key.repository)
                .map_err(|error| error.to_string())?
                != self.key.number
            || self.files.len() > MAX_CACHED_FILES
        {
            return Err("Invalid cached pull request identity or range".into());
        }
        let paths: BTreeSet<_> = self.files.iter().map(|file| &file.entry.path).collect();
        if paths.len() != self.files.len()
            || self
                .selected
                .as_ref()
                .is_some_and(|path| !paths.contains(path))
        {
            return Err("Invalid cached pull request file selection".into());
        }
        Ok(())
    }
}

/// Shell-specific persistence runs on the blocking pool. The write order is
/// captured on the UI thread; stores reject older writes for the same PR even
/// when their tasks finish later. A new process starts a new ordering epoch.
pub trait Store: Send + Sync {
    fn load(&self, key: &PullRequestKey) -> Result<Option<Snapshot>, String>;
    fn save(&self, snapshot: &Snapshot, order: u64) -> Result<(), String>;
}

#[cfg(test)]
pub(crate) fn fixture() -> Snapshot {
    use super::tests::{entry, loaded, pull_request, target};
    let entry = entry("src/main.rs", crate::repository::ChangeKind::Modified);
    Snapshot {
        key: PullRequestKey::new(target(1).repository, 1).unwrap(),
        viewer: "me".into(),
        pull_request: pull_request(1),
        range: loaded(Vec::new(), Vec::new()).range,
        selected: Some(entry.path.clone()),
        files: vec![File {
            entry,
            viewed: true,
            unverified: true,
        }],
        saved_ms: 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_reopen_retains_selection_and_rejects_stale_same_key_writes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("review.json");
        let store = JsonStore::new(path.clone());
        let mut current = fixture();
        store.save(&current, 2).unwrap();
        let saved = current.clone();
        current.files[0].viewed = false;
        store.save(&current, 1).unwrap();
        assert_eq!(
            JsonStore::new(path.clone()).load(&saved.key).unwrap(),
            Some(saved.clone())
        );
        current.key.number = 2;
        current.pull_request = super::super::tests::pull_request(2);
        current.range.number = 2;
        store.save(&current, 1).unwrap();
        assert_eq!(store.load(&current.key).unwrap(), Some(current));
        assert_eq!(store.load(&saved.key).unwrap(), Some(saved));
        std::fs::write(&path, br#"{"version":999,"reviews":[]}"#).unwrap();
        assert!(store.save(&fixture(), 3).is_err());
        assert!(std::fs::read_to_string(path).unwrap().contains("999"));
    }

    #[test]
    fn invalid_identity_and_selection_are_not_persisted() {
        let dir = tempfile::tempdir().unwrap();
        let store = JsonStore::new(dir.path().join("review.json"));
        let mut snapshot = fixture();
        snapshot.pull_request.head_ref_oid = "d".repeat(40);
        assert!(store.save(&snapshot, 1).is_err());
        let mut snapshot = fixture();
        snapshot.selected = Some(RepoPath::parse("missing.rs").unwrap());
        assert!(store.save(&snapshot, 2).is_err());
    }
}

/// Ceditor's bounded, atomic review.json lane. A private advisory lock
/// serializes read/modify/write across editor processes as well as tabs.
pub struct JsonStore {
    path: std::path::PathBuf,
}

#[derive(Serialize, Deserialize)]
struct Document {
    version: u32,
    reviews: Vec<Record>,
}

#[derive(Serialize, Deserialize)]
struct Record {
    snapshot: Snapshot,
    epoch: String,
    order: u64,
}

impl JsonStore {
    pub fn new(path: std::path::PathBuf) -> Self {
        Self { path }
    }

    fn read(&self) -> Result<Document, String> {
        use std::io::Read;
        let file = match crate::secure_file::open_existing_private_file(&self.path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Document {
                    version: 1,
                    reviews: Vec::new(),
                });
            }
            Err(error) => return Err(error.to_string()),
        };
        let mut bytes = Vec::new();
        file.take(MAX_CACHE_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|e| e.to_string())?;
        if bytes.len() as u64 > MAX_CACHE_BYTES {
            return Err("Review cache exceeds the size limit".into());
        }
        let document: Document = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
        if document.version != 1 || document.reviews.len() > MAX_CACHED_REVIEWS {
            return Err("Unsupported review cache format or size".into());
        }
        for record in &document.reviews {
            record.snapshot.validate()?;
        }
        Ok(document)
    }
}

impl Store for JsonStore {
    fn load(&self, key: &PullRequestKey) -> Result<Option<Snapshot>, String> {
        key.validate().map_err(|e| e.to_string())?;
        Ok(self
            .read()?
            .reviews
            .into_iter()
            .find(|record| {
                record.snapshot.key.number == key.number
                    && record
                        .snapshot
                        .key
                        .repository
                        .same_repository(&key.repository)
            })
            .map(|record| record.snapshot))
    }

    fn save(&self, snapshot: &Snapshot, order: u64) -> Result<(), String> {
        use std::{
            fs::{File, OpenOptions},
            io::Write,
            os::{fd::AsRawFd, unix::fs::OpenOptionsExt},
        };
        snapshot.validate()?;
        let parent = self
            .path
            .parent()
            .ok_or("Review cache has no parent directory")?;
        crate::secure_file::prepare_private_directory(parent).map_err(|e| e.to_string())?;
        let lock_path = self.path.with_extension("json.lock");
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
            .open(&lock_path)
            .map_err(|e| e.to_string())?;
        crate::secure_file::validate_and_privatize_open_file(&lock_path, &lock)
            .map_err(|e| e.to_string())?;
        // SAFETY: lock owns this descriptor until the transaction finishes.
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(std::io::Error::last_os_error().to_string());
        }
        let epoch = write_epoch();
        let mut document = self.read()?;
        let same_key = |record: &Record| {
            record.snapshot.key.number == snapshot.key.number
                && record
                    .snapshot
                    .key
                    .repository
                    .same_repository(&snapshot.key.repository)
        };
        if document
            .reviews
            .iter()
            .any(|record| same_key(record) && record.epoch == epoch && record.order >= order)
        {
            return Ok(());
        }
        document.reviews.retain(|record| !same_key(record));
        document.reviews.insert(
            0,
            Record {
                snapshot: snapshot.clone(),
                epoch: epoch.to_owned(),
                order,
            },
        );
        document.reviews.truncate(MAX_CACHED_REVIEWS);
        let bytes = loop {
            let bytes = serde_json::to_vec(&document).map_err(|e| e.to_string())?;
            if bytes.len() as u64 <= MAX_CACHE_BYTES {
                break bytes;
            }
            if document.reviews.len() <= 1 {
                return Err("Review cache exceeds the size limit".into());
            }
            document.reviews.pop();
        };
        let temporary = parent.join(format!(".review-{}.tmp", uuid::Uuid::new_v4()));
        let result = (|| -> std::io::Result<()> {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
                .open(&temporary)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
            std::fs::rename(&temporary, &self.path)?;
            File::open(parent)?.sync_all()
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&temporary);
        }
        result.map_err(|e| e.to_string())
    }
}
