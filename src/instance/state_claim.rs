//! Exclusion on an instance's saved state, not only on its runtime directory.
//!
//! Every lock a daemon or window takes lives in the runtime directory that
//! `XDG_RUNTIME_DIR` names. Two sessions can name different runtime
//! directories while sharing one data directory: a nested compositor session
//! gives itself a private runtime directory but keeps the person's `HOME`.
//! A window started there found no daemon in its runtime directory, started
//! one, and both daemons then restored, admitted and saved the same chats.
//!
//! A claim closes that gap. It is a lock file in the data directory that
//! records the runtime lock of whoever last held it: the daemon's lease or a
//! window's slot lock. Whoever takes the claim must already hold its own
//! runtime lock, and finds the state free only when
//!
//! - nobody holds the claim's lock, and
//! - the runtime lock the claim records is its own, or nobody holds it.
//!
//! The second rule is what lets the claim pass through an update without
//! handing over another descriptor. The updater keeps the outgoing daemon's
//! runtime lease, and maintenance holds it while it reads or converts the
//! store, so while either works the recorded runtime lock is held and a daemon
//! from another runtime directory is refused, though nobody holds the claim
//! itself. The successor daemon inherits that lease, so the claim it finds
//! names its own runtime lock and it takes the claim at once.
//!
//! The updater takes the lease only after the outgoing daemon has let go of
//! it, polling every 20 ms. So before taking over a claim that records
//! another runtime lock, a claimant watches that lock for [`HANDOFF`] and is
//! refused if anybody takes it meanwhile.
//!
//! Whether a runtime lock is held is read from `/proc/locks` on Linux, so a
//! check from another session never takes a lock that session's daemon,
//! updater or windows may be taking at that moment. That list leaves out
//! locks held from another PID namespace; only the second rule, which
//! matters during an update, depends on it.

use super::*;
use serde::{Deserialize, Serialize};
use std::os::unix::fs::FileExt;

#[cfg(any(target_os = "macos", test))]
mod saved_owner;

/// How a claim is recorded. Unknown fields are ignored so that a later version
/// can add some without an earlier one reading its claim as missing.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
struct Record {
    format: u64,
    /// The holder's runtime lock: whoever holds it acts for this claim.
    guard: PathBuf,
    /// The runtime directory the holder serves, for people to read.
    runtime: PathBuf,
    pid: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    host_session: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    boot: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    hostname: Option<String>,
}

const FORMAT: u64 = 1;
// The runtime path appears in both `guard` and `runtime`. Budget for
// two full Linux PATH_MAX values with JSON escaping, plus the header.
const MAX_RECORD_BYTES: u64 = 64 * 1024;
const POLL: Duration = Duration::from_millis(20);
/// How long a claimant watches the runtime lock a claim records before it
/// takes over from another runtime directory: many times the interval at
/// which an updater tries to take the lease of a daemon that is exiting.
const HANDOFF: Duration = Duration::from_millis(250);
/// How long a daemon waits for a predecessor in its own runtime directory to
/// finish exiting. A daemon releases its claim before its lease, so this is
/// only reached when a process dies holding both and its exit is slow.
const DAEMON_PATIENCE: Duration = Duration::from_secs(5);
/// The same, for a window that relaunches in its own runtime directory.
const WINDOW_PATIENCE: Duration = Duration::from_secs(2);
/// Under the data directory: who holds the saved data now, not saved data.
pub(crate) const CLAIMS_DIRECTORY: &str = "claims";
const DAEMON_CLAIM: &str = "daemon.lock";

/// What wants a claim.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Claimant {
    Daemon,
    /// Reading or converting the stopped daemon's store.
    Maintenance,
    Window,
}

/// The state is in use by a daemon or window that another runtime directory
/// serves.
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub struct StateInUse {
    claimant: Claimant,
    pid: Option<u32>,
    runtime: Option<PathBuf>,
    foreign_host: Option<String>,
    unreadable: bool,
    storage_unavailable: bool,
}

impl StateInUse {
    pub(super) fn can_try_another_slot(&self) -> bool {
        !self.storage_unavailable
    }

    pub fn pid(&self) -> Option<u32> {
        self.pid
    }

    pub fn runtime(&self) -> Option<&Path> {
        self.runtime.as_deref()
    }
}

impl std::fmt::Display for StateInUse {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let resource = match self.claimant {
            Claimant::Daemon | Claimant::Maintenance => "this cagents data",
            Claimant::Window => "this cagents window's saved layout",
        };
        if self.unreadable {
            return write!(
                formatter,
                "The ownership record for {resource} is unreadable. cagents could not verify that the saved data is free."
            );
        }
        if let Some(host) = &self.foreign_host {
            return write!(
                formatter,
                "Another cagents process on {host} is using {resource}. Close it on that machine before opening the same saved data here."
            );
        }
        let holder = match self.pid {
            Some(pid) => format!("process {pid}"),
            None => "it".to_owned(),
        };
        let runtime = self
            .runtime
            .as_ref()
            .map(|runtime| format!(", from the runtime directory {}", runtime.display()))
            .unwrap_or_default();
        let process = self
            .pid
            .map(|pid| format!(" (process {pid})"))
            .unwrap_or_default();
        match self.claimant {
            Claimant::Daemon => {
                write!(
                    formatter,
                    "Another cagents daemon{process} is already using this cagents data{runtime}. \
                     Two daemons on the same data overwrite each other's chats, so cagents did \
                     not start another one here."
                )?;
                // The runtime directory is the session's XDG_RUNTIME_DIR
                // with the instance's own directory below it. Pointing a
                // launch here at it would also move its display, so the
                // session comes first.
                match self.runtime.as_deref().and_then(Path::parent) {
                    Some(session) => write!(
                        formatter,
                        " Use cagents in the session that started it (XDG_RUNTIME_DIR={}), or \
                         stop that daemon there to use cagents here.",
                        session.display()
                    )?,
                    None => write!(formatter, " Stop {holder} to use cagents here.")?,
                }
                if self.pid.is_none() {
                    // Its process is gone: most likely it is being updated.
                    write!(formatter, " If it is updating, try again in a moment.")?;
                }
                Ok(())
            }
            Claimant::Maintenance => write!(
                formatter,
                "Another cagents daemon{process} is using this cagents data{runtime}, so its \
                 saved data can't be changed now. Stop {holder} first."
            ),
            Claimant::Window => write!(
                formatter,
                "This cagents window is already open in another session{process}{runtime}. Two \
                 copies of one window overwrite each other's saved layout, so this one did not \
                 open. Use the open window, or close it first."
            ),
        }
    }
}

/// Whether `error` is, or was caused by, a [`StateInUse`] refusal.
pub fn state_in_use<'a>(error: &'a (dyn std::error::Error + 'static)) -> Option<&'a StateInUse> {
    let mut current = Some(error);
    while let Some(error) = current {
        if let Some(refusal) = error.downcast_ref::<StateInUse>() {
            return Some(refusal);
        }
        if let Some(io) = error.downcast_ref::<io::Error>()
            && let Some(inner) = io.get_ref()
            && let Some(refusal) = inner.downcast_ref::<StateInUse>()
        {
            return Some(refusal);
        }
        current = error.source();
    }
    None
}

/// A held claim. It is released when dropped; drop it before the runtime lock
/// it records, so that nobody finds the claim held by a runtime lock that is
/// already free.
#[derive(Debug)]
pub struct StateClaim {
    _lock: OwnedLock,
}

/// This instance's claims, beside its saved state.
pub fn claims_directory() -> io::Result<PathBuf> {
    let dirs = project_dirs().ok_or_else(|| io::Error::other("data directory unavailable"))?;
    Ok(dirs.data_dir().join(CLAIMS_DIRECTORY))
}

/// A different runtime may have used the same shared data while no
/// process held the old runtime's lock. Its last claimant must still be the
/// acknowledged source; the caller holds the Darwin publication fence.
#[cfg(target_os = "macos")]
pub(crate) fn last_daemon_pid(
    lock: &Path,
    proof: &crate::model::HostSessionProof,
) -> io::Result<Option<u32>> {
    let claim = claims_directory()?.join(DAEMON_CLAIM);
    let guard = canonical_guard(lock);
    Ok(saved_owner::read_path(&claim)?
        .filter(|record| names_source(record, &guard, proof, None))
        .map(|record| record.pid))
}

#[cfg(target_os = "macos")]
fn names_source(
    record: &Record,
    guard: &Path,
    proof: &crate::model::HostSessionProof,
    pid: Option<u32>,
) -> bool {
    record.guard == guard
        && record.host_session.as_deref() == Some(proof.host_session_id.as_str())
        && record.boot.as_deref() == Some(proof.boot_id.as_str())
        && pid.is_none_or(|pid| record.pid == pid)
}

/// The acknowledged Ready successor may replace only its original source's
/// saved-data claim. Compare under the exclusive lock before recording a
/// successor so an intervening writer cannot race the coordinator's snapshot.
#[cfg(all(target_os = "macos", feature = "host"))]
pub(crate) fn claim_daemon_state_from(
    lock: &Path,
    host_session: &str,
    source: &crate::model::HostSessionProof,
    source_pid: Option<u32>,
) -> io::Result<StateClaim> {
    let claims = claims_directory()?;
    prepare_claim_directory(&claims, Claimant::Daemon)?;
    let ours = record(lock, Some(host_session))?;
    saved_owner::acquire(
        &claims.join(DAEMON_CLAIM),
        &ours,
        DAEMON_PATIENCE,
        |previous| names_source(previous, &ours.guard, source, source_pid),
    )
}

/// Claim this instance's daemon state for the daemon that holds the runtime
/// lease `lock`. The caller must hold that lease.
pub fn claim_daemon_state(lock: &Path, host_session: &str) -> io::Result<StateClaim> {
    claim_daemon_state_in(
        &claims_directory()?,
        lock,
        Some(host_session),
        Claimant::Daemon,
    )
}

/// Claim this instance's daemon state for maintenance of the stopped daemon's
/// store, which holds its runtime lease `lock`.
pub fn claim_store_maintenance(lock: &Path) -> io::Result<StateClaim> {
    #[cfg(target_os = "macos")]
    crate::update::require_no_activation().map_err(io::Error::other)?;
    claim_daemon_state_in(&claims_directory()?, lock, None, Claimant::Maintenance)
}

pub(crate) fn claim_daemon_state_in(
    claims: &Path,
    lock: &Path,
    host_session: Option<&str>,
    claimant: Claimant,
) -> io::Result<StateClaim> {
    prepare_claim_directory(claims, claimant)?;
    acquire(
        &claims.join(DAEMON_CLAIM),
        record(lock, host_session)?,
        claimant,
        DAEMON_PATIENCE,
    )
}

/// The daemon that another runtime directory serves this instance's state
/// from, if one does, for a launcher deciding whether to start one here.
/// Recheck invalid bytes under a nonblocking shared lock so a claim being
/// written does not masquerade as a permanently unreadable one.
pub fn daemon_served_elsewhere(lock: &Path) -> io::Result<Option<StateInUse>> {
    served_elsewhere(&claims_directory()?.join(DAEMON_CLAIM), lock)
}

/// Called under the Darwin publication and application launch fences. A daemon
/// or window already admitted in another runtime still holds its state claim.
#[cfg(target_os = "macos")]
pub(crate) fn require_idle_for_installation() -> io::Result<()> {
    let claims = claims_directory()?;
    crate::secure_file::reject_symlink_components(&claims)?;
    let entries = match fs::read_dir(claims) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_str().unwrap_or_default();
        let daemon = name == DAEMON_CLAIM;
        if !daemon && !(name.starts_with("window.") && name.ends_with(".lock")) {
            continue;
        }
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
            .open(entry.path())?;
        let meta = file.metadata()?;
        if !meta.is_file()
            || meta.uid() != current_uid()
            || meta.mode() & 0o077 != 0
            || meta.nlink() != 1
        {
            return Err(io::Error::other(
                "could not verify ownership of saved cagents data",
            ));
        }
        match owned_file_lock(file, libc::LOCK_EX | libc::LOCK_NB) {
            Ok(_) => (),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                let message = if daemon {
                    "the cagents daemon is running and using saved work; stop it before installing or removing cagents"
                } else {
                    "a cagents window is using saved work; close it before installing or removing cagents"
                };
                return Err(io::Error::other(message));
            }
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

pub(crate) fn served_elsewhere(claim: &Path, lock: &Path) -> io::Result<Option<StateInUse>> {
    let recorded = match read_path(claim) {
        Ok(Some(recorded)) => recorded,
        Ok(None) => return Ok(None),
        Err(_) => return verified_unreadable(claim),
    };
    let boot = local_boot();
    let foreign = other_boot(&recorded, boot.as_deref());
    let used = if foreign {
        // A private runtime path and PID on another boot say nothing about a
        // process here. The shared claim file is the cross-machine evidence.
        match private_lock_is_held(claim) {
            Ok(held) => {
                held || recorded.guard != canonical_guard(lock) && guard_held(&recorded.guard)
            }
            Err(_) => return verified_unreadable(claim),
        }
    } else {
        recorded.guard != canonical_guard(lock) && guard_held(&recorded.guard)
    };
    Ok(used.then(|| claim_refusal(claim, Claimant::Daemon, Some(&recorded), foreign)))
}

/// Claim one window's saved state for the window that holds its slot lock
/// `slot_lock`. The caller must hold that lock.
pub(crate) fn claim_window_state_in(
    claims: &Path,
    slot: &str,
    slot_lock: &Path,
) -> io::Result<StateClaim> {
    validate_window_slot(slot)?;
    prepare_claim_directory(claims, Claimant::Window)?;
    acquire(
        &claims.join(format!("window.{slot}.lock")),
        record(slot_lock, None)?,
        Claimant::Window,
        WINDOW_PATIENCE,
    )
}

pub(super) fn prepare_claim_directory(path: &Path, claimant: Claimant) -> io::Result<()> {
    crate::secure_file::prepare_private_directory(path).map_err(|error| {
        tracing::debug!(directory = %path.display(), %error, "could not verify the saved-data claim directory");
        storage_unavailable(claimant)
    })
}

fn verified_unreadable(path: &Path) -> io::Result<Option<StateInUse>> {
    let file = match open_claim(path, false) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Ok(Some(unreadable(Claimant::Daemon))),
    };
    let lock = match owned_file_lock(file, libc::LOCK_SH | libc::LOCK_NB) {
        Ok(lock) => lock,
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(None),
        Err(_) => return Ok(Some(unreadable(Claimant::Daemon))),
    };
    match same_named_claim(path, &lock) {
        Ok(false) => Ok(None),
        Ok(true) if read(&lock).is_ok() => Ok(None),
        _ => Ok(Some(unreadable(Claimant::Daemon))),
    }
}

fn unreadable(claimant: Claimant) -> StateInUse {
    let mut reason = refusal(claimant, None, false);
    reason.unreadable = true;
    reason
}

fn storage_unavailable(claimant: Claimant) -> io::Error {
    let mut reason = unreadable(claimant);
    reason.storage_unavailable = true;
    io::Error::other(reason)
}

fn open_claim(path: &Path, write: bool) -> io::Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .write(write)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.uid() != current_uid() || metadata.mode() & 0o077 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "claim is not a private owned regular file",
        ));
    }
    Ok(file)
}

fn same_named_claim(path: &Path, file: &File) -> io::Result<bool> {
    let opened = file.metadata()?;
    match fs::symlink_metadata(path) {
        Ok(named) => Ok(named.dev() == opened.dev() && named.ino() == opened.ino()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

fn record(guard: &Path, host_session: Option<&str>) -> io::Result<Record> {
    let guard = canonical_guard(guard);
    Ok(Record {
        format: FORMAT,
        runtime: guard
            .parent()
            .ok_or_else(|| io::Error::other("a runtime lock has no directory"))?
            .to_owned(),
        guard,
        pid: process::id(),
        host_session: host_session.map(str::to_owned),
        boot: local_boot(),
        hostname: crate::mobile::host_name(),
    })
}

fn local_boot() -> Option<String> {
    crate::daemon::read_boot_id()
        .ok()
        .map(|boot| boot.as_str().to_owned())
}

fn other_boot(record: &Record, local_boot: Option<&str>) -> bool {
    record
        .boot
        .as_deref()
        .zip(local_boot)
        .is_some_and(|(recorded, ours)| recorded != ours)
}

/// The same runtime directory can be spelled several ways; a claim compares
/// the directory's resolved path. A directory that cannot be resolved yet is
/// compared as given.
fn canonical_guard(guard: &Path) -> PathBuf {
    match (guard.parent(), guard.file_name()) {
        (Some(directory), Some(name)) => fs::canonicalize(directory)
            .map(|directory| directory.join(name))
            .unwrap_or_else(|_| guard.to_owned()),
        _ => guard.to_owned(),
    }
}

/// Publish the first claim fully initialized and already locked. Claimants
/// from earlier versions still synchronize on this inode once it is named.
fn create_claim(path: &Path, ours: &Record) -> io::Result<OwnedLock> {
    let directory = path
        .parent()
        .ok_or_else(|| io::Error::other("claim has no directory"))?;
    let mut candidate = tempfile::NamedTempFile::new_in(directory)?;
    candidate
        .as_file()
        .set_permissions(fs::Permissions::from_mode(PRIVATE_FILE_MODE))?;
    write(candidate.as_file_mut(), ours)?;
    let lock = owned_file_lock(
        candidate.as_file().try_clone()?,
        libc::LOCK_EX | libc::LOCK_NB,
    )?;
    candidate
        .persist_noclobber(path)
        .map_err(|error| error.error)?;
    Ok(lock)
}

fn acquire(
    path: &Path,
    ours: Record,
    claimant: Claimant,
    patience: Duration,
) -> io::Result<StateClaim> {
    acquire_with(path, &ours, claimant, patience, true, |file, _, _| {
        write(file, &ours).map_err(|_| storage_unavailable(claimant))
    })
}

fn acquire_with(
    path: &Path,
    ours: &Record,
    claimant: Claimant,
    patience: Duration,
    allow_takeover: bool,
    publish_owner: impl FnOnce(&mut File, Record, &[u8]) -> io::Result<()>,
) -> io::Result<StateClaim> {
    let started = std::time::Instant::now();
    let unreadable = || io::Error::other(unreadable(claimant));
    loop {
        let file = match open_claim(path, true) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                if !allow_takeover {
                    return Err(io::Error::other(
                        "The saved work no longer belongs to the update's acknowledged source",
                    ));
                }
                match create_claim(path, ours) {
                    Ok(lock) => return Ok(StateClaim { _lock: lock }),
                    Err(error)
                        if error.kind() == io::ErrorKind::AlreadyExists
                            && started.elapsed() < patience =>
                    {
                        continue;
                    }
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                        return Err(io::Error::other(refusal(claimant, None, false)));
                    }
                    Err(error) => {
                        tracing::debug!(claim = %path.display(), %error, "could not publish the saved-data claim");
                        return Err(storage_unavailable(claimant));
                    }
                }
            }
            Err(_) => return Err(unreadable()),
        };
        let found = match owned_file_lock(file, libc::LOCK_EX | libc::LOCK_NB) {
            Ok(mut lock) => {
                // A contender can open a file just before another client
                // removes it. The unnamed inode no longer protects the data.
                if !same_named_claim(path, &lock).map_err(|_| unreadable())? {
                    if started.elapsed() >= patience {
                        return Err(io::Error::other(refusal(claimant, None, false)));
                    }
                    continue;
                }
                // The last holder may have released the claim but left the
                // runtime lease in use by an updater or store maintenance.
                let bytes = read_record_bytes(&lock).map_err(|_| unreadable())?;
                let last = decode_record(&bytes).map_err(|_| unreadable())?;
                if allow_takeover && last.guard != ours.guard && taken_within(&last.guard, HANDOFF)
                {
                    let foreign = other_boot(&last, ours.boot.as_deref());
                    return Err(io::Error::other(claim_refusal(
                        path,
                        claimant,
                        Some(&last),
                        foreign,
                    )));
                }
                publish_owner(lock.file_mut(), last, &bytes)?;
                return Ok(StateClaim { _lock: lock });
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => match read_path(path) {
                Ok(found) => found,
                Err(error) if error.kind() == io::ErrorKind::InvalidData => None,
                Err(_) => return Err(unreadable()),
            },
            Err(_) => return Err(unreadable()),
        };
        if let Some(found) = &found {
            let foreign = other_boot(found, ours.boot.as_deref());
            if foreign || found.guard != ours.guard && guard_held(&found.guard) {
                return Err(io::Error::other(claim_refusal(
                    path,
                    claimant,
                    Some(found),
                    foreign,
                )));
            }
        }
        // Held, but by a predecessor in this runtime directory that has let
        // go of its runtime lock and is exiting, by a holder whose runtime
        // lock is free and so is exiting too, or by one still writing its
        // record. Each resolves by itself.
        if started.elapsed() >= patience {
            return Err(io::Error::other(claim_refusal(
                path,
                claimant,
                found.as_ref(),
                false,
            )));
        }
        std::thread::sleep(POLL);
    }
}

fn claim_refusal(
    _path: &Path,
    claimant: Claimant,
    record: Option<&Record>,
    foreign: bool,
) -> StateInUse {
    let reason = refusal(claimant, record, foreign);
    // The native guard still excludes other runtimes during Ready admission.
    // Its PID can be stale, and the Ready PID alone cannot rule out PID reuse.
    #[cfg(target_os = "macos")]
    if claimant != Claimant::Window
        && reason.pid.is_some()
        && let Some(record) = record
        && !saved_owner::confirms_legacy_owner(_path, record)
    {
        return StateInUse {
            pid: None,
            ..reason
        };
    }
    reason
}

fn refusal(claimant: Claimant, record: Option<&Record>, foreign: bool) -> StateInUse {
    StateInUse {
        claimant,
        // During an update the recorded daemon has exited and the updater
        // holds its lease, so naming that process would send people after one
        // that isn't there.
        pid: record
            .filter(|_| !foreign)
            .map(|record| record.pid)
            .filter(|&pid| process_exists(pid)),
        runtime: record
            .filter(|_| !foreign)
            .map(|record| record.runtime.clone()),
        foreign_host: foreign.then(|| {
            record
                .and_then(|record| record.hostname.clone())
                .unwrap_or_else(|| "another machine".to_owned())
        }),
        unreadable: false,
        storage_unavailable: false,
    }
}

/// Whether `pid` names a process, as far as this PID namespace can see. One
/// that can't be signalled for lack of permission still exists.
fn process_exists(pid: u32) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return false;
    };
    // SAFETY: signal 0 checks for the process without signalling it.
    pid > 0
        && (unsafe { libc::kill(pid, 0) } == 0
            || io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH))
}

/// Whether somebody holds the runtime lock at `guard`, or takes it before
/// `within` has passed.
fn taken_within(guard: &Path, within: Duration) -> bool {
    let started = std::time::Instant::now();
    loop {
        if guard_held(guard) {
            return true;
        }
        if started.elapsed() >= within {
            return false;
        }
        std::thread::sleep(POLL);
    }
}

/// Whether a recorded runtime lock remains held or cannot safely be inspected.
/// A missing name can be released, including when its runtime directory ended.
fn guard_held(path: &Path) -> bool {
    let lock = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(path)
    {
        Ok(lock) => lock,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return false,
        Err(_) => return true,
    };
    let Ok(metadata) = lock.metadata() else {
        return true;
    };
    if !metadata.is_file() || metadata.uid() != current_uid() {
        return true;
    }
    #[cfg(target_os = "linux")]
    match listed_flock(&lock, &metadata) {
        Listed::Held => return true,
        Listed::Free => return false,
        Listed::Unknown => {}
    }
    // Owners take an exclusive lease. Probe it with a shared read lock:
    // a read-only descriptor cannot request an exclusive lock on NFS.
    private_lock_is_held(path).unwrap_or(true)
}

#[cfg(target_os = "linux")]
#[derive(Debug, PartialEq, Eq)]
enum Listed {
    Held,
    Free,
    /// The list can't be read, names only a file on another device with this
    /// inode number, which may be this one under another device number, or
    /// leaves out a file it may list under another device and inode.
    Unknown,
}

/// Whether `/proc/locks` lists a flock held on `lock`, which `metadata`
/// describes.
#[cfg(target_os = "linux")]
fn listed_flock(lock: &File, metadata: &fs::Metadata) -> Listed {
    let Ok(locks) = fs::read_to_string("/proc/locks") else {
        return Listed::Unknown;
    };
    match parse_locks(&locks, metadata.dev(), metadata.ino()) {
        // The list names a file by the device and inode of the filesystem
        // underneath, which overlayfs, for one, reports differently. A
        // runtime directory is normally tmpfs, which reports the same ones.
        Listed::Free if !on_tmpfs(lock) => Listed::Unknown,
        listed => listed,
    }
}

#[cfg(target_os = "linux")]
fn on_tmpfs(file: &File) -> bool {
    const TMPFS_MAGIC: i64 = 0x0102_1994;
    let mut status = std::mem::MaybeUninit::<libc::statfs>::uninit();
    // SAFETY: fstatfs fills the buffer for a descriptor this function borrows.
    if unsafe { libc::fstatfs(file.as_raw_fd(), status.as_mut_ptr()) } != 0 {
        return false;
    }
    // SAFETY: fstatfs succeeded, so it filled the buffer.
    #[allow(clippy::unnecessary_cast)]
    let kind = unsafe { status.assume_init() }.f_type as i64;
    kind == TMPFS_MAGIC
}

/// What `locks`, in the format of `/proc/locks`, says about a flock on the
/// file with `device` and `inode`.
#[cfg(target_os = "linux")]
fn parse_locks(locks: &str, device: u64, inode: u64) -> Listed {
    // The kernel prints a device's major and minor numbers in hex.
    let major = ((device >> 8) & 0xfff) | ((device >> 32) & !0xfff);
    let minor = (device & 0xff) | ((device >> 12) & !0xff);
    let device = format!("{major:02x}:{minor:02x}");
    let inode = inode.to_string();
    let mut listed = Listed::Free;
    for line in locks.lines() {
        // "1: FLOCK  ADVISORY  WRITE 1234 00:1a:5678 0 EOF"; a process waiting
        // for the lock is listed as "1: -> FLOCK ...", and holds nothing.
        let mut fields = line.split_whitespace().skip(1);
        if fields.next() != Some("FLOCK") {
            continue;
        }
        let Some((on, number)) = fields.nth(3).and_then(|file| file.rsplit_once(':')) else {
            continue;
        };
        if number == inode {
            if on == device {
                return Listed::Held;
            }
            listed = Listed::Unknown;
        }
    }
    listed
}

fn read(file: &File) -> io::Result<Record> {
    decode_record(&read_record_bytes(file)?)
}

fn read_record_bytes(file: &File) -> io::Result<Vec<u8>> {
    let length = file.metadata()?.len();
    let invalid = || io::Error::new(io::ErrorKind::InvalidData, "invalid saved-data claim");
    if length == 0 || length > MAX_RECORD_BYTES {
        return Err(invalid());
    }
    let mut bytes = vec![0; length as usize];
    file.read_exact_at(&mut bytes, 0).map_err(|error| {
        if error.kind() == io::ErrorKind::UnexpectedEof {
            invalid()
        } else {
            error
        }
    })?;
    Ok(bytes)
}

fn decode_record(bytes: &[u8]) -> io::Result<Record> {
    let invalid = || io::Error::new(io::ErrorKind::InvalidData, "invalid saved-data claim");
    // Future formats can add fields while retaining this ownership header.
    // Its guard still fences this data during an update or store maintenance.
    let record: Record = serde_json::from_slice(bytes).map_err(|_| invalid())?;
    if !record.guard.is_absolute() || record.guard.parent() != Some(&record.runtime) {
        return Err(invalid());
    }
    Ok(record)
}

fn read_path(path: &Path) -> io::Result<Option<Record>> {
    match open_claim(path, false) {
        Ok(file) => read(&file).map(Some),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

/// Only the claim's holder writes. Readers recheck unreadable bytes under
/// the same lock, and will never treat an interrupted write as free state.
fn write(file: &mut File, record: &Record) -> io::Result<()> {
    let bytes = serde_json::to_vec(record).map_err(io::Error::other)?;
    if bytes.len() as u64 > MAX_RECORD_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "saved-data claim exceeds its record size limit",
        ));
    }
    file.write_all_at(&bytes, 0)?;
    file.set_len(bytes.len() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Runtime {
        _directory: tempfile::TempDir,
        lock: PathBuf,
    }

    impl Runtime {
        fn new() -> Self {
            let directory = tempfile::tempdir().unwrap();
            let lock = directory.path().join("cagentsd.lock");
            Self {
                _directory: directory,
                lock,
            }
        }

        fn hold(&self) -> OwnedLock {
            owned_lock(&self.lock, libc::LOCK_EX | libc::LOCK_NB).unwrap()
        }
    }

    fn refusal_of(result: io::Result<StateClaim>) -> StateInUse {
        let error = result.expect_err("the claim is refused");
        state_in_use(&error).cloned().expect("a state refusal")
    }

    #[test]
    fn a_daemon_in_another_runtime_directory_is_refused_while_the_first_serves() {
        let claims = tempfile::tempdir().unwrap();
        let (first, second) = (Runtime::new(), Runtime::new());
        let _first_lease = first.hold();
        let _claim =
            claim_daemon_state_in(claims.path(), &first.lock, Some("first"), Claimant::Daemon)
                .unwrap();
        let _second_lease = second.hold();
        let refusal = refusal_of(claim_daemon_state_in(
            claims.path(),
            &second.lock,
            None,
            Claimant::Daemon,
        ));
        assert_eq!(refusal.pid(), Some(process::id()));
        assert_eq!(
            refusal.runtime(),
            Some(
                fs::canonicalize(first.lock.parent().unwrap())
                    .unwrap()
                    .as_path()
            )
        );
        assert!(refusal.to_string().contains("XDG_RUNTIME_DIR"), "{refusal}");
        assert_eq!(
            served_elsewhere(&claims.path().join(DAEMON_CLAIM), &second.lock).unwrap(),
            Some(refusal)
        );
        assert_eq!(
            served_elsewhere(&claims.path().join(DAEMON_CLAIM), &first.lock).unwrap(),
            None,
            "a daemon's own runtime directory is not elsewhere"
        );
    }

    #[test]
    fn a_held_runtime_lease_keeps_the_state_after_its_daemon_leaves_the_claim() {
        // The updater keeps the outgoing daemon's lease, and maintenance holds
        // it, while nobody holds the claim itself.
        let claims = tempfile::tempdir().unwrap();
        let (serving, other) = (Runtime::new(), Runtime::new());
        let lease = serving.hold();
        drop(claim_daemon_state_in(claims.path(), &serving.lock, None, Claimant::Daemon).unwrap());
        let _other_lease = other.hold();
        refusal_of(claim_daemon_state_in(
            claims.path(),
            &other.lock,
            None,
            Claimant::Daemon,
        ));
        // The successor inherits the same lease and takes the claim at once.
        let successor =
            claim_daemon_state_in(claims.path(), &serving.lock, None, Claimant::Daemon).unwrap();
        drop(successor);
        // With that lease released, the state is free to another directory.
        drop(lease);
        claim_daemon_state_in(claims.path(), &other.lock, None, Claimant::Daemon).unwrap();
    }

    #[test]
    fn launch_diagnoses_persistent_invalid_bytes_after_the_writer_releases_its_claim() {
        let claims = tempfile::tempdir().unwrap();
        let path = claims.path().join(DAEMON_CLAIM);
        let runtime = Runtime::new();
        let mut writer = owned_lock(&path, libc::LOCK_EX | libc::LOCK_NB).unwrap();
        writer.file_mut().write_all(b"{\"future\":").unwrap();
        assert_eq!(served_elsewhere(&path, &runtime.lock).unwrap(), None);
        drop(writer);
        let refusal = served_elsewhere(&path, &runtime.lock).unwrap().unwrap();
        assert!(refusal.to_string().contains("unreadable"), "{refusal}");
    }

    #[test]
    fn an_updater_taking_the_lease_just_after_its_daemon_left_keeps_the_state() {
        // The outgoing daemon has let go of its claim and its lease, and the
        // updater has not yet taken the lease.
        let claims = tempfile::tempdir().unwrap();
        let (serving, other) = (Runtime::new(), Runtime::new());
        drop(claim_daemon_state_in(claims.path(), &serving.lock, None, Claimant::Daemon).unwrap());
        let _other_lease = other.hold();
        let (taken, release) = (
            std::sync::mpsc::channel::<()>(),
            std::sync::mpsc::channel::<()>(),
        );
        let lock = serving.lock.clone();
        let updater = std::thread::spawn(move || {
            std::thread::sleep(POLL * 2);
            let _lease = owned_lock(&lock, libc::LOCK_EX | libc::LOCK_NB).unwrap();
            taken.0.send(()).unwrap();
            release.1.recv().unwrap();
        });
        let refusal = refusal_of(claim_daemon_state_in(
            claims.path(),
            &other.lock,
            None,
            Claimant::Daemon,
        ));
        taken.1.recv().unwrap();
        assert_eq!(refusal.pid(), Some(process::id()));
        release.0.send(()).unwrap();
        updater.join().unwrap();
    }

    #[test]
    fn maintenance_refused_by_a_serving_daemon_says_it_cannot_change_the_data() {
        let claims = tempfile::tempdir().unwrap();
        let (serving, other) = (Runtime::new(), Runtime::new());
        let _lease = serving.hold();
        let _claim = claim_daemon_state_in(
            claims.path(),
            &serving.lock,
            Some("serving"),
            Claimant::Daemon,
        )
        .unwrap();
        let _other_lease = other.hold();
        let refusal = refusal_of(claim_daemon_state_in(
            claims.path(),
            &other.lock,
            None,
            Claimant::Maintenance,
        ));
        let message = refusal.to_string();
        assert!(message.contains("can't be changed now"), "{message}");
        assert!(!message.contains("did not start"), "{message}");
    }

    #[test]
    fn a_held_runtime_lock_is_seen_without_taking_it() {
        let runtime = Runtime::new();
        assert!(!guard_held(&runtime.lock), "a missing lock is free");
        let lease = runtime.hold();
        assert!(guard_held(&runtime.lock));
        drop(lease);
        assert!(!guard_held(&runtime.lock));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn the_lock_list_shows_a_held_runtime_lock_and_not_a_released_one() {
        let runtime = Runtime::new();
        let lease = runtime.hold();
        let listed = || {
            let file = File::open(&runtime.lock).unwrap();
            let metadata = file.metadata().unwrap();
            listed_flock(&file, &metadata)
        };
        assert_ne!(listed(), Listed::Free);
        drop(lease);
        assert_ne!(listed(), Listed::Held);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn the_lock_list_counts_only_a_flock_held_on_the_same_file() {
        // Device 0x1a is major 0, minor 0x1a.
        let held = "1: FLOCK  ADVISORY  WRITE 1234 00:1a:5678 0 EOF\n";
        assert_eq!(parse_locks(held, 0x1a, 5678), Listed::Held);
        assert_eq!(parse_locks(held, 0x1a, 5679), Listed::Free);
        assert_eq!(parse_locks("", 0x1a, 5678), Listed::Free);

        let waiting = "1: FLOCK  ADVISORY  WRITE 1234 00:1a:9 0 EOF\n\
                       1: -> FLOCK  ADVISORY  WRITE 4321 00:1a:5678 0 EOF\n";
        assert_eq!(parse_locks(waiting, 0x1a, 5678), Listed::Free);

        let posix = "1: POSIX  ADVISORY  WRITE 1234 00:1a:5678 0 EOF\n";
        assert_eq!(parse_locks(posix, 0x1a, 5678), Listed::Free);

        let elsewhere = "1: FLOCK  ADVISORY  WRITE 1234 00:2b:5678 0 EOF\n";
        assert_eq!(parse_locks(elsewhere, 0x1a, 5678), Listed::Unknown);
        let also_here = format!("{elsewhere}2: FLOCK  ADVISORY  WRITE 99 00:1a:5678 0 EOF\n");
        assert_eq!(parse_locks(&also_here, 0x1a, 5678), Listed::Held);

        // A major number past 0xff, as for device-mapper volumes, in hex.
        let mapped = libc::makedev(0x103, 2);
        let line = "1: FLOCK  ADVISORY  WRITE 1234 103:02:77 0 EOF\n";
        assert_eq!(parse_locks(line, mapped, 77), Listed::Held);
    }

    #[test]
    fn a_refusal_names_only_a_process_that_still_exists() {
        let mut exited = std::process::Command::new("true").spawn().unwrap();
        let gone = exited.id();
        exited.wait().unwrap();
        let record = |pid| Record {
            format: FORMAT,
            guard: PathBuf::from("/nonexistent/cagentsd.lock"),
            runtime: PathBuf::from("/nonexistent"),
            pid,
            host_session: None,
            boot: None,
            hostname: None,
        };
        assert_eq!(
            refusal(Claimant::Daemon, Some(&record(gone)), false).pid,
            None
        );
        let serving = std::process::id();
        assert_eq!(
            refusal(Claimant::Daemon, Some(&record(serving)), false).pid,
            Some(serving)
        );
    }

    #[test]
    fn a_claim_whose_runtime_directory_is_gone_is_free() {
        let claims = tempfile::tempdir().unwrap();
        let gone = Runtime::new();
        drop(claim_daemon_state_in(claims.path(), &gone.lock, None, Claimant::Daemon).unwrap());
        drop(gone);
        let current = Runtime::new();
        let _lease = current.hold();
        claim_daemon_state_in(claims.path(), &current.lock, None, Claimant::Daemon).unwrap();
    }

    #[test]
    fn windows_claim_each_slot_separately() {
        let claims = tempfile::tempdir().unwrap();
        let (first, second) = (Runtime::new(), Runtime::new());
        let _first_slot = first.hold();
        let _second_slot = second.hold();
        let _primary = claim_window_state_in(claims.path(), PRIMARY_WINDOW, &first.lock).unwrap();
        let refusal = refusal_of(claim_window_state_in(
            claims.path(),
            PRIMARY_WINDOW,
            &second.lock,
        ));
        assert!(refusal.to_string().contains("already open"), "{refusal}");
        claim_window_state_in(claims.path(), "window-2", &second.lock).unwrap();
    }

    #[test]
    fn a_newer_format_still_honors_its_held_runtime_guard() {
        let claims = tempfile::tempdir().unwrap();
        let (first, second) = (Runtime::new(), Runtime::new());
        let held = first.hold();
        let _other = second.hold();
        let path = claims.path().join(DAEMON_CLAIM);
        let mut opened = owned_lock(&path, libc::LOCK_EX).unwrap();
        let mut saved = record(&first.lock, None).unwrap();
        saved.format = FORMAT + 256;
        write(opened.file_mut(), &saved).unwrap();
        drop(opened);
        let bytes = fs::read(&path).unwrap();

        let refused = refusal_of(claim_daemon_state_in(
            claims.path(),
            &second.lock,
            None,
            Claimant::Daemon,
        ));
        assert_eq!(refused.runtime(), Some(saved.runtime.as_path()));
        assert_eq!(fs::read(&path).unwrap(), bytes);

        drop(held);
        claim_daemon_state_in(claims.path(), &second.lock, None, Claimant::Daemon).unwrap();
    }

    #[test]
    fn an_unreadable_claim_is_neither_erased_nor_taken_over() {
        let claims = tempfile::tempdir().unwrap();
        let current = Runtime::new();
        let _held = current.hold();
        let path = claims.path().join(DAEMON_CLAIM);
        let mut opened = owned_lock(&path, libc::LOCK_EX).unwrap();
        opened.file_mut().write_all(b"{future format").unwrap();
        drop(opened);

        let refused = refusal_of(claim_daemon_state_in(
            claims.path(),
            &current.lock,
            None,
            Claimant::Daemon,
        ));
        assert!(refused.to_string().contains("unreadable"), "{refused}");
        assert_eq!(fs::read(&path).unwrap(), b"{future format");
    }

    #[test]
    fn a_persistent_empty_claim_refuses_a_different_runtime() {
        let claims = tempfile::tempdir().unwrap();
        let (last, next) = (Runtime::new(), Runtime::new());
        let _updater = last.hold();
        let _next = next.hold();
        let path = claims.path().join(DAEMON_CLAIM);
        drop(owned_lock(&path, libc::LOCK_EX).unwrap());
        let refusal = served_elsewhere(&path, &next.lock).unwrap().unwrap();
        assert!(refusal.to_string().contains("unreadable"), "{refusal}");
        let refusal = refusal_of(claim_daemon_state_in(
            claims.path(),
            &next.lock,
            None,
            Claimant::Daemon,
        ));
        assert!(refusal.to_string().contains("unreadable"), "{refusal}");
        assert_eq!(fs::metadata(path).unwrap().len(), 0);
    }

    #[test]
    fn an_invalid_claim_path_is_a_typed_startup_refusal() {
        let claims = tempfile::tempdir().unwrap();
        let next = Runtime::new();
        let _lease = next.hold();
        let path = claims.path().join(DAEMON_CLAIM);
        let inaccessible = claims.path().join("missing");
        std::os::unix::fs::symlink(inaccessible, &path).unwrap();
        let refusal = served_elsewhere(&path, &next.lock).unwrap().unwrap();
        assert!(refusal.to_string().contains("unreadable"), "{refusal}");
        let refusal = refusal_of(claim_daemon_state_in(
            claims.path(),
            &next.lock,
            None,
            Claimant::Daemon,
        ));
        assert!(refusal.to_string().contains("unreadable"), "{refusal}");
        assert!(fs::symlink_metadata(path).unwrap().is_symlink());
    }

    #[test]
    fn a_symlinked_claim_directory_cannot_bypass_a_live_claim() {
        let data = tempfile::tempdir().unwrap();
        let claims = data.path().join("claims");
        let moved = data.path().join("moved");
        let (first, second) = (Runtime::new(), Runtime::new());
        let _first = first.hold();
        let _second = second.hold();
        let _daemon = claim_daemon_state_in(&claims, &first.lock, None, Claimant::Daemon).unwrap();
        let _window = claim_window_state_in(&claims, PRIMARY_WINDOW, &first.lock).unwrap();
        fs::rename(&claims, &moved).unwrap();
        std::os::unix::fs::symlink(&moved, &claims).unwrap();
        assert!(private_lock_is_held(&moved.join(DAEMON_CLAIM)).unwrap());
        for failure in [
            refusal_of(claim_daemon_state_in(
                &claims,
                &second.lock,
                None,
                Claimant::Daemon,
            )),
            refusal_of(claim_window_state_in(&claims, PRIMARY_WINDOW, &second.lock)),
        ] {
            assert!(failure.to_string().contains("unreadable"), "{failure}");
        }
    }

    #[test]
    fn first_claim_remains_readable_under_restrictive_umask() {
        const FIXTURE: &str = "CAGENTS_TEST_CLAIM_UMASK";
        if env::var_os(FIXTURE).is_none() {
            let status = process::Command::new(env::current_exe().unwrap())
                .args(["--exact", "instance::state_claim::tests::first_claim_remains_readable_under_restrictive_umask", "--test-threads=1"])
                .env(FIXTURE, "1")
                .status()
                .unwrap();
            assert!(status.success());
            return;
        }
        let root = tempfile::tempdir().unwrap();
        let runtime = Runtime::new();
        let _owner = runtime.hold();
        let claim = root.path().join(DAEMON_CLAIM);
        let claim_record = record(&runtime.lock, None).unwrap();
        // SAFETY: only the isolated subprocess test changes its umask.
        let original = unsafe { libc::umask(0o777) };
        let first = acquire(
            &claim,
            claim_record.clone(),
            Claimant::Daemon,
            Duration::ZERO,
        );
        unsafe { libc::umask(original) };
        drop(first.unwrap());
        assert_eq!(
            fs::metadata(&claim).unwrap().mode() & 0o777,
            PRIVATE_FILE_MODE
        );
        assert_eq!(read_path(&claim).unwrap(), Some(claim_record.clone()));
        acquire(&claim, claim_record, Claimant::Daemon, Duration::ZERO).unwrap();
    }

    #[test]
    fn a_checked_handoff_keeps_an_intervening_saved_data_claim() {
        let root = tempfile::tempdir().unwrap();
        prepare_claim_directory(root.path(), Claimant::Daemon).unwrap();
        let runtime = Runtime::new();
        let _owner = runtime.hold();
        let path = root.path().join(DAEMON_CLAIM);
        let original = record(&runtime.lock, Some("original")).unwrap();
        let other = record(&runtime.lock, Some("intervening")).unwrap();
        let successor = record(&runtime.lock, Some("successor")).unwrap();
        let handoff = || {
            saved_owner::acquire(&path, &successor, Duration::ZERO, |previous| {
                previous == &original
            })
        };

        assert!(handoff().is_err());
        assert!(!path.exists());
        drop(create_claim(&path, &original).unwrap());
        drop(acquire(&path, other.clone(), Claimant::Daemon, Duration::ZERO).unwrap());
        let saved = fs::read(&path).unwrap();
        assert!(handoff().is_err());
        assert_eq!(fs::read(&path).unwrap(), saved);
        drop(acquire(&path, original.clone(), Claimant::Daemon, Duration::ZERO).unwrap());
        drop(handoff().unwrap());
        assert_eq!(read_path(&path).unwrap(), Some(original));
        assert_eq!(saved_owner::read_path(&path).unwrap(), Some(successor));
    }

    #[test]
    fn a_long_runtime_claim_can_be_reopened_and_oversize_writes_preserve_it() {
        let root = tempfile::tempdir().unwrap();
        let mut runtime = root.path().to_path_buf();
        // PATH_MAX is 1024 on Darwin. This specifically crosses the old
        // record cap on systems that accept long runtime directories.
        let depth = if cfg!(target_os = "linux") { 24 } else { 6 };
        for _ in 0..depth {
            runtime.push("r".repeat(96));
        }
        fs::create_dir_all(&runtime).unwrap();
        let guard = runtime.join("cagentsd.lock");
        let _lock = owned_lock(&guard, libc::LOCK_EX | libc::LOCK_NB).unwrap();
        let claim = root.path().join(DAEMON_CLAIM);
        let valid = record(&guard, None).unwrap();
        drop(acquire(&claim, valid.clone(), Claimant::Daemon, Duration::ZERO).unwrap());
        assert_eq!(read_path(&claim).unwrap(), Some(valid.clone()));
        let mut oversize = valid.clone();
        oversize.host_session = Some("x".repeat(MAX_RECORD_BYTES as usize));
        assert!(acquire(&claim, oversize, Claimant::Daemon, Duration::ZERO).is_err());
        assert_eq!(read_path(&claim).unwrap(), Some(valid.clone()));
        acquire(&claim, valid, Claimant::Daemon, Duration::ZERO).unwrap();
    }

    #[test]
    fn a_runtime_guard_that_cannot_be_inspected_is_not_free() {
        let first = Runtime::new();
        let second = Runtime::new();
        let claims = tempfile::tempdir().unwrap();
        let path = claims.path().join(DAEMON_CLAIM);
        let prior = record(&first.lock, None).unwrap();
        let _maintenance = first.hold();
        let _next = second.hold();
        drop(create_claim(&path, &prior).unwrap());
        let parent = first.lock.parent().unwrap();
        fs::set_permissions(parent, fs::Permissions::from_mode(0o000)).unwrap();
        let inaccessible = fs::File::open(&first.lock).is_err();
        let attempt = acquire(
            &path,
            record(&second.lock, None).unwrap(),
            Claimant::Daemon,
            Duration::ZERO,
        );
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700)).unwrap();
        if unsafe { libc::geteuid() } != 0 {
            assert!(
                inaccessible,
                "permission changes must exercise the inspection failure"
            );
        }
        assert!(state_in_use(&attempt.unwrap_err()).is_some());
        assert_eq!(read_path(&path).unwrap(), Some(prior));
    }

    #[test]
    fn a_foreign_maintenance_guard_on_shared_storage_still_excludes_takeover() {
        let first = Runtime::new();
        let second = Runtime::new();
        let claims = tempfile::tempdir().unwrap();
        let path = claims.path().join(DAEMON_CLAIM);
        let mut prior = record(&first.lock, Some("previous")).unwrap();
        prior.boot = Some("before-the-update".into());
        prior.hostname = Some("build-host".into());
        let _maintenance = owned_lock(&first.lock, libc::LOCK_EX | libc::LOCK_NB).unwrap();
        let _next = owned_lock(&second.lock, libc::LOCK_EX | libc::LOCK_NB).unwrap();
        drop(create_claim(&path, &prior).unwrap());
        let mut ours = record(&second.lock, Some("new")).unwrap();
        ours.boot = Some("new-boot".into());
        let error = acquire(&path, ours, Claimant::Daemon, Duration::ZERO).unwrap_err();
        assert!(
            state_in_use(&error)
                .unwrap()
                .to_string()
                .contains("build-host")
        );
        assert_eq!(read_path(&path).unwrap(), Some(prior));
    }

    #[test]
    fn a_held_claim_from_another_boot_names_that_host_even_at_the_same_path() {
        let claims = tempfile::tempdir().unwrap();
        let ours = Runtime::new();
        let _lease = ours.hold();
        let path = claims.path().join(DAEMON_CLAIM);
        let mut held = owned_lock(&path, libc::LOCK_EX).unwrap();
        let mut remote = record(&ours.lock, None).unwrap();
        let local = remote
            .boot
            .as_ref()
            .expect("test requires the local kernel boot ID");
        remote.boot = Some(format!("{local}-other"));
        remote.hostname = Some("workstation-b".into());
        write(held.file_mut(), &remote).unwrap();

        let refused = refusal_of(claim_daemon_state_in(
            claims.path(),
            &ours.lock,
            None,
            Claimant::Daemon,
        ));
        assert_eq!(refused.pid(), None);
        assert_eq!(refused.runtime(), None);
        assert!(refused.to_string().contains("workstation-b"), "{refused}");
        assert_eq!(served_elsewhere(&path, &ours.lock).unwrap(), Some(refused));
        drop(held);

        assert_eq!(served_elsewhere(&path, &ours.lock).unwrap(), None);
        claim_daemon_state_in(claims.path(), &ours.lock, None, Claimant::Daemon).unwrap();
    }

    #[test]
    fn a_half_written_claim_is_waited_out_and_then_refused() {
        let claims = tempfile::tempdir().unwrap();
        let runtime = Runtime::new();
        let _lease = runtime.hold();
        crate::secure_file::prepare_private_directory(claims.path()).unwrap();
        // A holder whose record nobody can read yet is not taken for a
        // predecessor forever.
        let _holder = owned_lock(
            &claims.path().join(DAEMON_CLAIM),
            libc::LOCK_EX | libc::LOCK_NB,
        )
        .unwrap();
        let started = std::time::Instant::now();
        let refusal = refusal_of(acquire(
            &claims.path().join(DAEMON_CLAIM),
            record(&runtime.lock, None).unwrap(),
            Claimant::Daemon,
            Duration::from_millis(100),
        ));
        assert!(started.elapsed() >= Duration::from_millis(100));
        assert_eq!(refusal.pid(), None);
    }
}
