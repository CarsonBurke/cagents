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
//! One narrow window remains: after the outgoing daemon releases its lease and
//! before the updater has taken it, nothing holds either lock. A daemon from
//! another runtime directory that starts exactly then is admitted, and the
//! update's successor is the one refused. Only one daemon serves the data at a
//! time either way.

use super::*;
use serde::{Deserialize, Serialize};
use std::os::unix::fs::FileExt;

/// How a claim is recorded. Unknown fields are ignored so that a later version
/// can add some without an earlier one reading its claim as missing.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
struct Record {
    format: u8,
    /// The holder's runtime lock: whoever holds it acts for this claim.
    guard: PathBuf,
    /// The runtime directory the holder serves, for people to read.
    runtime: PathBuf,
    pid: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    host_session: Option<String>,
}

const FORMAT: u8 = 1;
const MAX_RECORD_BYTES: u64 = 4096;
const POLL: Duration = Duration::from_millis(20);
/// How long a daemon waits for a predecessor in its own runtime directory to
/// finish exiting. A daemon releases its claim before its lease, so this is
/// only reached when a process dies holding both and its exit is slow.
const DAEMON_PATIENCE: Duration = Duration::from_secs(5);
/// The same, for a window that relaunches in its own runtime directory.
const WINDOW_PATIENCE: Duration = Duration::from_secs(2);
const CLAIMS_DIRECTORY: &str = "claims";
const DAEMON_CLAIM: &str = "daemon.lock";

/// What holds a claim.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Claimant {
    Daemon,
    Window,
}

/// The state is in use by a daemon or window that another runtime directory
/// serves.
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub struct StateInUse {
    claimant: Claimant,
    pid: Option<u32>,
    runtime: Option<PathBuf>,
}

impl StateInUse {
    pub fn pid(&self) -> Option<u32> {
        self.pid
    }

    pub fn runtime(&self) -> Option<&Path> {
        self.runtime.as_deref()
    }
}

impl std::fmt::Display for StateInUse {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self.claimant {
            Claimant::Daemon => "Another cagents daemon",
            Claimant::Window => "This cagents window is already open in another session",
        })?;
        match (self.pid, &self.runtime) {
            (Some(pid), Some(runtime)) => write!(
                formatter,
                " (process {pid}, runtime directory {})",
                runtime.display()
            )?,
            (Some(pid), None) => write!(formatter, " (process {pid})")?,
            (None, _) => {}
        }
        formatter.write_str(match self.claimant {
            Claimant::Daemon => {
                " is already using this cagents data. Two daemons on the same data overwrite \
                 each other's chats, so this one did not start. To use the running daemon, \
                 start cagents with the same XDG_RUNTIME_DIR; otherwise stop it first."
            }
            Claimant::Window => {
                ". Two copies of one window overwrite each other's saved layout, so this one \
                 did not open. Switch to the open window, or close it first."
            }
        })
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

/// Claim this instance's daemon state for the daemon, or the maintenance,
/// that holds the runtime lease `lock`. The caller must hold that lease.
pub fn claim_daemon_state(lock: &Path, host_session: Option<&str>) -> io::Result<StateClaim> {
    claim_daemon_state_in(&claims_directory()?, lock, host_session)
}

pub(crate) fn claim_daemon_state_in(
    claims: &Path,
    lock: &Path,
    host_session: Option<&str>,
) -> io::Result<StateClaim> {
    crate::secure_file::prepare_private_directory(claims)?;
    acquire(
        &claims.join(DAEMON_CLAIM),
        record(lock, host_session)?,
        Claimant::Daemon,
        DAEMON_PATIENCE,
    )
}

/// The daemon that another runtime directory serves this instance's state
/// from, if one does, for a launcher deciding whether to start one here.
/// Reading a claim takes no lock on it, so a starting daemon is never held up.
pub fn daemon_served_elsewhere(lock: &Path) -> io::Result<Option<StateInUse>> {
    served_elsewhere(&claims_directory()?.join(DAEMON_CLAIM), lock)
}

pub(crate) fn served_elsewhere(claim: &Path, lock: &Path) -> io::Result<Option<StateInUse>> {
    let Some(recorded) = read_path(claim)? else {
        return Ok(None);
    };
    let ours = canonical_guard(lock);
    if recorded.guard == ours || !guard_held(&recorded.guard)? {
        return Ok(None);
    }
    Ok(Some(refusal(Claimant::Daemon, Some(&recorded))))
}

/// Claim one window's saved state for the window that holds its slot lock
/// `slot_lock`. The caller must hold that lock.
pub(crate) fn claim_window_state_in(
    claims: &Path,
    slot: &str,
    slot_lock: &Path,
) -> io::Result<StateClaim> {
    validate_window_slot(slot)?;
    crate::secure_file::prepare_private_directory(claims)?;
    acquire(
        &claims.join(format!("window.{slot}.lock")),
        record(slot_lock, None)?,
        Claimant::Window,
        WINDOW_PATIENCE,
    )
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
    })
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

fn acquire(
    path: &Path,
    ours: Record,
    claimant: Claimant,
    patience: Duration,
) -> io::Result<StateClaim> {
    let started = std::time::Instant::now();
    loop {
        let found = match owned_lock(path, libc::LOCK_EX | libc::LOCK_NB) {
            Ok(mut lock) => {
                let last = read(&lock);
                if let Some(last) = &last
                    && last.guard != ours.guard
                    && guard_held(&last.guard)?
                {
                    // The claim's last holder left it, but its runtime lock is
                    // still held: by its updater, or by store maintenance.
                    return Err(io::Error::other(refusal(claimant, Some(last))));
                }
                write(lock.file_mut(), &ours)?;
                return Ok(StateClaim { _lock: lock });
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => read_path(path)?,
            Err(error) => return Err(error),
        };
        if let Some(found) = &found
            && found.guard != ours.guard
            && guard_held(&found.guard)?
        {
            return Err(io::Error::other(refusal(claimant, Some(found))));
        }
        // Held, but by a predecessor in this runtime directory that has let
        // go of its runtime lock and is exiting, by a holder whose runtime
        // lock is free and so is exiting too, or by one still writing its
        // record. Each resolves by itself.
        if started.elapsed() >= patience {
            return Err(io::Error::other(refusal(claimant, found.as_ref())));
        }
        std::thread::sleep(POLL);
    }
}

fn refusal(claimant: Claimant, record: Option<&Record>) -> StateInUse {
    StateInUse {
        claimant,
        pid: record.map(|record| record.pid),
        runtime: record.map(|record| record.runtime.clone()),
    }
}

/// Whether anybody holds the runtime lock at `path`. A lock that no longer
/// exists, as after its runtime directory was removed, is free.
fn guard_held(path: &Path) -> io::Result<bool> {
    let lock = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(path)
    {
        Ok(lock) => lock,
        Err(error)
            if error.kind() == io::ErrorKind::NotFound
                || error.raw_os_error() == Some(libc::ENOTDIR) =>
        {
            return Ok(false);
        }
        Err(error) => return Err(error),
    };
    let metadata = lock.metadata()?;
    if !metadata.is_file() || metadata.uid() != current_uid() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{} is not a lock file you own", path.display()),
        ));
    }
    match owned_file_lock(lock, libc::LOCK_EX | libc::LOCK_NB) {
        // Dropping the probe releases it at once.
        Ok(_) => Ok(false),
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(true),
        Err(error) => Err(error),
    }
}

fn read(file: &File) -> Option<Record> {
    let length = file.metadata().ok()?.len();
    if length == 0 || length > MAX_RECORD_BYTES {
        return None;
    }
    let mut bytes = vec![0; length as usize];
    file.read_exact_at(&mut bytes, 0).ok()?;
    let record: Record = serde_json::from_slice(&bytes).ok()?;
    (record.format == FORMAT).then_some(record)
}

fn read_path(path: &Path) -> io::Result<Option<Record>> {
    match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
    {
        Ok(file) => Ok(read(&file)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

/// Only the claim's holder writes, and only right after taking it. A reader
/// that catches the record half written reads none and looks again.
fn write(file: &mut File, record: &Record) -> io::Result<()> {
    let bytes = serde_json::to_vec(record).map_err(io::Error::other)?;
    file.set_len(0)?;
    file.write_all_at(&bytes, 0)
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
        let _claim = claim_daemon_state_in(claims.path(), &first.lock, Some("first")).unwrap();
        let _second_lease = second.hold();
        let refusal = refusal_of(claim_daemon_state_in(claims.path(), &second.lock, None));
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
        drop(claim_daemon_state_in(claims.path(), &serving.lock, None).unwrap());
        let _other_lease = other.hold();
        refusal_of(claim_daemon_state_in(claims.path(), &other.lock, None));
        // The successor inherits the same lease and takes the claim at once.
        let successor = claim_daemon_state_in(claims.path(), &serving.lock, None).unwrap();
        drop(successor);
        // With that lease released, the state is free to another directory.
        drop(lease);
        claim_daemon_state_in(claims.path(), &other.lock, None).unwrap();
    }

    #[test]
    fn a_claim_whose_runtime_directory_is_gone_is_free() {
        let claims = tempfile::tempdir().unwrap();
        let gone = Runtime::new();
        drop(claim_daemon_state_in(claims.path(), &gone.lock, None).unwrap());
        drop(gone);
        let current = Runtime::new();
        let _lease = current.hold();
        claim_daemon_state_in(claims.path(), &current.lock, None).unwrap();
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
