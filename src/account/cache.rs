//! The account usage every window of this installation shares.
//!
//! Each window is its own process, and each would otherwise probe every
//! provider for itself and start up with nothing to show. One small file in
//! the instance's cache directory - which the development runner isolates
//! along with the rest of its state - lets a window start with the last
//! figures any window saw, and lets the window that probes next skip the
//! probe when another window already has.
//!
//! The same file remembers the models each provider last listed, so a new
//! conversation can offer every provider's models before any of their CLIs
//! is started for it.
//!
//! The file is written whole to a temporary sibling and renamed over the
//! original, under an advisory lock, so a reader never sees half of it and
//! two writers never lose each other's entries. It holds display metadata
//! only - the same figures, identity and model names the interface shows -
//! and is kept private to the user like every other file in the data
//! directories.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs::OpenOptions,
    io::{self, Read, Write},
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

use super::{Identity, UsageSnapshot, schedule::CLOCK_SLACK};
use crate::{agent::Provider, chat::Model};

const FILE_NAME: &str = "account-usage.json";
const LOCK_NAME: &str = "account-usage.lock";
const VERSION: u32 = 1;
/// A cache larger than this is not one cagents wrote, and is ignored rather
/// than read into memory.
const MAX_BYTES: u64 = 1024 * 1024;
/// How long an entry no configured account refers to is kept, in unix
/// milliseconds.
const ORPHAN_AFTER: u64 = 7 * 24 * 3_600_000;

/// What is remembered about one account.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    pub identity: Option<Identity>,
    pub snapshot: UsageSnapshot,
}

/// The models a provider last listed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Catalog {
    pub models: Vec<Model>,
    /// Unix milliseconds at which the provider listed them.
    pub observed_at: u64,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct Document {
    #[serde(default)]
    execution_policy: Option<String>,
    version: u32,
    entries: BTreeMap<String, Entry>,
    /// Added after the first version, which the version does not count: a
    /// build that predates it reads the rest, and drops it when it writes.
    #[serde(default, deserialize_with = "readable_catalogs")]
    catalogs: BTreeMap<Provider, Catalog>,
}

/// Each provider's list is read on its own, so one this build cannot read -
/// a provider added since, say - costs only that list, not the whole file.
fn readable_catalogs<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<BTreeMap<Provider, Catalog>, D::Error> {
    let serde_json::Value::Object(catalogs) = serde_json::Value::deserialize(deserializer)? else {
        return Ok(BTreeMap::new());
    };
    Ok(catalogs
        .into_iter()
        .filter_map(|(provider, catalog)| {
            Some((
                Provider::deserialize(serde_json::Value::String(provider)).ok()?,
                Catalog::deserialize(catalog).ok()?,
            ))
        })
        .collect())
}

/// The shared cache file.
#[derive(Debug, Clone)]
pub struct Cache {
    directory: PathBuf,
    execution_policy: Option<String>,
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
            execution_policy: None,
        }
    }

    /// Account metadata belongs to the captured executable, arguments and
    /// environment, not to a conversation's model/permission presets. Old
    /// unqualified cache files remain display history; they cannot answer a
    /// probe under another execution policy. Only a digest is stored here.
    pub(crate) fn for_cli(&self, provider: Provider, cli: &super::probe::Cli) -> Self {
        use sha2::{Digest, Sha256};
        use std::os::unix::ffi::OsStrExt;
        let program = cli.program.as_os_str().as_bytes();
        // PathBuf's JSON serializer rejects non-UTF8 paths; PATH discovery is
        // byte-preserving. Length framing keeps the byte and UTF8 fields distinct.
        let configuration =
            serde_json::to_vec(&(&cli.configuration.arguments, &cli.configuration.environment))
                .expect("string execution arguments and environment serialize");
        let digest = Sha256::new()
            .chain_update(b"cagents-cli-metadata-v1\0")
            .chain_update((program.len() as u64).to_le_bytes())
            .chain_update(program)
            .chain_update((configuration.len() as u64).to_le_bytes())
            .chain_update(configuration)
            .finalize();
        Self {
            // One bounded document per provider, not one directory per edit.
            // A writer from another policy may cost a cache hit, never relabel
            // its metadata as the current policy.
            directory: self
                .directory
                .join("cli-metadata-v1")
                .join(crate::settings::agents::provider_key(provider)),
            execution_policy: Some(format!("{digest:x}")),
        }
    }

    fn read(&self) -> Option<Document> {
        read_document(&self.path())
            .filter(|document| document.execution_policy == self.execution_policy)
    }

    fn path(&self) -> PathBuf {
        self.directory.join(FILE_NAME)
    }

    /// Every remembered account as of `now`, in unix milliseconds. A missing,
    /// oversized, damaged or future-version file reads as empty, and an entry
    /// stamped in the future is left out: the cache only ever saves a probe,
    /// so losing it costs one.
    pub fn load(&self, now: u64) -> BTreeMap<String, Entry> {
        self.read()
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
        let mut document = self.read().unwrap_or_default();
        document.version = VERSION;
        if document.entries.get(key).is_some_and(|existing| {
            supersedes(existing.snapshot.observed_at, entry.snapshot.observed_at)
        }) {
            return Ok(());
        }
        document.entries.insert(key.to_owned(), entry);
        self.write(&document)
    }

    /// The models each provider last listed, as of `now`. Like
    /// [`Cache::load`], a list stamped in the future is left out.
    pub fn catalogs(&self, now: u64) -> BTreeMap<Provider, Catalog> {
        self.read()
            .map(|document| {
                document
                    .catalogs
                    .into_iter()
                    .filter(|(_, catalog)| catalog.observed_at <= now.saturating_add(CLOCK_SLACK))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Remembers what `provider` listed, unless another window has written
    /// a newer list meanwhile - the same rule [`Cache::store`] keeps.
    pub fn store_catalog(&self, provider: Provider, catalog: Catalog) -> io::Result<()> {
        crate::secure_file::prepare_private_directory(&self.directory)?;
        let _lock = self.lock(libc::LOCK_EX)?;
        let mut document = self.read().unwrap_or_default();
        document.version = VERSION;
        if document
            .catalogs
            .get(&provider)
            .is_some_and(|existing| supersedes(existing.observed_at, catalog.observed_at))
        {
            return Ok(());
        }
        document.catalogs.insert(provider, catalog);
        self.write(&document)
    }

    /// Forgets the entries no account in `keep` refers to that are older
    /// than [`ORPHAN_AFTER`]: the figures of an account since removed, or of
    /// a home that moved. A younger one stays, because another window may
    /// have an account this one has not read from the registry yet.
    pub fn prune(&self, keep: &BTreeSet<String>, now: u64) -> io::Result<()> {
        // Most starts have nothing to forget, and read without the lock.
        let orphaned = |document: &Document| {
            document
                .entries
                .iter()
                .any(|(key, entry)| !keep.contains(key) && !current(entry, now))
        };
        if !self.read().is_some_and(|document| orphaned(&document)) {
            return Ok(());
        }
        let _lock = self.lock(libc::LOCK_EX)?;
        let Some(mut document) = self.read().filter(orphaned) else {
            return Ok(());
        };
        document
            .entries
            .retain(|key, entry| keep.contains(key) || current(entry, now));
        self.write(&document)
    }

    /// Replaces the file with `document`, whole: written to a private
    /// temporary sibling and renamed over the original. The caller holds the
    /// lock.
    fn write(&self, document: &Document) -> io::Result<()> {
        let mut document = document.clone();
        document.execution_policy = self.execution_policy.clone();
        let bytes = serde_json::to_vec(&document).map_err(io::Error::other)?;
        if bytes.len() as u64 > MAX_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "the account usage update is too large; the existing cache was preserved",
            ));
        }
        let mut temporary = tempfile::Builder::new()
            .prefix(".account-usage.")
            .permissions(std::os::unix::fs::PermissionsExt::from_mode(0o600))
            .tempfile_in(&self.directory)?;
        temporary.write_all(&bytes)?;
        temporary.as_file().sync_all()?;
        temporary
            .persist(self.path())
            .map_err(|error| error.error)?;
        Ok(())
    }

    /// Takes an account's probe lock, named by [`super::Account::lock_name`],
    /// without waiting, so two windows never probe the same account at once.
    /// `None` means another window holds it and its answer will reach this
    /// one through the cache.
    pub(crate) fn try_probe_lock(
        &self,
        name: &str,
    ) -> io::Result<Option<crate::instance::OwnedLock>> {
        crate::secure_file::prepare_private_directory(&self.directory)?;
        match crate::instance::owned_lock(
            &self.directory.join(format!("account-probe.{name}.lock")),
            libc::LOCK_EX | libc::LOCK_NB,
        ) {
            Ok(lock) => Ok(Some(lock)),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(None),
            Err(error) => Err(error),
        }
    }

    fn lock(&self, operation: i32) -> io::Result<crate::instance::OwnedLock> {
        crate::instance::owned_lock(&self.directory.join(LOCK_NAME), operation)
    }
}

/// Whether something already cached at `existing` is newer than an
/// observation at `incoming`, and stays. A stamp further ahead than
/// [`CLOCK_SLACK`] is from a clock since set back, and is replaced.
fn supersedes(existing: u64, incoming: u64) -> bool {
    existing > incoming && existing <= incoming.saturating_add(CLOCK_SLACK)
}

/// Whether `entry` is recent enough at `now` to keep with nothing referring
/// to it. A stamp further ahead than a clock drifts is from a clock since
/// set back, and says nothing about its age.
fn current(entry: &Entry, now: u64) -> bool {
    let observed = entry.snapshot.observed_at;
    observed <= now.saturating_add(CLOCK_SLACK) && now.saturating_sub(observed) < ORPHAN_AFTER
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
    fn execution_policy_caches_separate_pins_arguments_and_environment_not_presets() {
        let directory = tempfile::tempdir().unwrap();
        let cache = Cache::at(directory.path());
        let original = crate::settings::AgentLaunchSettings::default();
        let cli = |policy: &crate::settings::AgentLaunchSettings| super::super::probe::Cli {
            program: policy
                .executable
                .as_ref()
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("/known/claude")),
            configuration: policy.clone(),
        };
        let scope = cache.for_cli(Provider::Claude, &cli(&original));
        scope.store("claude:/owned", entry(10, "original")).unwrap();
        scope
            .store_catalog(Provider::Claude, catalog(10, "original-model"))
            .unwrap();
        for changed in [
            crate::settings::AgentLaunchSettings {
                executable: Some("/different/claude".into()),
                ..original.clone()
            },
            crate::settings::AgentLaunchSettings {
                arguments: vec!["--custom".into()],
                ..original.clone()
            },
            crate::settings::AgentLaunchSettings {
                environment: BTreeMap::from([("PATH".into(), "/different/tools".into())]),
                ..original.clone()
            },
        ] {
            let changed = cache.for_cli(Provider::Claude, &cli(&changed));
            assert!(changed.load(100).is_empty());
            assert!(changed.catalogs(100).is_empty());
            changed
                .store("claude:/owned", entry(20, "different"))
                .unwrap();
        }
        let other_binary = super::super::probe::Cli {
            program: "/different/inherited/claude".into(),
            configuration: original.clone(),
        };
        assert!(
            cache
                .for_cli(Provider::Claude, &other_binary)
                .load(100)
                .is_empty(),
            "different inherited PATH resolution is distinct too"
        );
        assert!(
            scope.load(100).is_empty(),
            "other policies never relabel a cache hit"
        );
        scope
            .store("claude:/owned", entry(30, "original-again"))
            .unwrap();
        scope
            .store_catalog(Provider::Claude, catalog(30, "original-model"))
            .unwrap();
        let preset = crate::settings::AgentLaunchSettings {
            model: Some("different-model".into()),
            effort: Some("low".into()),
            permission_mode: Some("plan".into()),
            ..original.clone()
        };
        assert_eq!(
            cache.for_cli(Provider::Claude, &cli(&preset)).load(100),
            scope.load(100)
        );
        assert_eq!(
            scope.catalogs(100)[&Provider::Claude].models[0].id,
            "original-model"
        );
        assert!(
            cache.load(100).is_empty(),
            "unqualified live cache is distinct"
        );
        assert!(
            cache
                .for_cli(Provider::Codex, &cli(&original))
                .load(100)
                .is_empty()
        );
    }

    #[test]
    fn non_utf8_discovered_executable_has_exact_cache_identity() {
        use std::os::unix::ffi::OsStringExt;
        let directory = tempfile::tempdir().unwrap();
        let path = directory
            .path()
            .join(std::ffi::OsString::from_vec(b"tools-\xff".to_vec()));
        let other = directory.path().join("tools-\u{fffd}");
        for path in [&path, &other] {
            fs::create_dir(path).unwrap();
            fs::write(path.join("claude"), "#!/bin/sh\nexit 0\n").unwrap();
            fs::set_permissions(path.join("claude"), fs::Permissions::from_mode(0o700)).unwrap();
        }
        let cli = |path: &Path| {
            super::super::probe::Cli::configured(
                Provider::Claude,
                Some(path.as_os_str()),
                Default::default(),
            )
            .ok()
            .expect("real PATH candidate")
        };
        let original = cli(&path);
        assert!(original.program.to_str().is_none());
        let alias = cli(&other);
        let cache = Cache::at(directory.path().join("cache"));
        cache
            .for_cli(Provider::Claude, &original)
            .store("claude:/owned", entry(10, "byte path"))
            .unwrap();
        assert!(cache.for_cli(Provider::Claude, &alias).load(100).is_empty());
        assert_eq!(
            cache.for_cli(Provider::Claude, &original).load(100)["claude:/owned"]
                .snapshot
                .detail
                .as_deref(),
            Some("byte path")
        );
    }

    #[test]
    fn oversized_entry_preserves_the_shared_usage_cache() {
        let directory = tempfile::tempdir().unwrap();
        let cache = Cache::at(directory.path());
        cache.store("claude:/account", entry(10, "before")).unwrap();
        let before = cache.load(100);
        let bytes = fs::read(cache.path()).unwrap();
        let oversized = entry(20, &"x".repeat(MAX_BYTES as usize));
        assert!(cache.store("codex:/other", oversized).is_err());
        assert_eq!(fs::read(cache.path()).unwrap(), bytes);
        assert_eq!(cache.load(100), before);
        cache.store("claude:/account", entry(30, "after")).unwrap();
        assert_eq!(
            cache.load(100)["claude:/account"]
                .snapshot
                .detail
                .as_deref(),
            Some("after")
        );
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

    /// Entries no account refers to go once they are old; recent ones and
    /// the ones still referred to stay, and a cache with nothing to forget
    /// is left untouched.
    #[test]
    fn old_entries_no_account_refers_to_are_forgotten() {
        let directory = tempfile::tempdir().unwrap();
        let cache = Cache::at(directory.path());
        let now = 100 * ORPHAN_AFTER;
        cache.store("kept", entry(1, "referred to")).unwrap();
        cache.store("recent", entry(now - 1_000, "recent")).unwrap();
        cache
            .store("removed", entry(now - ORPHAN_AFTER, "old"))
            .unwrap();
        cache
            .store("ahead", entry(now + 10 * CLOCK_SLACK, "clock set back"))
            .unwrap();
        let keep = BTreeSet::from(["kept".to_owned()]);
        cache.prune(&keep, now).unwrap();
        let file = directory.path().join(FILE_NAME);
        let remaining = read_document(&file).unwrap().entries;
        assert_eq!(
            remaining.keys().map(String::as_str).collect::<Vec<_>>(),
            ["kept", "recent"]
        );
        let modified = fs::metadata(&file).unwrap().modified().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        cache.prune(&keep, now).unwrap();
        assert_eq!(fs::metadata(&file).unwrap().modified().unwrap(), modified);
        // A cache that was never written stays unwritten.
        let empty = tempfile::tempdir().unwrap();
        Cache::at(empty.path().join("nested"))
            .prune(&keep, now)
            .unwrap();
        assert!(!empty.path().join("nested").exists());
    }

    fn catalog(observed_at: u64, id: &str) -> Catalog {
        Catalog {
            models: vec![Model {
                id: id.into(),
                label: id.to_uppercase(),
                reasoning: vec!["high".into()],
            }],
            observed_at,
        }
    }

    /// Each provider's list is kept beside the figures, and the newer of two
    /// windows' lists wins, as the figures' do.
    #[test]
    fn listed_models_round_trip_beside_the_figures() {
        let directory = tempfile::tempdir().unwrap();
        let cache = Cache::at(directory.path());
        assert!(cache.catalogs(u64::MAX).is_empty());
        cache.store("k", entry(1, "figures")).unwrap();
        cache
            .store_catalog(Provider::Claude, catalog(10, "opus"))
            .unwrap();
        cache
            .store_catalog(Provider::Codex, catalog(10, "gpt"))
            .unwrap();
        cache
            .store_catalog(Provider::Claude, catalog(5, "stale"))
            .unwrap();
        let catalogs = cache.catalogs(u64::MAX);
        assert_eq!(catalogs[&Provider::Claude], catalog(10, "opus"));
        assert_eq!(catalogs[&Provider::Codex], catalog(10, "gpt"));
        assert_eq!(cache.load(u64::MAX).len(), 1, "the figures stay");
        // Storing figures keeps the lists.
        cache.store("k", entry(2, "newer figures")).unwrap();
        assert_eq!(cache.catalogs(u64::MAX).len(), 2);
        // A list from a clock since set back is not read.
        cache
            .store_catalog(Provider::Omp, catalog(20 * CLOCK_SLACK, "ahead"))
            .unwrap();
        assert!(cache.catalogs(u64::MAX).contains_key(&Provider::Omp));
        assert!(!cache.catalogs(CLOCK_SLACK).contains_key(&Provider::Omp));
    }

    /// A list this build cannot read - a provider added since, or a model
    /// described differently - costs only that list, never the figures.
    #[test]
    fn an_unreadable_list_costs_only_itself() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(
            directory.path().join(FILE_NAME),
            br#"{"version": 1, "entries": {}, "catalogs": {
                "Gemini": {"models": [], "observed_at": 1},
                "Codex": {"models": "nonsense", "observed_at": 1},
                "Claude": {"models": [{"id": "opus", "label": "Opus", "reasoning": []}],
                           "observed_at": 3}}}"#,
        )
        .unwrap();
        let cache = Cache::at(directory.path());
        let catalogs = cache.catalogs(u64::MAX);
        assert_eq!(catalogs.keys().collect::<Vec<_>>(), [&Provider::Claude]);
        cache.store("k", entry(1, "figures")).unwrap();
        assert_eq!(cache.load(u64::MAX).len(), 1);
        // A file from before lists were kept reads as having none.
        fs::write(
            directory.path().join(FILE_NAME),
            br#"{"version": 1, "entries": {}}"#,
        )
        .unwrap();
        assert!(cache.catalogs(u64::MAX).is_empty());
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
