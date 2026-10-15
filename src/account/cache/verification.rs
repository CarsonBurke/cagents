//! Shared identity invalidations are independent of quota-cache rewrites.
use super::*;
use std::os::unix::fs::MetadataExt;

const FILE: &str = "account-identity-verification.json";
const LIMIT: usize = 4096;
const VERIFICATION_VERSION: u32 = 2;

#[derive(Serialize, Deserialize)]
struct Pending {
    version: u32,
    epoch: uuid::Uuid,
    #[serde(default)]
    blocked: bool,
    entries: BTreeMap<String, Barrier>,
}
#[derive(Serialize, Deserialize)]
struct Barrier {
    nonce: uuid::Uuid,
    revision: u64,
    observed_at: u64,
    detail: String,
    /// Retain the generation after successful verification so a pre-change
    /// token cannot become current again (None -> pending -> None ABA).
    pending: bool,
}
/// Captured before provider authentication, not inferred from usage time.
pub(in crate::account) struct Token {
    key: String,
    nonce: Option<(uuid::Uuid, u64)>,
    pending: bool,
}
impl Token {
    pub(in crate::account) fn pending(&self) -> bool { self.pending }
}
impl Default for Pending {
    fn default() -> Self { Self { version: VERIFICATION_VERSION, epoch: uuid::Uuid::new_v4(), blocked: false, entries: BTreeMap::new() } }
}
impl Pending {
    fn read(cache: &Cache) -> io::Result<Self> {
        let path = cache.directory.join(FILE);
        let file = match OpenOptions::new().read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC).open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Self::default()),
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
        if state.version != VERIFICATION_VERSION || state.entries.len() > LIMIT || state.blocked
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
    fn invalidate(&mut self, key: &str, observed_at: u64, detail: &str) -> io::Result<()> {
        if key.len() > 16384 || detail.len() > 1024 || (self.entries.len() >= LIMIT && !self.entries.contains_key(key)) {
            self.blocked = true;
            self.entries.clear();
            return Err(io::Error::other("Account identity verification capacity exceeded; repair the private account cache before routing"));
        }
        let Some(revision) = self.entries.get(key).map_or(Some(1), |old|old.revision.checked_add(1)) else {
            self.blocked = true;
            self.entries.clear();
            return Err(io::Error::other("Account identity verification revision exhausted; repair the private account cache before routing"));
        };
        self.entries.insert(key.into(), Barrier { nonce: uuid::Uuid::new_v4(), revision, observed_at,
            detail: detail.into(), pending: true });
        Ok(())
    }
    fn stamp(&self, key: &str) -> Option<IdentityVerification> {
        self.entries.get(key).map(|barrier| IdentityVerification { epoch: self.epoch,
            revision: barrier.revision, pending: barrier.pending })
    }
    fn overlay(&self, key: &str, entry: &mut Entry) {
        entry.verification = self.stamp(key);
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
        Ok(Token { key: key.into(), nonce: state.entries.get(key).map(|barrier|(barrier.nonce,barrier.revision)), pending: state.entries.get(key).is_some_and(|barrier|barrier.pending) })
    }
    /// Only the real provider-probe path calls this with its pre-auth token.
    /// Commit the entry durably before clearing the matching invalidation.
    pub(in crate::account) fn finish_verification(&self, token: Token, entry: Entry, persist: bool) -> io::Result<Entry> {
        let lock = self.lock(libc::LOCK_EX)?;
        let mut state = Pending::read(self)?;
        if state.entries.get(&token.key).map(|barrier|(barrier.nonce,barrier.revision)) != token.nonce {
            return Err(io::Error::other("The account sign-in changed during verification; retry checking this account"));
        }
        let confirmed = entry.identity.is_some() && entry.snapshot.status != super::super::UsageStatus::NeedsLogin;
        let mut document = if persist { read_document(&self.path()).unwrap_or_default() } else { Document::default() };
        let identity_changed = persist && confirmed
            && document.entries.get(&token.key).and_then(|old|old.identity.as_ref()) != entry.identity.as_ref();
        let mut visible = entry;
        // An actual negative authentication verdict is itself a new identity
        // invalidation, even without a separate provider login-change event.
        // Commit it before touching quota; only later positive auth may clear.
        let negative_auth = persist && visible.snapshot.status == super::super::UsageStatus::NeedsLogin;
        if negative_auth && !state.entries.get(&token.key).is_some_and(|barrier|barrier.pending) {
            let verdict = state.invalidate(&token.key,visible.snapshot.observed_at,
                "The provider needs this account to sign in again");
            self.save_invalidation(&lock,&state,|state|state.write(self))?;
            verdict?;
        }
        if identity_changed && !state.entries.get(&token.key).is_some_and(|barrier|barrier.pending) {
            // Fence the previous identity before replacing its quota, even
            // for an external sign-in with no streamed change notification.
            // A failed final sidecar write then leaves a recoverable pending
            // verdict, never a new identity carrying the previous revision.
            let verdict = state.invalidate(&token.key,visible.snapshot.observed_at,
                "The account identity changed during verification");
            self.save_invalidation(&lock,&state,|state|state.write(self))?;
            verdict?;
        }
        let verified = persist && confirmed && (identity_changed
            || state.entries.get(&token.key).is_some_and(|barrier|barrier.pending));
        if verified {
            // An actual changed identity is causal evidence even when no
            // streamed Login event preceded it and quota time moved backward.
            if let Err(error) = state.invalidate(&token.key,visible.snapshot.observed_at,
                "The account identity was verified") {
                self.save_invalidation(&lock,&state,|state|state.write(self))?;
                return Err(error);
            }
            state.entries.get_mut(&token.key).unwrap().pending = false;
        }
        // The sidecar on disk still holds pending until AFTER the verified
        // quota entry commits. Its overlay wins across any intervening crash.
        state.overlay(&token.key, &mut visible);
        if persist {
            document.version = VERSION;
            if token.pending || negative_auth || identity_changed || document.entries.get(&token.key).is_none_or(|old| !supersedes(old.snapshot.observed_at, visible.snapshot.observed_at)) {
                document.entries.insert(token.key.clone(), visible.clone());
                self.write(&document)?;
                self.sync_directory()?;
            } else {
                // A newer entry must not be replaced or have its identity
                // barrier cleared by this older completed probe.
                return Err(io::Error::other("A newer account observation arrived during verification; retry checking this account"));
            }
            if verified { state.write(self)?; }
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
        let verdict = state.invalidate(key,observed_at,detail);
        // Persist the invalidation (including global capacity refusal) first.
        // A crash before quota replacement must leave matched readers blocked.
        self.save_invalidation(lock,state,save)?;
        verdict?;
        let mut entry = read_document(&self.path()).and_then(|mut document|document.entries.remove(key))
            .unwrap_or_else(||Entry { verification: None, identity: None, snapshot: UsageSnapshot::state(super::super::UsageStatus::NeedsLogin, detail, observed_at) });
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
        for (key,entry) in entries.iter_mut() { state.overlay(key,entry); }
        // The sidecar is independent authority for pending identity. Losing,
        // pruning or rejecting the quota document must not hide that verdict.
        for (key,barrier) in state.entries.iter().filter(|(_,barrier)|barrier.pending) {
            entries.entry(key.clone()).or_insert_with(|| {
                let mut entry = Entry { verification: None, identity: None,
                    snapshot: UsageSnapshot::state(super::super::UsageStatus::NeedsLogin,
                        &barrier.detail,barrier.observed_at) };
                state.overlay(key,&mut entry);
                entry
            });
        }
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
        Entry { verification: None, identity: Some(Identity { email: Some(email.into()), plan: Some("pro".into()), organization: None }), snapshot }
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
                "future" => std::fs::write(&path, br#"{"version":999,"entries":{}}"#).unwrap(),
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
        let mut pending = Pending::default();
        for index in 0..LIMIT {
            pending.entries.insert(index.to_string(), Barrier { nonce: uuid::Uuid::new_v4(), revision: 1, observed_at: 1, detail: "Verify".into(), pending: true });
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

    #[test]
    fn identity_revision_exhaustion_never_wraps_to_old_ui_authority() {
        let root = tempfile::tempdir_in("/var/tmp").unwrap();
        let cache = Cache::at(root.path());
        cache.store(KEY,reading(1,UsageSource::Probe,"old@fixture.invalid")).unwrap();
        let mut state = Pending::default();
        state.entries.insert(KEY.into(),Barrier { nonce: uuid::Uuid::new_v4(), revision: u64::MAX,
            observed_at: 1,detail: "Verify".into(),pending: false });
        state.write(&cache).unwrap();
        assert!(cache.invalidate_identity(KEY,10,"Login changed").is_err());
        assert!(cache.load(20).is_empty());
        assert!(cache.begin_verification(KEY).is_err());
    }
    #[test]
    fn generic_writer_stamp_is_always_replaced_by_private_sidecar_verdict() {
        let root = tempfile::tempdir_in("/var/tmp").unwrap();
        let cache = Cache::at(root.path());
        cache.store(KEY,reading(1,UsageSource::Probe,"old@fixture.invalid")).unwrap();
        cache.invalidate_identity(KEY,10,"Login changed").unwrap();
        let pending = cache.load(10).remove(KEY).unwrap();
        let mut generic = reading(20,UsageSource::Live,"old@fixture.invalid");
        generic.verification = Some(IdentityVerification { epoch: uuid::Uuid::new_v4(),revision: u64::MAX,pending: false });
        cache.store(KEY,generic).unwrap();
        let stored = cache.load(20).remove(KEY).unwrap();
        assert_eq!(stored.verification,pending.verification);
        assert_eq!(stored.snapshot.status,UsageStatus::NeedsLogin);
    }

    #[test]
    fn verified_revision_exhaustion_keeps_pending_identity_blocked() {
        let root = tempfile::tempdir_in("/var/tmp").unwrap();
        let cache = Cache::at(root.path());
        cache.store(KEY,reading(1,UsageSource::Probe,"old@fixture.invalid")).unwrap();
        let mut state = Pending::default();
        state.entries.insert(KEY.into(),Barrier { nonce: uuid::Uuid::new_v4(), revision: u64::MAX,
            observed_at: 1,detail: "Verify".into(),pending: true });
        state.write(&cache).unwrap();
        assert!(cache.fixture_verify_identity(KEY,reading(20,UsageSource::Probe,"new@fixture.invalid")).is_err());
        assert!(cache.load(20).is_empty(),"exhaustion persists global refusal instead of wrapping");
        assert!(cache.begin_verification(KEY).is_err());
        assert!(cache.store(KEY,reading(30,UsageSource::Live,"old@fixture.invalid")).is_err());
        assert_eq!(read_document(&cache.path()).unwrap().entries[KEY].identity.as_ref().unwrap().email.as_deref(),
            Some("old@fixture.invalid"),"the failed verification never replaced underlying quota");
    }

    #[test]
    fn pending_identity_remains_visible_when_quota_document_or_entry_is_missing() {
        for missing_document in [false,true] {
            let root = tempfile::tempdir_in("/var/tmp").unwrap();
            let cache = Cache::at(root.path());
            cache.store(KEY,reading(1,UsageSource::Probe,"old@fixture.invalid")).unwrap();
            cache.invalidate_identity(KEY,10,"Login changed").unwrap();
            if missing_document { std::fs::remove_file(cache.path()).unwrap(); }
            else { cache.write(&Document { version: VERSION,..Document::default() }).unwrap(); }
            let reopened = Cache::at(root.path());
            assert_pending(&reopened,20);
        }
    }
    #[test]
    fn actual_negative_authentication_creates_a_new_pending_causal_verdict() {
        let root = tempfile::tempdir_in("/var/tmp").unwrap();
        let cache = Cache::at(root.path());
        cache.store(KEY,reading(100,UsageSource::Probe,"old@fixture.invalid")).unwrap();
        cache.invalidate_identity(KEY,100,"First change").unwrap();
        let confirmed = cache.fixture_verify_identity(KEY,reading(100,UsageSource::Probe,"current@fixture.invalid")).unwrap();
        let negative = cache.finish_verification(cache.begin_verification(KEY).unwrap(),Entry { verification: None,
            identity: None,snapshot: UsageSnapshot::state(UsageStatus::NeedsLogin,"Signed out",99) },true);
        let negative = negative.unwrap();
        assert_eq!(negative.snapshot.status,UsageStatus::NeedsLogin);
        assert_eq!(negative.snapshot.observed_at,99,"negative authentication must not be discarded or backdated by quota precedence");
        let pending = cache.load(100).remove(KEY).unwrap();
        assert!(pending.verification.as_ref().unwrap().pending());
        assert!(adopt_identity_verdict(pending.verification.as_ref(),confirmed.verification.as_ref(),
            &pending.snapshot,Some(&confirmed.snapshot),false));
        assert!(pending.identity.is_none());
        assert_eq!(pending.snapshot.status,UsageStatus::NeedsLogin);
        cache.store(KEY,reading(200,UsageSource::Live,"current@fixture.invalid")).unwrap();
        assert_pending(&cache,200);
    }

    #[test]
    fn actual_changed_identity_outranks_newer_quota_without_a_streamed_login_event() {
        let root = tempfile::tempdir_in("/var/tmp").unwrap();
        let cache = Cache::at(root.path());
        let previous = cache.fixture_verify_identity(KEY,reading(200,UsageSource::Probe,"old@fixture.invalid")).unwrap();
        let before_change = cache.begin_verification(KEY).unwrap();
        let current = cache.fixture_verify_identity(KEY,reading(199,UsageSource::Probe,"new@fixture.invalid")).unwrap();
        assert_eq!(current.snapshot.observed_at,199);
        assert_eq!(current.identity.as_ref().unwrap().email.as_deref(),Some("new@fixture.invalid"));
        assert!(adopt_identity_verdict(current.verification.as_ref(),previous.verification.as_ref(),
            &current.snapshot,Some(&previous.snapshot),false));
        assert!(cache.finish_verification(before_change,reading(201,UsageSource::Probe,"old@fixture.invalid"),true).is_err());
        assert_eq!(cache.load(201).remove(KEY).unwrap(),current);
    }

}
