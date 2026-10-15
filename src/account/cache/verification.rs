//! Shared identity invalidations are independent of quota-cache rewrites.
use super::*;
use std::os::unix::fs::MetadataExt;

const FILE: &str = "account-identity-verification.json";
const LIMIT: usize = 4096;

#[derive(Default, Serialize, Deserialize)]
struct Pending {
    version: u32,
    #[serde(default)]
    blocked: bool,
    entries: BTreeMap<String, Barrier>,
}
#[derive(Serialize, Deserialize)]
struct Barrier {
    nonce: uuid::Uuid,
    observed_at: u64,
    detail: String,
    /// Retain the generation after successful verification so a pre-change
    /// token cannot become current again (None -> pending -> None ABA).
    pending: bool,
}
/// Captured before provider authentication, not inferred from usage time.
pub(in crate::account) struct Token {
    key: String,
    nonce: Option<uuid::Uuid>,
    pending: bool,
}
impl Token {
    pub(in crate::account) fn pending(&self) -> bool { self.pending }
}
impl Pending {
    fn read(cache: &Cache) -> io::Result<Self> {
        let path = cache.directory.join(FILE);
        let file = match OpenOptions::new().read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC).open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Self { version: 1, ..Self::default() }),
            Err(error) => return Err(error),
        };
        let metadata = file.metadata()?;
        if !metadata.is_file() || metadata.uid() != unsafe { libc::geteuid() }
            || metadata.mode() & 0o077 != 0 || metadata.nlink() != 1 || metadata.len() > MAX_BYTES {
            return Err(io::Error::other("Account identity verification metadata is not a private bounded file"));
        }
        let mut bytes = Vec::new();
        file.take(MAX_BYTES + 1).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_BYTES { return Err(io::Error::other("Account identity verification metadata exceeds its size limit")); }
        let state: Self = serde_json::from_slice(&bytes).map_err(io::Error::other)?;
        if state.version != 1 || state.entries.len() > LIMIT || state.blocked
            || state.entries.iter().any(|(key,value)| key.len() > 16384 || value.detail.len() > 1024) {
            return Err(io::Error::other("Account identity verification metadata is unavailable; repair the private account cache before routing"));
        }
        Ok(state)
    }
    fn write(&self, cache: &Cache) -> io::Result<()> {
        let bytes = serde_json::to_vec(self).map_err(io::Error::other)?;
        if bytes.len() as u64 > MAX_BYTES { return Err(io::Error::other("Account identity verification metadata exceeds its size limit")); }
        let mut temporary = tempfile::Builder::new().prefix(".account-identity-verification.")
            .permissions(std::os::unix::fs::PermissionsExt::from_mode(0o600)).tempfile_in(&cache.directory)?;
        temporary.write_all(&bytes)?;
        temporary.as_file().sync_all()?;
        temporary.persist(cache.directory.join(FILE)).map_err(|error|error.error)?;
        cache.sync_directory()
    }
    fn overlay(&self, key: &str, entry: &mut Entry) {
        if let Some(barrier) = self.entries.get(key).filter(|barrier|barrier.pending) {
            let observed_at = entry.snapshot.observed_at.max(barrier.observed_at);
            entry.identity = None;
            entry.snapshot = UsageSnapshot::state(super::super::UsageStatus::NeedsLogin, &barrier.detail, observed_at);
            entry.snapshot.source = super::super::UsageSource::Cache;
        }
    }
}
impl Cache {
    fn sync_directory(&self) -> io::Result<()> {
        std::fs::File::open(&self.directory)?.sync_all()
    }
    pub(in crate::account) fn begin_verification(&self, key: &str) -> io::Result<Token> {
        let _lock = self.lock(libc::LOCK_SH)?;
        let state = Pending::read(self)?;
        Ok(Token { key: key.into(), nonce: state.entries.get(key).map(|barrier|barrier.nonce), pending: state.entries.get(key).is_some_and(|barrier|barrier.pending) })
    }
    /// Only the real provider-probe path calls this with its pre-auth token.
    /// Commit the entry durably before clearing the matching invalidation.
    pub(in crate::account) fn finish_verification(&self, token: Token, entry: Entry, persist: bool) -> io::Result<Entry> {
        let _lock = self.lock(libc::LOCK_EX)?;
        let mut state = Pending::read(self)?;
        if state.entries.get(&token.key).map(|barrier|barrier.nonce) != token.nonce {
            return Err(io::Error::other("The account sign-in changed during verification; retry checking this account"));
        }
        let confirmed = entry.identity.is_some() && entry.snapshot.status != super::super::UsageStatus::NeedsLogin;
        let mut visible = entry;
        if !confirmed { state.overlay(&token.key, &mut visible); }
        if persist {
            let mut document = read_document(&self.path()).unwrap_or_default();
            document.version = VERSION;
            if token.pending || document.entries.get(&token.key).is_none_or(|old| !supersedes(old.snapshot.observed_at, visible.snapshot.observed_at)) {
                document.entries.insert(token.key.clone(), visible.clone());
                self.write(&document)?;
                self.sync_directory()?;
            } else {
                // A newer entry must not be replaced or have its identity
                // barrier cleared by this older completed probe.
                return Err(io::Error::other("A newer account observation arrived during verification; retry checking this account"));
            }
            if confirmed && let Some(barrier) = state.entries.get_mut(&token.key).filter(|barrier|barrier.pending) {
                barrier.pending = false;
                state.write(self)?;
            }
        }
        Ok(visible)
    }
    pub(crate) fn invalidate_identity(&self, key: &str, observed_at: u64, detail: &str) -> io::Result<Entry> {
        crate::secure_file::prepare_private_directory(&self.directory)?;
        let lock = self.lock(libc::LOCK_EX)?;
        let mut state = Pending::read(self)?;
        self.invalidate_identity_locked(&lock, &mut state, key, observed_at, detail, |state| state.write(self))
    }
    fn invalidate_identity_locked(
        &self,
        lock: &crate::instance::OwnedLock,
        state: &mut Pending,
        key: &str,
        observed_at: u64,
        detail: &str,
        save: impl FnOnce(&Pending) -> io::Result<()>,
    ) -> io::Result<Entry> {
        if key.len() > 16384 || detail.len() > 1024 || (state.entries.len() >= LIMIT && !state.entries.contains_key(key)) {
            state.blocked = true;
            state.entries.clear();
            self.save_invalidation(lock, state, save)?;
            return Err(io::Error::other("Account identity verification capacity exceeded; repair the private account cache before routing"));
        }
        state.entries.insert(key.into(), Barrier { nonce: uuid::Uuid::new_v4(), observed_at, detail: detail.into(), pending: true });
        // Persist the invalidation first. A crash before quota replacement
        // must leave every reader blocked, including after a process restart.
        self.save_invalidation(lock, state, save)?;
        let mut entry = read_document(&self.path()).and_then(|mut document|document.entries.remove(key))
            .unwrap_or_else(||Entry { identity: None, snapshot: UsageSnapshot::state(super::super::UsageStatus::NeedsLogin, detail, observed_at) });
        state.overlay(key, &mut entry);
        Ok(entry)
    }
    fn save_invalidation(
        &self,
        lock: &crate::instance::OwnedLock,
        state: &Pending,
        save: impl FnOnce(&Pending) -> io::Result<()>,
    ) -> io::Result<()> {
        if let Err(error) = save(state) {
            // Keep every matched reader/writer fail-closed even when an
            // atomic sidecar replacement cannot allocate space. The lock
            // inode already exists and this sparse length needs no payload.
            // If the filesystem cannot commit even inode metadata, report
            // both failures; no successful persistence is claimed.
            let poisoned = lock.set_len(1).and_then(|()| lock.sync_all());
            return Err(match poisoned {
                Ok(()) => io::Error::other(format!(
                    "Account identity invalidation failed and the cache was disabled: {error}"
                )),
                Err(poison) => io::Error::other(format!(
                    "Account identity invalidation could not be saved ({error}) or the cache disabled ({poison}); restore writable storage before routing"
                )),
            });
        }
        Ok(())
    }
    #[cfg(test)]
    pub(crate) fn fixture_verify_identity(&self, key: &str, entry: Entry) -> io::Result<Entry> {
        self.finish_verification(self.begin_verification(key)?, entry, true)
    }
    pub(super) fn overlay_verification(&self, entries: &mut BTreeMap<String, Entry>) -> io::Result<()> {
        let state = Pending::read(self)?;
        for (key,entry) in entries { state.overlay(key,entry); }
        Ok(())
    }
    pub(super) fn protect_verification(&self, key: &str, entry: &mut Entry, existing: Option<&Entry>) -> io::Result<()> {
        let state = Pending::read(self)?;
        if state.entries.get(key).is_some_and(|barrier| !barrier.pending)
            && existing.and_then(|entry|entry.identity.as_ref()) != entry.identity.as_ref() {
            return Err(io::Error::other("Only a fresh identity probe can replace the verified account identity"));
        }
        state.overlay(key,entry);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account::{UsageSource, UsageStatus};
    use std::os::unix::fs::{PermissionsExt, symlink};
    const KEY: &str = "codex:/private-fixture/home";
    fn reading(at: u64, source: UsageSource, email: &str) -> Entry {
        let mut snapshot = UsageSnapshot::state(UsageStatus::Ok, "", at);
        snapshot.source = source;
        Entry { identity: Some(Identity { email: Some(email.into()), plan: Some("pro".into()), organization: None }), snapshot }
    }
    fn assert_pending(cache: &Cache, now: u64) {
        let entry = cache.load(now).remove(KEY).unwrap();
        assert_eq!(entry.snapshot.status, UsageStatus::NeedsLogin);
        assert!(entry.identity.is_none());
        assert!(cache.begin_verification(KEY).unwrap().pending());
    }
    #[test]
    fn pending_identity_survives_reopen_live_and_probe_looking_generic_writers() {
        let root = tempfile::tempdir_in("/var/tmp").unwrap();
        let cache = Cache::at(root.path());
        cache.store(KEY, reading(1, UsageSource::Probe, "old@fixture.invalid")).unwrap();
        cache.invalidate_identity(KEY, 10, "Verify the changed login").unwrap();
        let cache = Cache::at(root.path()); // No resident flag survives this reopen.
        assert_pending(&cache, 20);
        cache.store(KEY, reading(20, UsageSource::Live, "old@fixture.invalid")).unwrap();
        assert_pending(&cache, 20);
        cache.store(KEY, reading(30, UsageSource::Probe, "unproved@fixture.invalid")).unwrap();
        assert_pending(&cache, 30);
        // Even an older writer replacing the quota file cannot erase the
        // independent invalidation. This is not matched old-binary support.
        let document = Document { version: VERSION, entries: BTreeMap::from([(KEY.into(), reading(40, UsageSource::Live, "old@fixture.invalid"))]), catalogs: BTreeMap::new() };
        cache.write(&document).unwrap();
        assert_pending(&cache, 40);
        let token = cache.begin_verification(KEY).unwrap();
        let verified = cache.finish_verification(token, reading(50, UsageSource::Probe, "current@fixture.invalid"), true).unwrap();
        assert_eq!(verified.identity.unwrap().email.as_deref(), Some("current@fixture.invalid"));
        assert!(!cache.begin_verification(KEY).unwrap().pending());
        assert_eq!(cache.load(50)[KEY].snapshot.status, UsageStatus::Ok);
    }
    #[test]
    fn invalidation_after_probe_start_cannot_be_cleared_by_its_late_result() {
        let root = tempfile::tempdir_in("/var/tmp").unwrap();
        let cache = Cache::at(root.path());
        cache.store(KEY, reading(1, UsageSource::Probe, "old@fixture.invalid")).unwrap();
        let before = cache.begin_verification(KEY).unwrap();
        cache.invalidate_identity(KEY, 10, "Changed while checking").unwrap();
        assert!(cache.finish_verification(before, reading(20, UsageSource::Probe, "old@fixture.invalid"), true).is_err());
        assert_pending(&cache, 20);
        let before = cache.begin_verification(KEY).unwrap();
        cache.invalidate_identity(KEY, 30, "Changed again while checking").unwrap();
        assert!(cache.finish_verification(before, reading(40, UsageSource::Probe, "old@fixture.invalid"), true).is_err());
        assert_pending(&cache, 40);
        // Failed/Busy probes never invoke this authority. A completed probe
        // without positive identity cannot clear the pending nonce either.
        let before = cache.begin_verification(KEY).unwrap();
        let mut unknown = reading(50, UsageSource::Probe, "unused@fixture.invalid");
        unknown.identity = None;
        let result = cache.finish_verification(before, unknown, true).unwrap();
        assert_eq!(result.snapshot.status, UsageStatus::NeedsLogin);
        assert_pending(&cache, 50);
    }
    #[test]
    fn failed_verified_entry_write_keeps_the_pending_nonce_and_blocks_old_quota() {
        let root = tempfile::tempdir_in("/var/tmp").unwrap();
        let cache = Cache::at(root.path());
        cache.store(KEY, reading(1, UsageSource::Probe, "old@fixture.invalid")).unwrap();
        cache.invalidate_identity(KEY, 10, "Verify after login change").unwrap();
        let token = cache.begin_verification(KEY).unwrap();
        let saved = root.path().join("saved-quota.json");
        std::fs::rename(cache.path(), &saved).unwrap();
        std::fs::create_dir(cache.path()).unwrap();
        assert!(cache.finish_verification(token, reading(20, UsageSource::Probe, "new@fixture.invalid"), true).is_err());
        assert!(cache.begin_verification(KEY).unwrap().pending());
        std::fs::remove_dir(cache.path()).unwrap();
        std::fs::rename(saved, cache.path()).unwrap();
        assert_pending(&cache, 20);
    }
    #[test]
    fn malformed_future_oversize_symlink_and_fifo_sidecars_fail_closed() {
        for kind in ["malformed", "future", "oversize", "symlink", "fifo"] {
            let root = tempfile::tempdir_in("/var/tmp").unwrap();
            let cache = Cache::at(root.path());
            cache.store(KEY, reading(1, UsageSource::Probe, "old@fixture.invalid")).unwrap();
            let path = root.path().join(FILE);
            match kind {
                "malformed" => std::fs::write(&path, b"bad json").unwrap(),
                "future" => std::fs::write(&path, br#"{"version":2,"entries":{}}"#).unwrap(),
                "oversize" => std::fs::write(&path, vec![b' '; MAX_BYTES as usize + 1]).unwrap(),
                "symlink" => { let other = root.path().join("outside.json"); std::fs::write(&other, b"{}").unwrap(); symlink(other, &path).unwrap(); },
                "fifo" => { use std::os::unix::ffi::OsStrExt; let path = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap(); assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0); },
                _ => unreachable!(),
            }
            if kind != "symlink" { std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap(); }
            let started = std::time::Instant::now();
            assert!(cache.load(20).is_empty(), "{kind}");
            assert!(cache.begin_verification(KEY).is_err(), "{kind}");
            assert!(cache.store(KEY, reading(20, UsageSource::Live, "old@fixture.invalid")).is_err(), "{kind}");
            assert!(started.elapsed() < std::time::Duration::from_secs(1));
        }
    }
    #[test]
    fn failed_invalidation_commit_disables_every_cache_user_across_reopen() {
        let root = tempfile::tempdir_in("/var/tmp").unwrap();
        let cache = Cache::at(root.path());
        cache.store(KEY, reading(1, UsageSource::Probe, "old@fixture.invalid")).unwrap();
        let lock = cache.lock(libc::LOCK_EX).unwrap();
        let mut pending = Pending::read(&cache).unwrap();
        let failure = cache.invalidate_identity_locked(&lock, &mut pending, KEY, 10,
            "Login changed", |_| Err(io::Error::from_raw_os_error(libc::ENOSPC))).unwrap_err();
        assert!(failure.to_string().contains("cache was disabled"));
        assert_eq!(lock.metadata().unwrap().len(), 1);
        drop(lock);
        let reopened = Cache::at(root.path());
        assert!(reopened.load(20).is_empty());
        assert!(reopened.begin_verification(KEY).is_err());
        assert!(reopened.store(KEY, reading(20, UsageSource::Live, "old@fixture.invalid")).is_err());
        // The old positive cache still exists; refusal comes from the durable
        // lock marker, not an accidentally absent reading or resident flag.
        assert_eq!(read_document(&reopened.path()).unwrap().entries[KEY].snapshot.status, UsageStatus::Ok);
    }
    #[test]
    fn invalidation_capacity_failure_persists_global_refusal() {
        let root = tempfile::tempdir_in("/var/tmp").unwrap();
        let cache = Cache::at(root.path());
        cache.store(KEY, reading(1, UsageSource::Probe, "old@fixture.invalid")).unwrap();
        let mut pending = Pending { version: 1, ..Pending::default() };
        for index in 0..LIMIT {
            pending.entries.insert(index.to_string(), Barrier { nonce: uuid::Uuid::new_v4(), observed_at: 1, detail: "Verify".into(), pending: true });
        }
        pending.write(&cache).unwrap();
        assert!(cache.invalidate_identity(KEY, 10, "Login changed").is_err());
        let reopened = Cache::at(root.path());
        assert!(reopened.load(20).is_empty());
        assert!(reopened.begin_verification(KEY).is_err());
        assert!(reopened.store(KEY, reading(20, UsageSource::Live, "old@fixture.invalid")).is_err());
    }

    #[test]
    fn successful_new_verification_cannot_make_a_pre_change_token_current_again() {
        let root = tempfile::tempdir_in("/var/tmp").unwrap();
        let cache = Cache::at(root.path());
        cache.store(KEY, reading(1, UsageSource::Probe, "old@fixture.invalid")).unwrap();
        let before_change = cache.begin_verification(KEY).unwrap();
        cache.invalidate_identity(KEY, 10, "Login changed").unwrap();
        cache.fixture_verify_identity(KEY, reading(20, UsageSource::Probe, "new@fixture.invalid")).unwrap();
        assert!(!cache.begin_verification(KEY).unwrap().pending());
        assert!(cache.finish_verification(before_change,
            reading(30, UsageSource::Probe, "old@fixture.invalid"), true).is_err());
        assert_eq!(cache.load(30)[KEY].identity.as_ref().unwrap().email.as_deref(), Some("new@fixture.invalid"));
    }

    #[test]
    fn old_generic_writer_cannot_replace_a_newly_verified_identity() {
        let root = tempfile::tempdir_in("/var/tmp").unwrap();
        let cache = Cache::at(root.path());
        cache.store(KEY, reading(1, UsageSource::Probe, "old@fixture.invalid")).unwrap();
        cache.invalidate_identity(KEY, 10, "Login changed").unwrap();
        cache.fixture_verify_identity(KEY, reading(20, UsageSource::Probe, "new@fixture.invalid")).unwrap();
        assert!(cache.store(KEY, reading(30, UsageSource::Live, "old@fixture.invalid")).is_err());
        assert!(cache.store(KEY, reading(30, UsageSource::Probe, "unproved@fixture.invalid")).is_err());
        assert_eq!(cache.load(30)[KEY].identity.as_ref().unwrap().email.as_deref(), Some("new@fixture.invalid"));
    }

}
