//! The account usage every window of this installation shares.
//!
//! Each window is its own process, and each would otherwise probe every
//! provider for itself and start up with nothing to show. One small file in
//! the instance's cache directory - which the development runner isolates
//! along with the rest of its state - lets a window start with the last
//! figures any window saw, and lets the window that probes next skip the
//! probe when another window already has.
//!
//! The file is written whole to a temporary sibling and renamed over the
//! original, under an advisory lock, so a reader never sees half of it and
//! two writers never lose each other's entries. It holds display metadata
//! only - the same figures and identity the interface shows - and is kept
//! private to the user like every other file in the data directories.

use std::{
    collections::BTreeMap,
    fs::{File, OpenOptions},
    io::{self, Read, Write},
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

use super::{Identity, UsageSnapshot, schedule::CLOCK_SLACK};

const FILE_NAME: &str = "account-usage.json";
const LOCK_NAME: &str = "account-usage.lock";
const VERSION: u32 = 1;
/// A cache larger than this is not one cagents wrote, and is ignored rather
/// than read into memory.
const MAX_BYTES: u64 = 1024 * 1024;

/// What is remembered about one account.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    pub identity: Option<Identity>,
    pub snapshot: UsageSnapshot,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Document {
    version: u32,
    entries: BTreeMap<String, Entry>,
}

/// The shared cache file.
#[derive(Debug, Clone)]
pub struct Cache {
    directory: PathBuf,
}

impl Cache {
    /// The cache of the running installation, or `None` where the platform
    /// offers no cache directory.
    pub fn installation() -> Option<Self> {
        crate::instance::project_dirs().map(|dirs| Self::at(dirs.cache_dir()))
    }

    pub fn at(directory: impl Into<PathBuf>) -> Self {
        Self {
            directory: directory.into(),
        }
    }

    fn path(&self) -> PathBuf {
        self.directory.join(FILE_NAME)
    }

    /// Every remembered account as of `now`, in unix milliseconds. A missing,
    /// oversized, damaged or future-version file reads as empty, and an entry
    /// stamped in the future is left out: the cache only ever saves a probe,
    /// so losing it costs one.
    pub fn load(&self, now: u64) -> BTreeMap<String, Entry> {
        read_document(&self.path())
            .map(|document| {
                document
                    .entries
                    .into_iter()
                    .filter(|(_, entry)| {
                        entry.snapshot.observed_at <= now.saturating_add(CLOCK_SLACK)
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Remembers `entry`, observed just now, for `key` - unless the cache
    /// already holds something newer for it that another window wrote
    /// meanwhile. An entry stamped further ahead than [`CLOCK_SLACK`] is from
    /// a clock that has since been set back, and is replaced.
    pub fn store(&self, key: &str, entry: Entry) -> io::Result<()> {
        crate::secure_file::prepare_private_directory(&self.directory)?;
        let _lock = self.lock(libc::LOCK_EX)?;
        let path = self.path();
        let mut document = read_document(&path).unwrap_or_default();
        document.version = VERSION;
        match document.entries.get(key) {
            Some(existing)
                if existing.snapshot.observed_at > entry.snapshot.observed_at
                    && existing.snapshot.observed_at
                        <= entry.snapshot.observed_at.saturating_add(CLOCK_SLACK) =>
            {
                return Ok(());
            }
            _ => {
                document.entries.insert(key.to_owned(), entry);
            }
        }
        let bytes = serde_json::to_vec(&document).map_err(io::Error::other)?;
        let mut temporary = tempfile::Builder::new()
            .prefix(".account-usage.")
            .permissions(std::os::unix::fs::PermissionsExt::from_mode(0o600))
            .tempfile_in(&self.directory)?;
        temporary.write_all(&bytes)?;
        temporary.as_file().sync_all()?;
        temporary.persist(&path).map_err(|error| error.error)?;
        Ok(())
    }

    /// Takes the per-provider probe lock without waiting, so two windows
    /// never probe the same provider at once. `None` means another window
    /// holds it and its answer will reach this one through the cache.
    pub fn try_probe_lock(&self, provider: &str) -> io::Result<Option<File>> {
        crate::secure_file::prepare_private_directory(&self.directory)?;
        match crate::instance::owned_lock(
            &self
                .directory
                .join(format!("account-probe.{provider}.lock")),
            libc::LOCK_EX | libc::LOCK_NB,
        ) {
            Ok(lock) => Ok(Some(lock)),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(None),
            Err(error) => Err(error),
        }
    }

    fn lock(&self, operation: i32) -> io::Result<File> {
        crate::instance::owned_lock(&self.directory.join(LOCK_NAME), operation)
    }
}

fn read_document(path: &Path) -> Option<Document> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)
        .ok()?;
    let metadata = file.metadata().ok()?;
    if !metadata.is_file() || metadata.len() > MAX_BYTES {
        return None;
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(MAX_BYTES).read_to_end(&mut bytes).ok()?;
    let document: Document = serde_json::from_slice(&bytes).ok()?;
    (document.version == VERSION).then_some(document)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account::{UsageSource, UsageStatus};
    use std::{fs, os::unix::fs::PermissionsExt};

    fn entry(observed_at: u64, detail: &str) -> Entry {
        let mut snapshot = UsageSnapshot::state(UsageStatus::NeedsLogin, detail, observed_at);
        snapshot.source = UsageSource::Probe;
        Entry {
            identity: Some(Identity {
                email: Some("user@example.com".into()),
                organization: None,
                plan: Some("max".into()),
            }),
            snapshot,
        }
    }

    #[test]
    fn entries_round_trip_privately_and_newer_figures_win() {
        let directory = tempfile::tempdir().unwrap();
        let cache = Cache::at(directory.path().join("nested"));
        assert!(cache.load(u64::MAX).is_empty());
        cache
            .store("claude:/h/.claude", entry(10, "first"))
            .unwrap();
        cache.store("codex:/h/.codex", entry(10, "codex")).unwrap();
        // Another window wrote something newer; an older write loses.
        cache
            .store("claude:/h/.claude", entry(30, "newer"))
            .unwrap();
        cache
            .store("claude:/h/.claude", entry(20, "stale"))
            .unwrap();
        let loaded = cache.load(u64::MAX);
        assert_eq!(loaded.len(), 2);
        assert_eq!(
            loaded["claude:/h/.claude"].snapshot.detail.as_deref(),
            Some("newer")
        );
        let mode = fs::metadata(directory.path().join("nested").join(FILE_NAME))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        let directory_mode = fs::metadata(directory.path().join("nested"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(directory_mode & 0o777, 0o700);
    }

    /// A clock set back after a probe must not freeze the figures until it
    /// catches up with the stamp.
    #[test]
    fn entries_stamped_in_the_future_are_neither_read_nor_kept() {
        let directory = tempfile::tempdir().unwrap();
        let cache = Cache::at(directory.path());
        let now = 1_000_000;
        cache
            .store("k", entry(now + 10 * CLOCK_SLACK, "ahead"))
            .unwrap();
        assert!(cache.load(now).is_empty());
        cache.store("k", entry(now, "current")).unwrap();
        assert_eq!(
            cache.load(now)["k"].snapshot.detail.as_deref(),
            Some("current")
        );
        // Within the slack a newer entry is another window's, and it stays.
        cache
            .store("k", entry(now + CLOCK_SLACK / 2, "other window"))
            .unwrap();
        cache.store("k", entry(now, "late")).unwrap();
        assert_eq!(
            cache.load(now)["k"].snapshot.detail.as_deref(),
            Some("other window")
        );
    }

    #[test]
    fn a_damaged_or_foreign_cache_reads_as_empty() {
        let directory = tempfile::tempdir().unwrap();
        let cache = Cache::at(directory.path());
        fs::write(directory.path().join(FILE_NAME), b"{not json").unwrap();
        assert!(cache.load(u64::MAX).is_empty());
        fs::write(
            directory.path().join(FILE_NAME),
            br#"{"version": 99, "entries": {}}"#,
        )
        .unwrap();
        assert!(cache.load(u64::MAX).is_empty());
        // A damaged file is replaced by the next store rather than blocking it.
        cache.store("k", entry(1, "fresh")).unwrap();
        assert_eq!(cache.load(u64::MAX).len(), 1);
        fs::remove_file(directory.path().join(FILE_NAME)).unwrap();
        assert!(cache.load(u64::MAX).is_empty());
    }

    #[test]
    fn a_symlinked_cache_file_is_not_followed() {
        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("elsewhere.json");
        fs::write(&target, br#"{"version": 1, "entries": {}}"#).unwrap();
        std::os::unix::fs::symlink(&target, directory.path().join(FILE_NAME)).unwrap();
        let cache = Cache::at(directory.path());
        assert!(cache.load(u64::MAX).is_empty());
    }

    #[test]
    fn only_one_window_holds_a_providers_probe_lock() {
        let directory = tempfile::tempdir().unwrap();
        let cache = Cache::at(directory.path());
        let held = cache.try_probe_lock("claude").unwrap();
        assert!(held.is_some());
        assert!(cache.try_probe_lock("claude").unwrap().is_none());
        assert!(cache.try_probe_lock("codex").unwrap().is_some());
        drop(held);
        assert!(cache.try_probe_lock("claude").unwrap().is_some());
    }
}
