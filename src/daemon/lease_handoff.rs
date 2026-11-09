//! Transfer one flock open-file description from the updater to its successor.
use super::*;
use std::{os::fd::BorrowedFd, os::unix::process::CommandExt, process::Command};

pub(crate) const DAEMON_LEASE_ENV: &str = "CAGENTS_DAEMON_LEASE_FD";
const DAEMON_LEASE_FD: i32 = 199;

/// Unlike HostLease, this guard never unlinks a socket or explicitly unlocks:
/// the successor shares its open-file description and must retain the lock.
pub(crate) struct LeaseHandoff {
    lock: File,
}

pub(crate) struct HandoffSource {
    pub(crate) pid: u32,
    process: File,
    // An adopted flock retains the original updater PID, which can differ
    // from the serving daemon pinned above. Compare like identities.
    pub(crate) locker_pid: Option<i32>,
}

impl HandoffSource {
    pub(crate) fn pin(
        paths: &RuntimePaths,
        proof: &HostSessionProof,
    ) -> Result<Self, PlatformError> {
        let lock = open_lock(paths)?;
        let pid = lease_owner(&paths.lock, proof)?;
        let process = open_validated_pidfd(&lock, pid, Path::new("/proc"))?;
        let locker_pid = holder_pid(paths)?;
        Ok(Self {
            pid,
            process,
            locker_pid,
        })
    }

    pub(crate) fn exited(&self) -> io::Result<bool> {
        let mut poll = libc::pollfd {
            fd: self.process.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one live initialized pollfd; pidfd pins the original process.
        if unsafe { libc::poll(&mut poll, 1, 0) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(poll.revents != 0)
    }
}

impl LeaseHandoff {
    pub(crate) fn wait(paths: &RuntimePaths, expected_pid: u32) -> Result<Self, PlatformError> {
        let lock = open_lock(paths)?;
        loop {
            // SAFETY: a live validated regular file; flock blocks off the job thread.
            if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) } == 0 {
                break;
            }
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error.into());
            }
        }
        validate_inode(&lock, paths)?;
        let stored: StoredLockMetadata = serde_json::from_reader(&lock)?;
        if stored.pid != expected_pid {
            return Err(io::Error::other(format!(
                "Terminal service lease changed during handoff: expected process {expected_pid}, found holder process {}",
                stored.pid
            )).into());
        }
        Ok(Self { lock })
    }

    pub(crate) fn inherit(&self, command: &mut Command) -> io::Result<()> {
        forward(
            command,
            self.lock.try_clone()?,
            self.lock.as_raw_fd() == DAEMON_LEASE_FD,
        )
    }
}

fn open_lock(paths: &RuntimePaths) -> Result<File, PlatformError> {
    let directory = fs::symlink_metadata(&paths.directory)?;
    validate_owner(&paths.directory, &directory, current_uid())?;
    if !directory.is_dir() || directory.mode() & 0o022 != 0 {
        return Err(io::Error::other("unsafe daemon runtime directory").into());
    }
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(LOCK_OPEN_FLAGS | libc::O_CLOEXEC)
        .open(&paths.lock)?;
    validate_lock_file(&paths.lock, &lock.metadata()?, current_uid())?;
    Ok(lock)
}

fn validate_inode(lock: &File, paths: &RuntimePaths) -> Result<(), PlatformError> {
    let actual = lock.metadata()?;
    validate_lock_file(&paths.lock, &actual, current_uid())?;
    let expected = fs::symlink_metadata(&paths.lock)?;
    validate_lock_file(&paths.lock, &expected, current_uid())?;
    if actual.dev() != expected.dev() || actual.ino() != expected.ino() {
        return Err(io::Error::other("daemon inherited a different lease inode").into());
    }
    Ok(())
}

pub(super) fn validate_inherited(lock: &File, paths: &RuntimePaths) -> Result<(), PlatformError> {
    validate_inode(lock, paths)?;
    // fdinfo describes this exact open-file description, including locks
    // inherited from another PID. Contention alone could be someone else's lock.
    let info = fs::read_to_string(format!("/proc/self/fdinfo/{}", lock.as_raw_fd()))?;
    if !info
        .lines()
        .filter_map(|line| line.strip_prefix("lock:"))
        .any(|line| {
            let fields: Vec<_> = line.split_whitespace().collect();
            fields.len() == 8
                && fields[1..4] == ["FLOCK", "ADVISORY", "WRITE"]
                && fields[6..8] == ["0", "EOF"]
        })
    {
        return Err(io::Error::other("daemon inherited an unheld lease descriptor").into());
    }
    // SAFETY: the validated descriptor must itself own the exclusive flock.
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err(io::Error::other("daemon inherited a lease held by another process").into());
    }
    Ok(())
}

fn inherited() -> io::Result<Option<File>> {
    let Some(value) = env::var_os(DAEMON_LEASE_ENV) else {
        return Ok(None);
    };
    if value != "199" {
        return Err(io::Error::other("invalid daemon lease descriptor"));
    }
    // SAFETY: check existence before borrowing; clone gives this caller ownership.
    if unsafe { libc::fcntl(DAEMON_LEASE_FD, libc::F_GETFD) } == -1 {
        return Err(io::Error::last_os_error());
    }
    let lock = File::from(unsafe { BorrowedFd::borrow_raw(DAEMON_LEASE_FD) }.try_clone_to_owned()?);
    let paths = RuntimePaths::discover().map_err(io::Error::other)?;
    validate_inherited(&lock, &paths).map_err(io::Error::other)?;
    Ok(Some(lock))
}

pub(crate) fn has_inherited_lease() -> io::Result<bool> {
    Ok(inherited()?.is_some())
}

pub(crate) fn inherit_daemon_lease(command: &mut Command) -> io::Result<()> {
    if let Some(lock) = inherited()? {
        forward(command, lock, true)?;
    }
    Ok(())
}

/// Consume startup authority before any diagnostics/runtime threads exist.
/// # Safety
/// The caller must be the single-threaded daemon entry point.
#[cfg(feature = "host")]
pub(crate) unsafe fn take_inherited_lease() -> io::Result<Option<File>> {
    let lock = inherited()?;
    if lock.is_some() {
        // SAFETY: caller guarantees exclusive environment access; the reserved
        // original is owned by this startup contract. All retained copies use CLOEXEC.
        unsafe {
            env::remove_var(DAEMON_LEASE_ENV);
            libc::close(DAEMON_LEASE_FD);
        }
    }
    Ok(lock)
}

fn forward(command: &mut Command, lock: File, inherited_slot: bool) -> io::Result<()> {
    if !inherited_slot
        && lock.as_raw_fd() != DAEMON_LEASE_FD
        && unsafe { libc::fcntl(DAEMON_LEASE_FD, libc::F_GETFD) } != -1
    {
        return Err(io::Error::from_raw_os_error(libc::EBUSY));
    }
    command.env(DAEMON_LEASE_ENV, DAEMON_LEASE_FD.to_string());
    // SAFETY: only descriptor syscalls after fork. Capturing the File keeps
    // the source alive until spawn; this hook follows descriptor sealing.
    unsafe {
        command.pre_exec(move || {
            if !inherited_slot
                && lock.as_raw_fd() != DAEMON_LEASE_FD
                && libc::fcntl(DAEMON_LEASE_FD, libc::F_GETFD) != -1
            {
                return Err(io::Error::from_raw_os_error(libc::EBUSY));
            }
            if libc::dup2(lock.as_raw_fd(), DAEMON_LEASE_FD) == -1
                || libc::fcntl(DAEMON_LEASE_FD, libc::F_SETFD, 0) == -1
            {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    Ok(())
}

/// Kernel lock ownership, including contenders that have not written metadata.
pub(crate) fn holder_pid(paths: &RuntimePaths) -> io::Result<Option<i32>> {
    let metadata = fs::symlink_metadata(&paths.lock)?;
    let device = metadata.dev();
    Ok(fs::read_to_string("/proc/locks")?.lines().find_map(|line| {
        let fields: Vec<_> = line.split_whitespace().collect();
        if fields.len() != 8 || fields[1..4] != ["FLOCK", "ADVISORY", "WRITE"] {
            return None;
        }
        let mut key = fields[5].split(':');
        let major = u32::from_str_radix(key.next()?, 16).ok()?;
        let minor = u32::from_str_radix(key.next()?, 16).ok()?;
        let inode = key.next()?.parse::<u64>().ok()?;
        (major == libc::major(device) && minor == libc::minor(device) && inode == metadata.ino())
            .then(|| fields[4].parse().ok())
            .flatten()
    }))
}
