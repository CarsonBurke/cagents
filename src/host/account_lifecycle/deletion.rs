//! Account deletion requires both scoped admission and complete ownership
//! evidence. A stopped provider, missing PID or missing socket is never a
//! retirement receipt. This module does not authorize session-specific purge.
use super::{Home, Reservation};
use crate::{
    account::{Account, AccountId, scope::LaunchAccountScope},
    daemon::holder_registry::{self as holders, Kind, Record},
    host::state::SharedHost,
};
use std::{
    collections::BTreeSet,
    fs::{File, Metadata, OpenOptions},
    io::Read,
    os::{fd::AsRawFd, unix::{fs::{FileTypeExt, MetadataExt, OpenOptionsExt}, net::UnixStream}},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

const MAX_ENTRIES: usize = 8192;
const MAX_RECORD: u64 = 256 * 1024;
const CENSUS_BUDGET: Duration = Duration::from_secs(10);

/// Cannot be deserialized, cloned, or constructed by an account/storage caller.
/// The borrow keeps the original mutation reservation alive through deletion.
pub(crate) struct DeletionPermit<'reservation> {
    reservation: &'reservation Reservation,
    host: &'reservation SharedHost,
    account: Account,
    home: &'reservation Home,
}

impl Reservation {
    pub(in crate::host) fn deletion_permit<'a>(
        &'a self,
        host: &'a SharedHost,
        account: &Account,
    ) -> Result<DeletionPermit<'a>, String> {
        let index = self.accounts.iter().position(|selected| selected == account)
            .ok_or("The requested deletion is outside this account reservation")?;
        let permit = DeletionPermit {
            reservation: self,
            host,
            account: account.clone(),
            home: &self.homes[index],
        };
        permit.verify()?;
        Ok(permit)
    }
}

impl DeletionPermit<'_> {
    /// Recheck immediately before a physical-home purge, inside its storage API.
    /// Optional registry aliases in the caller's scope are not authority.
    pub(crate) fn authorize_scope(&self, scope: &LaunchAccountScope) -> Result<(), String> {
        self.verify()?;
        let original = self.home.pin.as_ref().ok_or("The selected account home was absent")?
            .metadata().map_err(|e| e.to_string())?;
        if scope.provider != self.account.provider
            || scope.canonical_home != self.home.canonical
            || (scope.device, scope.inode) != (original.dev(), original.ino())
        {
            return Err("The purge scope is not the reserved physical account".into());
        }
        scope.revalidate()
    }

    /// Missing homes permit only the exact selected historical registration's
    /// purge index. This does not reconstruct a physical scope or grant reads.
    pub(crate) fn authorize_registered(&self, id: &AccountId) -> Result<(), String> {
        self.verify()?;
        if self.home.pin.is_some() || id != &self.account.id() {
            return Err("Historical purge is limited to this reserved missing account".into());
        }
        Ok(())
    }

    /// Storage must hold its registered-ID association lease while collecting
    /// the complete validated index and invoking this method. Reserve every
    /// historical namespace before touching any tombstone or payload. Guards
    /// remain installed until the account reservation is dropped, including
    /// after a failed census, so a partial error cannot reopen a writer race.
    pub(crate) fn reserve_registered_namespaces(&self, id: &AccountId, keys: &[String]) -> Result<(), String> {
        self.authorize_registered(id)?;
        if keys.len() > 4096 || keys.iter().any(|key| key.len() != 64
            || !key.bytes().all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)))
        { return Err("Historical account purge has an invalid or excessive namespace index".into()); }
        let gate = self.host.lock().unwrap_or_else(|p| p.into_inner()).control_gate.clone();
        {
            let _writer = gate.write().unwrap_or_else(|p| p.into_inner());
            self.reservation.verify()?;
            let other_homes = {
                let map = self.reservation.map.lock().unwrap_or_else(|p| p.into_inner());
                if map.iter().any(|(token, entry)| token != &self.reservation.token
                    && keys.iter().any(|key| entry.namespaces.contains(key)))
                { return Err("Another deletion already reserves this historical physical namespace".into()); }
                map.iter().filter(|(token, _)| *token != &self.reservation.token)
                    .flat_map(|(_, entry)| entry.homes.iter().cloned()).collect::<Vec<_>>()
            };
            // Also detect a concurrent physical-home reservation for a past
            // namespace. Its pinned inode, not the old registry label, counts.
            for home in &other_homes {
                    for provider in crate::account::Account::PROVIDERS {
                        if let Some(scope) = home.scope(provider)
                            && keys.contains(&crate::chat::annotations::storage::account_key(&scope))
                        { return Err("Another account operation owns a historical purge namespace".into()); }
                    }
            }
            let mut map = self.reservation.map.lock().unwrap_or_else(|p| p.into_inner());
            let selected = map.get_mut(&self.reservation.token)
                .ok_or("The historical purge reservation was released")?;
            if selected.namespaces.iter().chain(keys).collect::<BTreeSet<_>>().len() > 4096 {
                return Err("The account reservation exceeds its historical namespace bound".into());
            }
            selected.namespaces.extend(keys.iter().cloned());
        }
        self.verify()
    }

    pub(crate) fn verify(&self) -> Result<(), String> {
        self.reservation.verify()?;
        let namespaces = self.reservation.map.lock().unwrap_or_else(|p| p.into_inner())
            .get(&self.reservation.token).ok_or("The account deletion reservation was released")?
            .namespaces.clone();
        complete_census(self.host, &self.account, self.home, &namespaces)?;
        self.reservation.verify()
    }
}

fn complete_census(host: &SharedHost, account: &Account, home: &Home, namespaces: &BTreeSet<String>) -> Result<(), String> {
    let deadline = Instant::now() + CENSUS_BUDGET;
    crate::host::client::ensure_account_homes_unused(host, vec![account.clone()])
        .map_err(|e| e.message)?;
    let (registry, installed_terminals, proof, current_scopes) = {
        let state = host.lock().unwrap_or_else(|p| p.into_inner());
        if !state.native_adoption_failures.is_empty()
            || !state.native_closing.is_empty() || !state.chat_bindings.is_empty()
            || !state.execution_startups.is_empty()
        {
            return Err("Account ownership is unresolved during native adoption, closure or launch; finish that operation before deleting files".into());
        }
        let registry = state.execution_registry.clone()
            .ok_or("Account deletion requires the complete durable ownership registry")?;
        let installed = state.terminals.values()
            .filter_map(|record| record.handle.holder_identity().cloned())
            .collect::<Vec<_>>();
        let scopes = state.chats.values().map(|record| {
            record.handle.launch_account_scope().cloned().map_err(str::to_owned)
        }).collect::<Vec<_>>();
        (registry, installed, state.proof().clone(), scopes)
    };
    // Include completed/unsubscribed records too: generic completion or Stop
    // is not final retirement. Explicit management close removes the record.
    for scope in current_scopes {
        let scope = scope.map_err(|_| "A hosted conversation has unknown account ownership; explicitly close it before deleting files")?;
        if overlaps(home, account, &scope, namespaces)? {
            return Err("A hosted conversation still owns this physical account; explicitly close it before deleting files".into());
        }
    }
    // Read-only strict inventory: unlike discover(), this never skips or
    // removes stale/malformed entries, and absence never becomes retirement.
    let mut evidence = Vec::new();
    for kind in [Kind::Native, Kind::Terminal] {
        evidence.push(Directory::read(&holders::directory(kind)?, kind, deadline)?);
    }
    for record in installed_terminals {
        check_time(deadline)?;
        let closed = crate::terminal::holder::cleanup::confirmed_owner(&record, &proof)?;
        for directory in &mut evidence {
            directory.allow(&record, false);
            if closed { directory.allow(&record, true); }
        }
    }
    // Each mutex hold reads at most 128 owners. Walk through the empty page,
    // including zero-event, unnamed and unknown-scope actors. No history cap
    // or semantic-event cursor can stand in for completeness.
    let mut after = None;
    loop {
        check_time(deadline)?;
        let page = lock_registry(&registry, deadline)?
            .native_actor_ownerships(after, 128)?;
        if page.is_empty() { break; }
        for owner in page {
            check_time(deadline)?;
            if after.is_some_and(|previous| owner.actor() <= previous) {
                return Err("The durable ownership census did not advance".into());
            }
            after = Some(owner.actor());
            let retired = lock_registry(&registry, deadline)?
                .native_actor_retirement(owner.actor())?;
            if let Some(retired) = retired {
                let same_scope = match (owner.scope(), retired.scope()) {
                    (Ok(a), Ok(b)) => a.provider == b.provider
                        && a.canonical_home == b.canonical_home
                        && (a.device, a.inode) == (b.device, b.inode),
                    (Err(a), Err(b)) => a == b,
                    _ => false,
                };
                if retired.actor() != owner.actor() || retired.owner() != owner.owner()
                    || retired.source().id != owner.source().id || !same_scope
                {
                    return Err("An actor retirement does not match its durable owner".into());
                }
                if owner.owner().boot == proof.boot_id
                    && (owner.owner().holder.is_current() || owner.owner().root.is_current())
                {
                    return Err("A retired actor's exact process is still present; deletion refused".into());
                }
                // Exact finally-retired Unknown scope can be excluded from
                // live access. It supplies no physical/session purge authority.
                for directory in &mut evidence { directory.allow(owner.owner(), true); }
            } else {
                let scope = owner.scope().map_err(|_| {
                    "An unretired actor has unknown account ownership; explicitly close it before deleting account files"
                })?;
                if scope.provider != owner.source().launch.provider {
                    return Err("The durable actor account provider is inconsistent".into());
                }
                if overlaps(home, account, scope, namespaces)? {
                    return Err("This account still has an actor without proved final retirement; close it before deleting files".into());
                }
                // Only an authenticated exact foreign owner can explain its
                // live discovery record. Unknown/unregistered files still fail.
                for directory in &mut evidence { directory.allow(owner.owner(), false); }
            }
        }
    }
    for directory in &evidence { directory.finish(deadline)?; }
    crate::host::client::ensure_account_homes_unused(host, vec![account.clone()])
        .map_err(|e| e.message)?;
    check_time(deadline)
}

fn overlaps(home: &Home, account: &Account, scope: &LaunchAccountScope, namespaces: &BTreeSet<String>) -> Result<bool, String> {
    // Canonical prefixes remain conservative for renamed/replaced homes; the
    // inode comparison catches aliases even when their names differ.
    let same_inode = home.pin.as_ref().map(|pin| pin.metadata())
        .transpose().map_err(|e| e.to_string())?
        .is_some_and(|metadata| (metadata.dev(), metadata.ino()) == (scope.device, scope.inode));
    let historical_registration = home.pin.is_none()
        && (scope.registered_id.as_ref() == Some(&account.id()) || scope.purge_ids.contains(&account.id()));
    Ok(same_inode || historical_registration
        || namespaces.contains(&crate::chat::annotations::storage::account_key(scope))
        || scope.canonical_home.starts_with(&home.canonical)
        || home.canonical.starts_with(&scope.canonical_home))
}

fn check_time(deadline: Instant) -> Result<(), String> {
    if Instant::now() >= deadline {
        Err("Account ownership census timed out; no files were authorized for deletion".into())
    } else { Ok(()) }
}

fn lock_registry<'a>(
    registry: &'a std::sync::Mutex<crate::host::model::Registry>,
    deadline: Instant,
) -> Result<std::sync::MutexGuard<'a, crate::host::model::Registry>, String> {
    loop {
        check_time(deadline)?;
        match registry.try_lock() {
            Ok(guard) => return Ok(guard),
            Err(std::sync::TryLockError::Poisoned(error)) => return Ok(error.into_inner()),
            Err(std::sync::TryLockError::WouldBlock) => std::thread::sleep(Duration::from_millis(2)),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Stamp { device: u64, inode: u64, mode: u32, bytes: u64, modified: (i64, i64), changed: (i64, i64) }
impl From<Metadata> for Stamp {
    fn from(value: Metadata) -> Self {
        Self { device: value.dev(), inode: value.ino(), mode: value.mode(), bytes: value.len(),
            modified: (value.mtime(), value.mtime_nsec()), changed: (value.ctime(), value.ctime_nsec()) }
    }
}
struct Entry { name: std::ffi::OsString, stamp: Stamp, record: Option<Record>, closed: bool, allowed: bool }
struct Directory { path: PathBuf, kind: Kind, pin: Option<File>, stamp: Option<Stamp>, entries: Vec<Entry> }

impl Directory {
    fn read(path: &Path, kind: Kind, deadline: Instant) -> Result<Self, String> {
        crate::secure_file::reject_symlink_components(path).map_err(|e| e.to_string())?;
        let pin = match OpenOptions::new().read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW).open(path)
        {
            Ok(pin) => pin,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self { path: path.into(), kind, pin: None, stamp: None, entries: Vec::new() });
            }
            Err(error) => return Err(format!("Cannot census holder directory: {error}")),
        };
        let metadata = pin.metadata().map_err(|e| e.to_string())?;
        if metadata.uid() != crate::daemon::current_uid() || metadata.mode() & 0o077 != 0 {
            return Err("Holder discovery directory is not private; deletion refused".into());
        }
        let stamp = Stamp::from(metadata);
        let pinned_path = PathBuf::from(format!("/proc/self/fd/{}", pin.as_raw_fd()));
        let mut entries = Vec::new();
        let mut sockets = BTreeSet::new();
        let mut records = BTreeSet::new();
        let mut record_bytes = 0u64;
        for entry in std::fs::read_dir(&pinned_path).map_err(|e| e.to_string())? {
            check_time(deadline)?;
            if entries.len() >= MAX_ENTRIES { return Err("Holder census exceeds its entry bound".into()); }
            let entry = entry.map_err(|e| e.to_string())?;
            let name = entry.file_name();
            let metadata = std::fs::symlink_metadata(entry.path()).map_err(|e| e.to_string())?;
            if metadata.uid() != crate::daemon::current_uid() || metadata.mode() & 0o077 != 0 || metadata.file_type().is_symlink() {
                return Err("Unsafe holder discovery entry; deletion refused".into());
            }
            let text = name.to_str().ok_or("Unrecognized holder discovery filename")?;
            let closed = text.ends_with(".closed");
            let record = if metadata.file_type().is_socket() {
                let id = text.parse::<uuid::Uuid>().map_err(|_| "Unrecognized holder socket")?;
                if id.to_string() != text { return Err("Noncanonical holder socket name".into()); }
                sockets.insert(text.to_owned());
                None
            } else if metadata.is_file() && metadata.nlink() == 1 && (closed || text.ends_with(".json")) {
                record_bytes = record_bytes.checked_add(metadata.len()).ok_or("Holder census byte bound overflowed")?;
                if record_bytes > 16 * 1024 * 1024 { return Err("Holder census exceeds its total metadata byte bound".into()); }
                let mut file = OpenOptions::new().read(true)
                    .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
                    .open(entry.path()).map_err(|e| e.to_string())?;
                if Stamp::from(file.metadata().map_err(|e| e.to_string())?) != Stamp::from(metadata.clone()) || metadata.len() > MAX_RECORD {
                    return Err("Holder discovery record changed or exceeds its bound".into());
                }
                let mut bytes = Vec::new();
                (&mut file).take(MAX_RECORD + 1).read_to_end(&mut bytes).map_err(|e| e.to_string())?;
                if bytes.len() as u64 > MAX_RECORD { return Err("Holder record grew beyond its bound".into()); }
                let record: Record = if closed {
                    #[derive(serde::Deserialize)] struct Closed { record: Record }
                    serde_json::from_slice::<Closed>(&bytes).map_err(|e| e.to_string())?.record
                } else { serde_json::from_slice(&bytes).map_err(|e| e.to_string())? };
                let expected = if closed { format!("{}-{}-{}.closed", record.terminal, record.holder.pid, record.holder.start) }
                    else { format!("{}.json", record.terminal) };
                if text != expected || record.kind != kind || !matches!(record.version, 3 | 4)
                    || !holders::record_application_matches(&record)
                { return Err("Holder record does not match its exact private namespace".into()); }
                if !closed { records.insert(record.terminal.to_string()); }
                Some(record)
            } else {
                // Includes partial .cleanup-* publication: wait for its owner
                // to finish instead of treating unreadable work as absence.
                return Err("Unrecognized or pending holder evidence; resolve it before deleting account files".into());
            };
            entries.push(Entry { name, stamp: Stamp::from(metadata), record, closed, allowed: false });
        }
        if sockets != records { return Err("Holder socket/record census is incomplete; deletion refused".into()); }
        let result = Self { path: path.into(), kind, pin: Some(pin), stamp: Some(stamp), entries };
        result.revalidate()?;
        Ok(result)
    }

    fn allow(&mut self, record: &Record, retired: bool) {
        if record.kind != self.kind { return; }
        for entry in &mut self.entries {
            if entry.closed == retired && entry.record.as_ref() == Some(record) { entry.allowed = true; }
        }
    }

    fn finish(&self, deadline: Instant) -> Result<(), String> {
        for entry in &self.entries {
            check_time(deadline)?;
            if let Some(record) = &entry.record {
                if !entry.allowed { return Err("A holder has no exact accounted ownership/retirement; deletion refused".into()); }
                if !entry.closed {
                    if record.boot != crate::daemon::read_boot_id().map_err(|e| e.to_string())?
                        || !record.holder.is_current()
                    { return Err("A stale discovery record cannot prove retirement".into()); }
                    let stream = holders::connect_until(&self.path.join(record.terminal.to_string()), deadline)
                        .map_err(|e| format!("Cannot authenticate holder census endpoint: {e}"))?;
                    verify_peer(&stream, record)?;
                }
            }
        }
        self.revalidate()
    }

    fn revalidate(&self) -> Result<(), String> {
        match (&self.pin, &self.stamp) {
            (None, None) => match std::fs::symlink_metadata(&self.path) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
                _ => Err("A holder directory appeared during account deletion census".into()),
            },
            (Some(pin), Some(stamp)) => {
                crate::secure_file::reject_symlink_components(&self.path).map_err(|e| e.to_string())?;
                let current = std::fs::symlink_metadata(&self.path).map_err(|e| e.to_string())?;
                if &Stamp::from(current) != stamp || &Stamp::from(pin.metadata().map_err(|e| e.to_string())?) != stamp {
                    return Err("Holder directory changed during account deletion census".into());
                }
                let pinned_path = PathBuf::from(format!("/proc/self/fd/{}", pin.as_raw_fd()));
                for entry in &self.entries {
                    if Stamp::from(std::fs::symlink_metadata(pinned_path.join(&entry.name)).map_err(|e| e.to_string())?) != entry.stamp {
                        return Err("Holder evidence changed during account deletion census".into());
                    }
                }
                Ok(())
            }
            _ => Err("Incomplete holder census state".into()),
        }
    }
}

fn verify_peer(stream: &UnixStream, record: &Record) -> Result<(), String> {
    // SAFETY: getsockopt writes exactly into the supplied owned scalar buffer.
    let mut credentials: libc::ucred = unsafe { std::mem::zeroed() };
    let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let result = unsafe { libc::getsockopt(stream.as_raw_fd(), libc::SOL_SOCKET, libc::SO_PEERCRED,
        (&mut credentials as *mut libc::ucred).cast(), &mut length) };
    if result != 0 { return Err(std::io::Error::last_os_error().to_string()); }
    if length as usize != std::mem::size_of::<libc::ucred>()
        || credentials.uid != crate::daemon::current_uid() || credentials.pid != record.holder.pid
        || !record.holder.is_current()
    { return Err("Holder endpoint no longer belongs to its exact registered owner".into()); }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::{fs::{PermissionsExt, symlink}, net::UnixListener};

    fn private_directory() -> tempfile::TempDir {
        let directory = tempfile::tempdir_in("/var/tmp").unwrap();
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        directory
    }
    fn record(kind: Kind) -> Record {
        let process = holders::Process::read(std::process::id() as i32).unwrap();
        // JSON keeps this fixture compatible with the optional build metadata
        // field in the shared actor foundation, without inventing capability.
        serde_json::from_value(serde_json::json!({
            "version":3,"kind":kind,"application":crate::instance::APPLICATION_ID,
            "boot":crate::daemon::read_boot_id().unwrap(),
            "terminal":crate::model::TerminalId::new(),"holder":process,"root":process
        })).unwrap()
    }
    fn save(path: &Path, bytes: &[u8]) {
        let mut file = OpenOptions::new().write(true).create_new(true).mode(0o600).open(path).unwrap();
        std::io::Write::write_all(&mut file, bytes).unwrap();
    }
    fn publish(directory: &Path, record: &Record) -> UnixListener {
        record.create(directory).unwrap();
        let socket = directory.join(record.terminal.to_string());
        let listener = UnixListener::bind(&socket).unwrap();
        std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600)).unwrap();
        listener
    }
    fn deadline() -> Instant { Instant::now() + Duration::from_secs(2) }

    #[test]
    fn discovery_requires_exact_accounted_owner_even_with_a_live_pid() {
        let directory = private_directory();
        let owner = record(Kind::Terminal);
        let _listener = publish(directory.path(), &owner);
        let mut census = Directory::read(directory.path(), Kind::Terminal, deadline()).unwrap();
        assert!(census.finish(deadline()).is_err(), "unregistered discovery cannot certify an account");
        let mut wrong = owner.clone(); wrong.holder.start += 1;
        census.allow(&wrong, false);
        assert!(census.finish(deadline()).is_err());
        census.allow(&owner, false);
        census.finish(deadline()).unwrap();
    }

    #[test]
    fn stale_pid_and_socket_absence_never_become_final_retirement() {
        let directory = private_directory();
        let mut owner = record(Kind::Terminal);
        owner.holder.pid = i32::MAX;
        let _listener = publish(directory.path(), &owner);
        let mut census = Directory::read(directory.path(), Kind::Terminal, deadline()).unwrap();
        census.allow(&owner, false);
        assert!(census.finish(deadline()).is_err(), "a dead process is not a retirement receipt");
        std::fs::remove_file(directory.path().join(owner.terminal.to_string())).unwrap();
        assert!(Directory::read(directory.path(), Kind::Terminal, deadline()).is_err());
        assert!(directory.path().join(format!("{}.json",owner.terminal)).exists(), "census never cleans evidence");
    }

    #[test]
    fn unsafe_partial_and_unpaired_discovery_refuse_without_effects() {
        for case in 0..6 {
            let directory = private_directory();
            let path = directory.path().join("unrecognized");
            match case {
                0 => save(&path,b"keep"),
                1 => save(&directory.path().join(format!(".cleanup-{}",uuid::Uuid::new_v4())),b"keep"),
                2 => { symlink("/nonexistent", &path).unwrap(); }
                3 => { let owner=record(Kind::Native); owner.create(directory.path()).unwrap(); }
                4 => { save(&directory.path().join(format!("{}.json",uuid::Uuid::new_v4())),b"{broken"); }
                _ => { std::fs::set_permissions(directory.path(),std::fs::Permissions::from_mode(0o755)).unwrap(); }
            }
            assert!(Directory::read(directory.path(),Kind::Native,deadline()).is_err(),"case {case}");
            assert!(directory.path().exists());
        }
    }

    #[test]
    fn replaced_directory_and_record_cannot_reuse_a_census() {
        let root=private_directory();
        let path=root.path().join("holders");
        std::fs::create_dir(&path).unwrap();
        std::fs::set_permissions(&path,std::fs::Permissions::from_mode(0o700)).unwrap();
        let owner=record(Kind::Terminal);
        let _listener=publish(&path,&owner);
        let census=Directory::read(&path,Kind::Terminal,deadline()).unwrap();
        let record_path=path.join(format!("{}.json",owner.terminal));
        let bytes=std::fs::read(&record_path).unwrap();
        std::fs::remove_file(&record_path).unwrap();save(&record_path,&bytes);
        assert!(census.revalidate().is_err(),"identical bytes on a new inode cannot inherit authority");
        let census=Directory::read(&path,Kind::Terminal,deadline()).unwrap();
        std::fs::rename(&path,root.path().join("old")).unwrap();
        std::fs::create_dir(&path).unwrap();
        assert!(census.revalidate().is_err(),"the retained FD cannot authorize the replacement path");
    }

    #[test]
    fn account_permit_requires_complete_roster_and_original_reservation() {
        const CHILD: &str="CAGENTS_DELETION_PERMIT_FIXTURE";
        const NAME: &str="host::account_lifecycle::deletion::tests::account_permit_requires_complete_roster_and_original_reservation";
        if std::env::var_os(CHILD).is_none() {
            let directory=private_directory();
            for name in ["home","config","data","cache","runtime"] {
                std::fs::create_dir(directory.path().join(name)).unwrap();
            }
            let output=std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact",NAME,"--nocapture"]).env_clear().env(CHILD,directory.path())
                .env("HOME",directory.path().join("home"))
                .env("XDG_CONFIG_HOME",directory.path().join("config"))
                .env("XDG_DATA_HOME",directory.path().join("data"))
                .env("XDG_CACHE_HOME",directory.path().join("cache"))
                .env("XDG_RUNTIME_DIR",directory.path().join("runtime"))
                .env("PATH","/usr/bin:/bin").output().unwrap();
            assert!(output.status.success(),"{}\n{}",String::from_utf8_lossy(&output.stdout),String::from_utf8_lossy(&output.stderr));
            return;
        }
        use crate::{agent::Provider,account::{home::Homes,registry::{Registered,Store}},host::{lock::HostMutex,state::HostState},model::{HostSessionProof,HostSessionId,HostIncarnation}};
        let root=PathBuf::from(std::env::var_os(CHILD).unwrap());
        let homes=Homes::installation().unwrap();let store=Store::installation().unwrap();
        let (key,path)=homes.create(Provider::Claude,None).unwrap();
        store.update(|registry|registry.register(Registered {key,provider:Provider::Claude,linked_home:None,label:None,email:None,organization:None,added_at:0},&[]).unwrap()).unwrap();
        let account=Account::managed(Provider::Claude,key,path.clone());
        let proof=HostSessionProof {host_session_id:HostSessionId::new("permit-fixture"),boot_id:crate::daemon::read_boot_id().unwrap(),host_incarnation:HostIncarnation::new("permit-fixture")};
        let host=std::sync::Arc::new(HostMutex::new(HostState::new(proof)));
        host.lock().unwrap().install_model(crate::host::model::Registry::at(&root.join("model.sqlite3")).unwrap());
        let reservation=Reservation::install(&host,&[account.clone()]).unwrap();
        let scope=LaunchAccountScope::resolve(Provider::Claude,&root,|key|(key=="CLAUDE_CONFIG_DIR").then(||path.clone().into_os_string())).unwrap();
        let permit=reservation.deletion_permit(&host,&account).unwrap();
        permit.authorize_scope(&scope).unwrap();
        assert!(permit.authorize_registered(&account.id()).is_err(),"present scope cannot become historical-index authority");
        let mut wrong=scope.clone();wrong.inode+=1;
        assert!(permit.authorize_scope(&wrong).is_err());

        let registry=host.lock().unwrap().execution_registry.clone().unwrap();
        // The final owner sorts after a full 128-row page. A one-page census
        // would miss it. Earlier owners have proved exact final retirement;
        // this fixture tests the trusted registry consumer, not helper teardown.
        for index in 1..=129u128 {
            let actor=uuid::Uuid::from_u128(index);
            let mut owner=record(Kind::Native);owner.holder.pid=i32::MAX;owner.root.pid=i32::MAX;
            let source=crate::chat::Session {id:owner.terminal,launch:crate::chat::Launch {provider:Provider::Claude,recipe:crate::model::LaunchRecipe {program:"claude".into(),arguments:Vec::new(),working_directory:root.clone(),environment:Default::default(),unset_environment:Default::default()},session_id:None},proof:Some(host.lock().unwrap().proof().clone()),terminal:None,owned_codex_listener:None,native_continuity:crate::chat::NativeContinuity::RetainedClaudeActor {protocol:1}};
            let mut registry=registry.lock().unwrap();
            registry.register_native_actor_ownership(actor,&owner,&source,Err("fixture unknown scope")).unwrap();
            if index<129 {registry.commit_native_actor_retirement(actor,&owner,&source,Err("fixture unknown scope"),None).unwrap();}
        }
        assert!(permit.verify().is_err(),"the last unknown owner is unretired even with no PID/socket");
        let last=registry.lock().unwrap().native_actor_ownerships(Some(uuid::Uuid::from_u128(128)),1).unwrap().remove(0);
        registry.lock().unwrap().commit_native_actor_retirement(last.actor(),last.owner(),last.source(),last.scope(),None).unwrap();
        permit.authorize_scope(&scope).unwrap();
        // A missing registration may refer to an older physical namespace.
        // Reserve opaque keys before purge, and reject a live writer even when
        // its current account label/path differs from the selected missing one.
        let (missing_key,missing_path)=homes.create(Provider::Claude,None).unwrap();
        let (foreign_key,foreign_path)=homes.create(Provider::Claude,None).unwrap();
        for key in [missing_key,foreign_key] {
            store.update(|registry|registry.register(Registered {key,provider:Provider::Claude,linked_home:None,label:None,email:None,organization:None,added_at:0},&[]).unwrap()).unwrap();
        }
        std::fs::remove_dir_all(&missing_path).unwrap();
        let missing_account=Account::managed(Provider::Claude,missing_key,missing_path);
        let missing_reservation=Reservation::install(&host,&[missing_account.clone()]).unwrap();
        let missing_permit=missing_reservation.deletion_permit(&host,&missing_account).unwrap();
        missing_permit.authorize_registered(&missing_account.id()).unwrap();
        assert!(missing_permit.authorize_registered(&account.id()).is_err());
        let foreign_scope=LaunchAccountScope::resolve(Provider::Claude,&root,|key|(key=="CLAUDE_CONFIG_DIR").then(||foreign_path.clone().into_os_string())).unwrap();
        let mut foreign_owner=record(Kind::Native);foreign_owner.holder.pid=i32::MAX;foreign_owner.root.pid=i32::MAX;
        let mut foreign_source=last.source().clone();foreign_source.id=foreign_owner.terminal;
        registry.lock().unwrap().register_native_actor_ownership(uuid::Uuid::from_u128(130),&foreign_owner,&foreign_source,Ok(&foreign_scope)).unwrap();
        let key=crate::chat::annotations::storage::account_key(&foreign_scope);
        assert!(missing_permit.reserve_registered_namespaces(&missing_account.id(),&[key]).is_err(),"an unrelated label cannot hide a historical namespace writer");
        assert!(super::super::ensure_captured_scope(&host,Some(&foreign_scope)).is_err(),"failed census retains its namespace guard until reservation drop");
        drop(missing_permit);drop(missing_reservation);
        super::super::ensure_captured_scope(&host,Some(&foreign_scope)).unwrap();
        // A stale registration/path and a released lease independently revoke
        // the borrowed permit; no deletion has occurred in this fixture.
        let retained=root.join("retained-home");std::fs::rename(&path,&retained).unwrap();
        std::fs::create_dir(&path).unwrap();std::fs::write(path.join("keep"),b"replacement").unwrap();
        assert!(permit.authorize_scope(&scope).is_err());
        assert_eq!(std::fs::read(path.join("keep")).unwrap(),b"replacement");
        std::fs::remove_file(path.join("keep")).unwrap();std::fs::remove_dir(&path).unwrap();std::fs::rename(retained,&path).unwrap();
        reservation.map.lock().unwrap().remove(&reservation.token);
        assert!(permit.verify().is_err(),"an invalidated reservation cannot retain deletion authority");
    }
}
