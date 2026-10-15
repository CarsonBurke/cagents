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

/// An ordering observation captured before a cache task enters the blocking pool.
/// Updated clients on the same kernel boot and Linux time namespace share this
/// clock domain. Cross-container clocks and older SQLite writers are outside
/// this ordering contract. JsonStore separately fences older version-one JSON
/// writers after committing a version-two document. The boot identity and clock
/// observation are inseparable.
/// Equal clock ticks keep the already committed snapshot; clock resolution
/// bounds ordering precision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriteOrder {
    pub(crate) epoch: String,
    pub(crate) tick: u64,
}

impl WriteOrder {
    pub fn capture() -> Result<Self, String> {
        static EPOCH: std::sync::OnceLock<Result<String, String>> = std::sync::OnceLock::new();
        let epoch = EPOCH
            .get_or_init(|| {
                let boot = std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
                    .map_err(|error| format!("Cannot identify the review-cache clock: {error}"))?;
                let boot = uuid::Uuid::parse_str(boot.trim())
                    .map_err(|error| format!("Invalid review-cache boot identity: {error}"))?;
                Ok(format!("boottime-v1:{boot}"))
            })
            .clone()?;
        let mut time = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // SAFETY: time is a valid writable timespec for the duration of the call.
        if unsafe { libc::clock_gettime(libc::CLOCK_BOOTTIME, &mut time) } != 0 {
            return Err(format!(
                "Cannot read the review-cache clock: {}",
                std::io::Error::last_os_error()
            ));
        }
        Ok(Self {
            epoch,
            tick: Self::checked_tick(time.tv_sec, time.tv_nsec)?,
        })
    }

    fn checked_tick(seconds: libc::time_t, nanos: libc::c_long) -> Result<u64, String> {
        let tick = u64::try_from(seconds)
            .ok()
            .zip(
                u64::try_from(nanos)
                    .ok()
                    .filter(|nanos| *nanos < 1_000_000_000),
            )
            .and_then(|(seconds, nanos)| seconds.checked_mul(1_000_000_000)?.checked_add(nanos))
            .filter(|tick| *tick <= i64::MAX as u64);
        tick.ok_or_else(|| "Review-cache clock is outside its supported range".into())
    }

    pub(crate) fn superseded_by(&self, epoch: &str, tick: u64) -> bool {
        epoch == self.epoch && tick >= self.tick
    }

    #[cfg(test)]
    pub(crate) fn fixture(tick: u64) -> Self {
        Self {
            epoch: "boottime-v1:fixture".into(),
            tick,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct File {
    pub entry: DiffEntry,
    pub viewed: bool,
    #[serde(default)]
    pub changed_since_viewed: bool,
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
    #[serde(default)]
    pub unknown_paths: BTreeSet<RepoPath>,
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
        if self.unknown_paths.iter().any(|path| !paths.contains(path))
            || paths.len() != self.files.len()
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
/// when their tasks finish later, including writes from other UI processes on
/// the same boot. Legacy epochs and earlier boots remain readable.
pub trait Store: Send + Sync {
    fn load(&self, key: &PullRequestKey) -> Result<Option<Snapshot>, String>;
    fn save(&self, snapshot: &Snapshot, order: &WriteOrder) -> Result<(), String>;
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
        unknown_paths: BTreeSet::new(),
        selected: Some(entry.path.clone()),
        files: vec![File {
            entry,
            viewed: true,
            changed_since_viewed: false,
            unverified: true,
        }],
        saved_ms: 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store_for_lane(directory: &std::path::Path, lane: &str) -> Box<dyn Store> {
        match lane {
            "sqlite" => Box::new(
                crate::persistence::StateStore::open(
                    directory.join("state.sqlite3"),
                    crate::model::BootId::new("same-boot"),
                )
                .unwrap(),
            ),
            "json" => Box::new(JsonStore::new(directory.join("review.json"))),
            _ => panic!("unknown cache lane"),
        }
    }

    #[test]
    fn crossprocess_cache_writes_keep_the_newer_capture_in_both_stores() {
        use std::{
            io::Write,
            process::{Command, Stdio},
            time::{Duration, Instant},
        };
        const ROLE: &str = "CAGENTS_REVIEW_CACHE_ORDER_ROLE";
        const DIRECTORY: &str = "CAGENTS_REVIEW_CACHE_ORDER_DIRECTORY";
        const LANE: &str = "CAGENTS_REVIEW_CACHE_ORDER_LANE";
        const NAME: &str = "pull_request_review::cache::tests::crossprocess_cache_writes_keep_the_newer_capture_in_both_stores";
        if let Some(role) = std::env::var_os(ROLE) {
            let directory = std::path::PathBuf::from(std::env::var_os(DIRECTORY).unwrap());
            let store = store_for_lane(&directory, &std::env::var(LANE).unwrap());
            let mut snapshot = fixture();
            let order = WriteOrder::capture().unwrap();
            if role == "old" {
                snapshot.saved_ms = 200; // Wall time deliberately moves backwards in the newer writer.
                snapshot.files[0].viewed = true;
                std::fs::write(directory.join("old-ready"), order.tick.to_string()).unwrap();
                let mut release = [0];
                std::io::Read::read_exact(&mut std::io::stdin(), &mut release).unwrap();
            } else {
                assert_eq!(role, "new");
                let earlier: u64 = std::fs::read_to_string(directory.join("old-ready"))
                    .unwrap()
                    .parse()
                    .unwrap();
                assert!(order.tick > earlier);
                snapshot.saved_ms = 100;
                snapshot.files[0].viewed = false;
            }
            store.save(&snapshot, &order).unwrap();
            if role == "new" {
                std::fs::write(directory.join("new-saved"), "committed").unwrap();
            }
            return;
        }
        for lane in ["sqlite", "json"] {
            let directory = tempfile::tempdir().unwrap();
            let child = |role| {
                let mut command = Command::new(std::env::current_exe().unwrap());
                command
                    .args(["--exact", NAME, "--nocapture"])
                    .env(ROLE, role)
                    .env(DIRECTORY, directory.path())
                    .env(LANE, lane);
                command
            };
            let mut old = child("old").stdin(Stdio::piped()).spawn().unwrap();
            let deadline = Instant::now() + Duration::from_secs(5);
            while !directory.path().join("old-ready").exists() {
                if Instant::now() >= deadline || old.try_wait().unwrap().is_some() {
                    let _ = old.kill();
                    let _ = old.wait();
                    panic!("old process did not capture its snapshot");
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            let newer = child("new").status().unwrap();
            assert!(
                directory.path().join("new-saved").exists(),
                "new child must execute its exact test and commit"
            );
            old.stdin.take().unwrap().write_all(&[1]).unwrap();
            let older = old.wait().unwrap();
            assert!(newer.success() && older.success());
            let store = store_for_lane(directory.path(), lane);
            let snapshot = store.load(&fixture().key).unwrap().unwrap();
            assert_eq!(
                snapshot.saved_ms, 100,
                "{lane}: later capture survives clock rollback"
            );
            assert!(
                !snapshot.files[0].viewed,
                "{lane}: late old writer was refused"
            );
        }
    }

    #[test]
    fn boot_order_bounds_equal_ticks_and_legacy_epochs_in_both_stores() {
        assert_eq!(WriteOrder::checked_tick(1, 3).unwrap(), 1_000_000_003);
        for (seconds, nanos) in [
            (-1, 0),
            (0, -1),
            (0, 1_000_000_000),
            (i64::MAX, 0),
            (9_223_372_036, 854_775_808),
        ] {
            assert!(WriteOrder::checked_tick(seconds, nanos).is_err());
        }
        assert_eq!(
            WriteOrder::checked_tick(9_223_372_036, 854_775_807).unwrap(),
            i64::MAX as u64
        );
        for lane in ["sqlite", "json"] {
            let directory = tempfile::tempdir().unwrap();
            let store = store_for_lane(directory.path(), lane);
            let mut snapshot = fixture();
            let legacy = WriteOrder {
                epoch: uuid::Uuid::new_v4().to_string(),
                tick: 99,
            };
            store.save(&snapshot, &legacy).unwrap();
            snapshot.files[0].viewed = false;
            let first = WriteOrder::fixture(10);
            store.save(&snapshot, &first).unwrap();
            let saved = snapshot.clone();
            snapshot.files[0].viewed = true;
            store.save(&snapshot, &first).unwrap();
            assert_eq!(
                store.load(&snapshot.key).unwrap(),
                Some(saved.clone()),
                "equal ticks keep committed state"
            );
            drop(store);
            let store = store_for_lane(directory.path(), lane);
            store.save(&snapshot, &WriteOrder::fixture(9)).unwrap();
            assert_eq!(
                store.load(&snapshot.key).unwrap(),
                Some(saved),
                "restart preserves same-boot order"
            );
            let next_boot = WriteOrder {
                epoch: "boottime-v1:next-boot".into(),
                tick: 1,
            };
            store.save(&snapshot, &next_boot).unwrap();
            assert_eq!(
                store.load(&snapshot.key).unwrap(),
                Some(snapshot),
                "new boot accepts its reset clock"
            );
        }
    }

    #[test]
    fn json_reopen_retains_selection_and_rejects_stale_same_key_writes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("review.json");
        let store = JsonStore::new(path.clone());
        let mut current = fixture();
        store.save(&current, &WriteOrder::fixture(2)).unwrap();
        let saved = current.clone();
        current.files[0].viewed = false;
        store.save(&current, &WriteOrder::fixture(1)).unwrap();
        assert_eq!(
            JsonStore::new(path.clone()).load(&saved.key).unwrap(),
            Some(saved.clone())
        );
        current.key.number = 2;
        current.pull_request = super::super::tests::pull_request(2);
        current.range.number = 2;
        store.save(&current, &WriteOrder::fixture(1)).unwrap();
        assert_eq!(store.load(&current.key).unwrap(), Some(current));
        assert_eq!(store.load(&saved.key).unwrap(), Some(saved));
        std::fs::write(&path, br#"{"version":999,"reviews":[]}"#).unwrap();
        assert!(store.save(&fixture(), &WriteOrder::fixture(3)).is_err());
        assert!(std::fs::read_to_string(path).unwrap().contains("999"));
    }

    #[test]
    fn json_cache_reads_legacy_records_and_commits_the_new_writer_gate() {
        let directory = tempfile::tempdir().unwrap();
        crate::secure_file::prepare_private_directory(directory.path()).unwrap();
        let path = directory.path().join("review.json");
        let legacy = fixture();
        let document = Document {
            version: 1,
            reviews: vec![Record {
                snapshot: legacy.clone(),
                epoch: uuid::Uuid::new_v4().to_string(),
                order: 99,
            }],
        };
        use std::{io::Write, os::unix::fs::OpenOptionsExt};
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .unwrap();
        file.write_all(&serde_json::to_vec(&document).unwrap())
            .unwrap();
        drop(file);
        let store = JsonStore::new(path.clone());
        assert_eq!(store.load(&legacy.key).unwrap(), Some(legacy.clone()));
        let mut current = legacy;
        current.files[0].viewed = false;
        store.save(&current, &WriteOrder::fixture(1)).unwrap();
        let written: Document = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(
            written.version, 2,
            "the old reader's version != 1 gate refuses subsequent writes"
        );
        assert_eq!(written.reviews.len(), 1);
        assert_eq!(written.reviews[0].snapshot, current);
        assert_eq!(store.load(&current.key).unwrap(), Some(current));
    }

    #[test]
    fn invalid_identity_and_selection_are_not_persisted() {
        let dir = tempfile::tempdir().unwrap();
        let store = JsonStore::new(dir.path().join("review.json"));
        let mut snapshot = fixture();
        snapshot.pull_request.head_ref_oid = "d".repeat(40);
        assert!(store.save(&snapshot, &WriteOrder::fixture(1)).is_err());
        let mut snapshot = fixture();
        snapshot.selected = Some(RepoPath::parse("missing.rs").unwrap());
        assert!(store.save(&snapshot, &WriteOrder::fixture(2)).is_err());
    }
}

/// Ceditor's bounded, atomic review.json lane. A private advisory lock
/// serializes read/modify/write across editor processes as well as tabs.
/// Version-one documents remain readable. Accepted writes publish version two:
/// older version-one readers refuse that format before modifying the cache, so
/// they cannot replace an already committed boot-ordered document.
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
                    version: 2,
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
        if !matches!(document.version, 1 | 2) || document.reviews.len() > MAX_CACHED_REVIEWS {
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

    fn save(&self, snapshot: &Snapshot, order: &WriteOrder) -> Result<(), String> {
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
            .any(|record| same_key(record) && order.superseded_by(&record.epoch, record.order))
        {
            return Ok(());
        }
        document.version = 2;
        document.reviews.retain(|record| !same_key(record));
        document.reviews.insert(
            0,
            Record {
                snapshot: snapshot.clone(),
                epoch: order.epoch.clone(),
                order: order.tick,
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
