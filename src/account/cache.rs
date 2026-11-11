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

#[derive(Debug, Default, Serialize, Deserialize)]
struct Document {
    version: u32,
    entries: BTreeMap<String, Entry>,
    /// Added after the first version, which the version does not count: a
    /// build that predates it reads the rest, and drops it when it writes.
    #[serde(default, deserialize_with = "readable_catalogs")]
    catalogs: BTreeMap<Provider, Catalog>,
    /// Added after the first version, like `catalogs`: each account's
    /// settled disagreement between two records of its login, by account
    /// key. A build that predates it drops it when it writes, which only
    /// shows a settled disagreement again until it is settled again. Each
    /// is read on its own, so one this build cannot read costs only itself.
    #[serde(
        default,
        skip_serializing_if = "BTreeMap::is_empty",
        deserialize_with = "readable_settlements"
    )]
    settled: BTreeMap<String, Settled>,
    /// Optional profile display metadata is tied to one login, not usage age.
    #[serde(
        default,
        skip_serializing_if = "BTreeMap::is_empty",
        deserialize_with = "readable_plan_profiles"
    )]
    plan_profiles: BTreeMap<String, PlanProfile>,
    #[serde(
        default,
        skip_serializing_if = "BTreeMap::is_empty",
        deserialize_with = "readable_plan_epochs"
    )]
    plan_epochs: BTreeMap<String, u64>,
}

/// Only allowlisted, bounded display metadata survives a verified profile.
/// A failed lookup is retained too, so usage probes never become a profile poll.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PlanProfile {
    pub binding: String,
    pub verified: bool,
    pub display_name: Option<String>,
    pub rate_limit_tier: Option<String>,
    observed_at: u64,
}

impl PlanProfile {
    pub(crate) fn new(
        binding: String,
        verified: bool,
        display_name: Option<String>,
        rate_limit_tier: Option<String>,
    ) -> Self {
        Self {
            binding,
            verified,
            display_name,
            rate_limit_tier,
            observed_at: crate::chat::now_ms().unwrap_or_default(),
        }
    }

    fn valid(&self) -> bool {
        let bounded = |value: &Option<String>| {
            value.as_ref().is_none_or(|value| {
                !value.is_empty()
                    && value.chars().count() <= 256
                    && !value.chars().any(char::is_control)
            })
        };
        self.binding.len() == 64
            && self.binding.bytes().all(|byte| byte.is_ascii_hexdigit())
            && bounded(&self.display_name)
            && bounded(&self.rate_limit_tier)
            && (self.verified || self.display_name.is_none() && self.rate_limit_tier.is_none())
    }
}

fn readable_plan_profiles<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<BTreeMap<String, PlanProfile>, D::Error> {
    let serde_json::Value::Object(profiles) = serde_json::Value::deserialize(deserializer)? else {
        return Ok(BTreeMap::new());
    };
    Ok(profiles
        .into_iter()
        .filter_map(|(key, value)| {
            let profile = PlanProfile::deserialize(value).ok()?;
            profile.valid().then_some((key, profile))
        })
        .collect())
}

fn readable_plan_epochs<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<BTreeMap<String, u64>, D::Error> {
    let serde_json::Value::Object(epochs) = serde_json::Value::deserialize(deserializer)? else {
        return Ok(BTreeMap::new());
    };
    Ok(epochs
        .into_iter()
        .filter_map(|(key, value)| value.as_u64().map(|value| (key, value)))
        .collect())
}

fn invalidate_document_plan(document: &mut Document, key: &str) {
    document.plan_profiles.remove(key);
    let epoch = document.plan_epochs.entry(key.to_owned()).or_default();
    *epoch = epoch.wrapping_add(1);
}

/// How two records of one login's identity, which named different logins,
/// were settled (see [`Identity::disputed_by`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Settled {
    /// A sign-in that cagents ran for the account finished at `at`, in Unix
    /// milliseconds: the record the CLI reads is the one that sign-in wrote,
    /// so the disagreement the probe that follows it sees is settled in its
    /// favour (see [`super::claude::SIGN_IN_SETTLES_FOR`]).
    SignedIn { at: u64 },
    /// The CLI's record, naming `reported`, was written by a sign-in after
    /// the other record, naming `other`: the disagreement between exactly
    /// these two is settled, and any other is shown again.
    Reported { reported: String, other: String },
}

fn readable_settlements<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<BTreeMap<String, Settled>, D::Error> {
    let serde_json::Value::Object(settled) = serde_json::Value::deserialize(deserializer)? else {
        return Ok(BTreeMap::new());
    };
    Ok(settled
        .into_iter()
        .filter_map(|(key, settled)| Some((key, Settled::deserialize(settled).ok()?)))
        .collect())
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

    /// Remembers `entry` for `key`, unless another writer already stored a
    /// newer observation. Background writes may arrive long after observation;
    /// only a stamp ahead of the current clock by more than [`CLOCK_SLACK`]
    /// indicates a clock that has since been set back and is replaced.
    pub fn store(&self, key: &str, entry: Entry) -> io::Result<()> {
        crate::secure_file::prepare_private_directory(&self.directory)?;
        let _lock = self.lock(libc::LOCK_EX)?;
        let path = self.path();
        let mut document = read_document(&path).unwrap_or_default();
        document.version = VERSION;
        // Read after acquiring the lock: even waiting to publish can outlast
        // CLOCK_SLACK, without a clock rollback having occurred.
        let now = crate::chat::now_ms()
            .ok_or_else(|| io::Error::other("The system clock is before the Unix epoch"))?;
        if document.entries.get(key).is_some_and(|existing| {
            supersedes(
                existing.snapshot.observed_at,
                entry.snapshot.observed_at,
                now,
            )
        }) {
            return Ok(());
        }
        document.entries.insert(key.to_owned(), entry);
        self.write(&document)
    }

    /// The models each provider last listed, as of `now`. Like
    /// [`Cache::load`], a list stamped in the future is left out.
    pub fn catalogs(&self, now: u64) -> BTreeMap<Provider, Catalog> {
        read_document(&self.path())
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
        let path = self.path();
        let mut document = read_document(&path).unwrap_or_default();
        document.version = VERSION;
        let now = crate::chat::now_ms()
            .ok_or_else(|| io::Error::other("The system clock is before the Unix epoch"))?;
        if document
            .catalogs
            .get(&provider)
            .is_some_and(|existing| supersedes(existing.observed_at, catalog.observed_at, now))
        {
            return Ok(());
        }
        document.catalogs.insert(provider, catalog);
        self.write(&document)
    }

    /// Serializes just this account's profile lookup across windows. This is
    /// separate from the short document lock; usage writes stay independent.
    pub(crate) fn lock_plan(&self, key: &str) -> io::Result<crate::instance::OwnedLock> {
        use sha2::{Digest, Sha256};
        crate::secure_file::prepare_private_directory(&self.directory)?;
        let digest = Sha256::digest(key.as_bytes());
        let name = digest
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        crate::instance::owned_lock(
            &self.directory.join(format!("account-plan.{name}.lock")),
            libc::LOCK_EX,
        )
    }

    /// Current login generation and its one profile attempt, independent of
    /// usage timestamps, force refreshes and rotating access tokens.
    pub(crate) fn plan_profile(&self, key: &str, binding: &str) -> (u64, Option<PlanProfile>) {
        let document = read_document(&self.path()).unwrap_or_default();
        let epoch = document.plan_epochs.get(key).copied().unwrap_or_default();
        let profile = document
            .plan_profiles
            .get(key)
            .filter(|profile| profile.binding == binding)
            .cloned();
        (epoch, profile)
    }

    /// Publishes only into the login generation that started the request. A
    /// sign-out/sign-in while HTTP is pending cannot resurrect its old plan.
    pub(crate) fn store_plan_profile(
        &self,
        key: &str,
        epoch: u64,
        profile: PlanProfile,
    ) -> io::Result<bool> {
        if !profile.valid() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Profile display metadata is invalid",
            ));
        }
        crate::secure_file::prepare_private_directory(&self.directory)?;
        let _lock = self.lock(libc::LOCK_EX)?;
        let mut document = read_document(&self.path()).unwrap_or_default();
        if document.plan_epochs.get(key).copied().unwrap_or_default() != epoch {
            return Ok(false);
        }
        document.version = VERSION;
        document.plan_profiles.insert(key.to_owned(), profile);
        self.write(&document)?;
        Ok(true)
    }

    /// Explicit login transitions invalidate successful and failed profile
    /// attempts, including a sign-in that returns to the same email address.
    pub fn invalidate_plan(&self, key: &str) -> io::Result<()> {
        crate::secure_file::prepare_private_directory(&self.directory)?;
        let _lock = self.lock(libc::LOCK_EX)?;
        let mut document = read_document(&self.path()).unwrap_or_default();
        document.version = VERSION;
        invalidate_document_plan(&mut document, key);
        self.write(&document)
    }

    /// How the disagreement between two records of the login of the account
    /// filed under `key` was last settled, if it was.
    pub fn settled(&self, key: &str) -> Option<Settled> {
        read_document(&self.path())?.settled.remove(key)
    }

    /// Records how the disagreement over `key`'s login is settled, or with
    /// `None`, that nothing is.
    pub fn settle(&self, key: &str, settled: Option<Settled>) -> io::Result<()> {
        self.settle_with(key, |_, _| ((), Some(settled)))
    }

    /// Decides how the disagreement over `key`'s login is settled from the
    /// settlement on record and the identity last cached for `key`, with no
    /// other window settling it in between. `decide` returns its answer and
    /// `None` to leave the settlement, or the settlement to record instead.
    pub fn settle_with<T>(
        &self,
        key: &str,
        decide: impl FnOnce(Option<&Settled>, Option<&Identity>) -> (T, Option<Option<Settled>>),
    ) -> io::Result<T> {
        crate::secure_file::prepare_private_directory(&self.directory)?;
        let _lock = self.lock(libc::LOCK_EX)?;
        let mut document = read_document(&self.path()).unwrap_or_default();
        let previous = document
            .entries
            .get(key)
            .and_then(|entry| entry.identity.as_ref());
        let (answer, settle) = decide(document.settled.get(key), previous);
        let changed = match settle {
            None => false,
            Some(Some(settled)) => {
                let changed = document.settled.get(key) != Some(&settled);
                if changed && matches!(settled, Settled::SignedIn { .. }) {
                    invalidate_document_plan(&mut document, key);
                }
                document.settled.insert(key.to_owned(), settled);
                changed
            }
            Some(None) => document.settled.remove(key).is_some(),
        };
        if changed {
            document.version = VERSION;
            self.write(&document)?;
        }
        Ok(answer)
    }

    /// Forgets the entries no account in `keep` refers to that are older
    /// than [`ORPHAN_AFTER`]: the figures of an account since removed, or of
    /// a home that moved. A younger one stays, because another window may
    /// have an account this one has not read from the registry yet.
    pub fn prune(&self, keep: &BTreeSet<String>, now: u64) -> io::Result<()> {
        let path = self.path();
        // Most starts have nothing to forget, and read without the lock.
        let orphaned = |document: &Document| {
            document
                .entries
                .iter()
                .any(|(key, entry)| !keep.contains(key) && !current(entry, now))
                || document
                    .settled
                    .keys()
                    .any(|key| !keep.contains(key) && !document.entries.contains_key(key))
                || document.plan_profiles.iter().any(|(key, profile)| {
                    !keep.contains(key)
                        && (profile.observed_at > now.saturating_add(CLOCK_SLACK)
                            || now.saturating_sub(profile.observed_at) >= ORPHAN_AFTER)
                })
        };
        if !read_document(&path).is_some_and(|document| orphaned(&document)) {
            return Ok(());
        }
        let _lock = self.lock(libc::LOCK_EX)?;
        let Some(mut document) = read_document(&path).filter(orphaned) else {
            return Ok(());
        };
        document
            .entries
            .retain(|key, entry| keep.contains(key) || current(entry, now));
        let entries = &document.entries;
        document
            .settled
            .retain(|key, _| keep.contains(key) || entries.contains_key(key));
        document.plan_profiles.retain(|key, profile| {
            keep.contains(key)
                || profile.observed_at <= now.saturating_add(CLOCK_SLACK)
                    && now.saturating_sub(profile.observed_at) < ORPHAN_AFTER
        });
        // Keep invalidation tombstones even after an orphan's data is pruned:
        // deleting one could let a generation-zero in-flight read publish if
        // an account key is recreated. The document size remains bounded.

        self.write(&document)
    }

    /// Replaces the file with `document`, whole: written to a private
    /// temporary sibling and renamed over the original. The caller holds the
    /// lock.
    fn write(&self, document: &Document) -> io::Result<()> {
        let bytes = serde_json::to_vec(document).map_err(io::Error::other)?;
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
/// observation at `incoming`, and stays. Only a stamp further ahead of `now`
/// than [`CLOCK_SLACK`] is from a clock since set back, and is replaced.
fn supersedes(existing: u64, incoming: u64, now: u64) -> bool {
    existing > incoming && existing <= now.saturating_add(CLOCK_SLACK)
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
                disputed_by: None,
            }),
            snapshot,
        }
    }

    #[test]
    fn profile_plan_generation_blocks_old_writes_and_login_changes_keep_usage() {
        let directory = tempfile::tempdir().unwrap();
        let cache = Cache::at(directory.path());
        let key = "claude:/fixture/home";
        let binding = "a".repeat(64);
        cache.store(key, entry(10, "usage retained")).unwrap();
        let profile = PlanProfile::new(
            binding.clone(),
            true,
            Some("Ultra".into()),
            Some("default_claude_max_20x".into()),
        );
        assert!(cache.store_plan_profile(key, 0, profile.clone()).unwrap());
        assert_eq!(
            cache
                .plan_profile(key, &binding)
                .1
                .unwrap()
                .display_name
                .as_deref(),
            Some("Ultra")
        );
        cache
            .settle(key, Some(Settled::SignedIn { at: 20 }))
            .unwrap();
        let (epoch, cached) = cache.plan_profile(key, &binding);
        assert_eq!(epoch, 1);
        assert!(cached.is_none());
        assert!(
            !cache.store_plan_profile(key, 0, profile.clone()).unwrap(),
            "old request cannot publish after sign-in"
        );
        assert!(
            cache
                .store_plan_profile(key, epoch, profile.clone())
                .unwrap()
        );
        cache
            .settle(key, Some(Settled::SignedIn { at: 20 }))
            .unwrap();
        assert!(
            cache.plan_profile(key, &binding).1.is_some(),
            "reobserving one settlement isn't a new sign-in"
        );
        cache.invalidate_plan(key).unwrap();
        assert!(!cache.store_plan_profile(key, epoch, profile).unwrap());
        assert_eq!(
            cache.load(100)[key].snapshot.detail.as_deref(),
            Some("usage retained")
        );
    }

    #[test]
    fn pruning_orphaned_data_does_not_reset_profile_invalidation_generation() {
        let directory = tempfile::tempdir().unwrap();
        let cache = Cache::at(directory.path());
        let key = "claude:/removed-and-recreated";
        let binding = "c".repeat(64);
        cache.invalidate_plan(key).unwrap();
        cache.store("old-usage", entry(1, "orphan")).unwrap();
        cache.prune(&BTreeSet::new(), ORPHAN_AFTER + 2).unwrap();
        assert_eq!(cache.plan_profile(key, &binding).0, 1);
        let stale = PlanProfile::new(binding, true, Some("Old login".into()), None);
        assert!(!cache.store_plan_profile(key, 0, stale).unwrap());
    }

    #[test]
    fn malformed_optional_profile_metadata_preserves_usage_and_other_profiles() {
        let directory = tempfile::tempdir().unwrap();
        let cache = Cache::at(directory.path());
        cache.store("usage", entry(10, "retained")).unwrap();
        let binding = "b".repeat(64);
        let failed = PlanProfile::new(binding.clone(), false, None, None);
        assert!(cache.store_plan_profile("claude:/good", 0, failed).unwrap());
        let mut document: serde_json::Value =
            serde_json::from_slice(&fs::read(cache.path()).unwrap()).unwrap();
        document["plan_profiles"]["bad"] =
            serde_json::json!({"binding":"bad","verified":true,"display_name":"poisoned"});
        document["plan_epochs"] = serde_json::json!({"bad":"not a generation","claude:/good":0});
        fs::write(cache.path(), serde_json::to_vec(&document).unwrap()).unwrap();
        assert_eq!(
            cache.load(100)["usage"].snapshot.detail.as_deref(),
            Some("retained")
        );
        assert!(
            !cache
                .plan_profile("claude:/good", &binding)
                .1
                .unwrap()
                .verified
        );
        assert!(cache.plan_profile("bad", "bad").1.is_none());
        let invalid = PlanProfile::new(binding, true, Some("control\nname".into()), None);
        assert!(cache.store_plan_profile("bad", 0, invalid).is_err());
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

    #[test]
    fn delayed_probe_write_cannot_replace_newer_live_quota_evidence() {
        let directory = tempfile::tempdir().unwrap();
        let cache = Cache::at(directory.path());
        let now = crate::chat::now_ms().unwrap();
        let newer_at = now - 1_000;
        let older_at = newer_at - CLOCK_SLACK - 1;
        // Logical observation times model a held background write without
        // sleeping or changing the process clock. Both stamps are in the past.
        let mut held_probe = entry(older_at, "held successful probe");
        held_probe.identity = None;
        held_probe.snapshot.status = UsageStatus::Ok;
        let mut newer_live = entry(newer_at, "newer live quota rejection");
        newer_live.snapshot.status = UsageStatus::Limited;
        newer_live.snapshot.source = UsageSource::Live;
        newer_live.snapshot.ordinary_usage_allowed = Some(false);

        cache.store("codex:/fixture", newer_live.clone()).unwrap();
        cache.store("codex:/fixture", held_probe).unwrap();
        assert_eq!(
            cache.load(now).remove("codex:/fixture"),
            Some(newer_live),
            "a delayed probe must preserve the newer status, identity and timestamp"
        );
    }

    /// A clock set back after a probe must not freeze the figures until it
    /// catches up with the stamp.
    #[test]
    fn entries_stamped_in_the_future_are_neither_read_nor_kept() {
        let directory = tempfile::tempdir().unwrap();
        let cache = Cache::at(directory.path());
        let now = crate::chat::now_ms().unwrap();
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

    #[test]
    fn a_settled_disagreement_is_kept_beside_the_figures_until_its_account_is_gone() {
        let directory = tempfile::tempdir().unwrap();
        let cache = Cache::at(directory.path());
        assert_eq!(cache.settled("system"), None);
        cache
            .settle("system", Some(Settled::SignedIn { at: 5 }))
            .unwrap();
        cache.store("system", entry(1, "figures")).unwrap();
        assert_eq!(cache.settled("system"), Some(Settled::SignedIn { at: 5 }));
        let settled = Settled::Reported {
            reported: "me@example.com".into(),
            other: "other@example.com".into(),
        };
        cache.settle("system", Some(settled.clone())).unwrap();
        cache
            .settle("gone", Some(Settled::SignedIn { at: 5 }))
            .unwrap();
        assert_eq!(cache.settled("system"), Some(settled.clone()));
        assert_eq!(cache.load(10).len(), 1);
        // An account no one refers to takes its settlement with it.
        cache
            .prune(&BTreeSet::from(["system".to_owned()]), 100 * ORPHAN_AFTER)
            .unwrap();
        assert_eq!(cache.settled("system"), Some(settled));
        assert_eq!(cache.settled("gone"), None);
        cache.settle("system", None).unwrap();
        assert_eq!(cache.settled("system"), None);
    }

    /// A settlement a later build records in a way this one cannot read
    /// costs only itself, never the figures beside it.
    #[test]
    fn an_unreadable_settlement_costs_only_itself() {
        let directory = tempfile::tempdir().unwrap();
        let cache = Cache::at(directory.path());
        cache.store("system", entry(1, "figures")).unwrap();
        cache
            .settle("work", Some(Settled::SignedIn { at: 5 }))
            .unwrap();
        let path = cache.path();
        let mut document: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        document["settled"]["system"] = serde_json::json!({"state": "from_a_later_build"});
        std::fs::write(&path, document.to_string()).unwrap();
        assert_eq!(cache.load(10).len(), 1);
        assert_eq!(cache.settled("system"), None);
        assert_eq!(cache.settled("work"), Some(Settled::SignedIn { at: 5 }));
    }

    /// The settlement and the identity last cached are read, and the new
    /// settlement written, under one lock.
    #[test]
    fn a_settlement_is_decided_from_what_is_on_record() {
        let directory = tempfile::tempdir().unwrap();
        let cache = Cache::at(directory.path());
        cache.store("system", entry(1, "figures")).unwrap();
        let seen = cache
            .settle_with("system", |settled, identity| {
                assert_eq!(settled, None);
                let email = identity.and_then(|identity| identity.email.clone());
                (email, Some(Some(Settled::SignedIn { at: 7 })))
            })
            .unwrap();
        assert_eq!(seen.as_deref(), Some("user@example.com"));
        let kept = cache
            .settle_with("system", |settled, _| (settled.cloned(), None))
            .unwrap();
        assert_eq!(kept, Some(Settled::SignedIn { at: 7 }));
        assert_eq!(cache.settled("system"), Some(Settled::SignedIn { at: 7 }));
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

    #[test]
    fn delayed_catalog_write_preserves_newer_models_and_other_cached_data() {
        let directory = tempfile::tempdir().unwrap();
        let cache = Cache::at(directory.path());
        let now = crate::chat::now_ms().unwrap();
        let newer_at = now - 1_000;
        let older_at = newer_at - CLOCK_SLACK - 1;
        let held_catalog = catalog(older_at, "old-model");
        let newer_catalog = catalog(newer_at, "new-model");
        let other_catalog = catalog(newer_at, "other-provider-model");
        let figures = entry(newer_at, "other account figures");
        cache.store("other-account", figures.clone()).unwrap();
        cache
            .store_catalog(Provider::Claude, newer_catalog.clone())
            .unwrap();
        cache
            .store_catalog(Provider::Codex, other_catalog.clone())
            .unwrap();
        cache.store_catalog(Provider::Claude, held_catalog).unwrap();

        let catalogs = cache.catalogs(now);
        assert_eq!(catalogs[&Provider::Claude], newer_catalog);
        assert_eq!(catalogs[&Provider::Codex], other_catalog);
        assert_eq!(cache.load(now).remove("other-account"), Some(figures));
    }

    #[test]
    fn catalogs_stamped_in_the_future_are_neither_read_nor_kept() {
        let directory = tempfile::tempdir().unwrap();
        let cache = Cache::at(directory.path());
        let now = crate::chat::now_ms().unwrap();
        cache
            .store_catalog(Provider::Claude, catalog(now + 10 * CLOCK_SLACK, "ahead"))
            .unwrap();
        assert!(cache.catalogs(now).is_empty());
        cache
            .store_catalog(Provider::Claude, catalog(now, "current"))
            .unwrap();
        assert_eq!(
            cache.catalogs(now)[&Provider::Claude],
            catalog(now, "current")
        );
        cache
            .store_catalog(
                Provider::Claude,
                catalog(now + CLOCK_SLACK / 2, "other-window"),
            )
            .unwrap();
        cache
            .store_catalog(Provider::Claude, catalog(now, "late"))
            .unwrap();
        assert_eq!(
            cache.catalogs(now)[&Provider::Claude],
            catalog(now + CLOCK_SLACK / 2, "other-window")
        );
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
