#[cfg(all(feature = "host", target_os = "linux"))]
pub(crate) mod child_reaper;
pub(crate) mod holder_registry;
#[cfg(feature = "host")]
pub(crate) mod owned_children;
#[cfg(feature = "host")]
pub(crate) mod process_scope;

use std::{
    env,
    ffi::c_int,
    fs::{self, File, OpenOptions},
    io::{self, Seek, SeekFrom},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::{
            fs::{FileTypeExt, MetadataExt, OpenOptionsExt},
            net::UnixStream,
        },
    },
    path::{Path, PathBuf},
};

#[cfg(feature = "host")]
use std::{
    fs::DirBuilder,
    io::{Read, Write},
    os::unix::{
        fs::{DirBuilderExt, PermissionsExt},
        net::UnixListener,
    },
    sync::{
        LazyLock,
        atomic::{AtomicBool, AtomicI32, Ordering},
    },
};

use serde::Deserialize;
#[cfg(feature = "host")]
use serde::Serialize;
use thiserror::Error;

use crate::model::{BootId, HostSessionProof};
use crate::{instance, terminal_ipc::SOCKET_FILE_NAME};

#[cfg(feature = "host")]
const PRIVATE_DIRECTORY_MODE: u32 = 0o700;
#[cfg(any(feature = "host", test))]
const PRIVATE_FILE_MODE: u32 = 0o600;
const O_NOFOLLOW: c_int = libc::O_NOFOLLOW;
// Validate the opened descriptor without blocking on an invalid special file.
const LOCK_OPEN_FLAGS: c_int = O_NOFOLLOW | libc::O_NONBLOCK;
#[cfg(feature = "host")]
const POLL_FOREVER: c_int = -1;

#[cfg(feature = "host")]
static TERMINATION_REQUESTED: AtomicBool = AtomicBool::new(false);

/// The daemon's wakeup pipe. It is created once and never dropped, because the
/// signal handler writes to its sending descriptor: that descriptor must never
/// be closed, and its number must never be recycled by an unrelated `open`.
#[cfg(feature = "host")]
static WAKEUP: LazyLock<io::Result<Wakeup>> = LazyLock::new(Wakeup::create);
/// The sending descriptor of [`WAKEUP`], readable from a signal handler, which
/// must touch nothing but lock-free atomics.
#[cfg(feature = "host")]
static WAKEUP_FD: AtomicI32 = AtomicI32::new(-1);

#[cfg(feature = "host")]
/// A socket pair used purely as a wakeup channel: any byte written to `writer`
/// interrupts a [`wait_for_activity`] blocked on `reader`.
struct Wakeup {
    reader: UnixStream,
    writer: UnixStream,
}

#[cfg(feature = "host")]
impl Wakeup {
    fn create() -> io::Result<Self> {
        let channel = Self::channel()?;
        // Only the never-dropped static publishes a signal-handler descriptor.
        WAKEUP_FD.store(channel.writer.as_raw_fd(), Ordering::Release);
        Ok(channel)
    }

    fn channel() -> io::Result<Self> {
        let (reader, writer) = UnixStream::pair()?;
        reader.set_nonblocking(true)?;
        // A full pipe is itself a pending wakeup, so no writer may block on one.
        writer.set_nonblocking(true)?;
        Ok(Self { reader, writer })
    }

    fn wake(&self) {
        let mut writer = &self.writer;
        let _ = writer.write(&[0]);
    }

    /// Empties the pipe, so a burst of wakeups leaves no readable byte behind to
    /// spin the accept loop.
    fn drain(&self) {
        let mut reader = &self.reader;
        let mut buffer = [0_u8; 64];
        while matches!(reader.read(&mut buffer), Ok(read) if read > 0) {}
    }
}

#[cfg(feature = "host")]
fn wakeup() -> io::Result<&'static Wakeup> {
    // `io::Error` is not `Clone`, so the stored failure is reproduced rather
    // than handed out; kind and message are what callers report.
    WAKEUP
        .as_ref()
        .map_err(|error| io::Error::new(error.kind(), error.to_string()))
}

/// Interrupts a blocking [`wait_for_activity`] from any thread. Losing a byte to
/// a full pipe is harmless, because a full pipe is a pending wakeup.
#[cfg(feature = "host")]
pub(crate) fn wake() {
    if let Ok(wakeup) = &*WAKEUP {
        wakeup.wake();
    }
}

/// Blocks until `listener` has a connection pending or something calls [`wake`].
/// There is no timeout: an idle daemon causes no wakeups at all.
#[cfg(feature = "host")]
pub(crate) fn wait_for_activity(listener: &UnixListener) -> io::Result<()> {
    wait_for_activity_with(listener, wakeup()?)
}

#[cfg(feature = "host")]
fn wait_for_activity_with(listener: &UnixListener, wakeup: &Wakeup) -> io::Result<()> {
    let mut fds = [
        libc::pollfd {
            fd: listener.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        },
        libc::pollfd {
            fd: wakeup.reader.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        },
    ];
    // SAFETY: fds is a live, fully initialised array of the length passed to
    // poll, and both descriptors are owned for the duration of the call.
    let ready = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, POLL_FOREVER) };
    if ready < 0 {
        let error = io::Error::last_os_error();
        // A signal handler ran. The caller re-reads the termination flag, so an
        // interrupted wait is a wakeup rather than a failure.
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    } else if fds[1].revents != 0 {
        wakeup.drain();
    }
    Ok(())
}

#[derive(Debug, Error)]
pub(crate) enum PlatformError {
    #[error("XDG_RUNTIME_DIR is not set to an absolute path")]
    MissingRuntimeDirectory,
    #[error("runtime path {path:?} is not owned by the current user")]
    WrongRuntimeOwner { path: PathBuf },
    #[error("runtime path {path:?} is not a real directory")]
    #[cfg(feature = "host")]
    UnsafeRuntimeDirectory { path: PathBuf },
    #[error("daemon lock path {path:?} is not a regular file")]
    UnsafeLockPath { path: PathBuf },
    #[error("daemon lock path {path:?} has {links} filesystem links; expected exactly one")]
    UnsafeLockLinkCount { path: PathBuf, links: u64 },
    #[error("another cagents daemon owns {path:?}")]
    AlreadyRunning { path: PathBuf },
    #[error("refusing to replace non-socket runtime path {path:?}")]
    UnsafeSocketPath { path: PathBuf },
    #[error("failed to prepare cagents daemon runtime: {0}")]
    Io(#[from] io::Error),
    #[error("failed to process cagents daemon lock metadata: {0}")]
    LockMetadata(#[from] serde_json::Error),
}

#[derive(Clone, Debug)]
pub(crate) struct RuntimePaths {
    pub(crate) directory: PathBuf,
    pub(crate) socket: PathBuf,
    pub(crate) lock: PathBuf,
}

impl RuntimePaths {
    pub(crate) fn discover() -> Result<Self, PlatformError> {
        let base = env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
            .ok_or(PlatformError::MissingRuntimeDirectory)?;
        let directory = base.join(instance::RUNTIME_SUBDIRECTORY);
        Ok(Self {
            socket: directory.join(SOCKET_FILE_NAME),
            lock: directory.join("cagentsd.lock"),
            directory,
        })
    }
}

/// Serializes recovery discovery through confirmed attachment across all UIs.
/// The file is never unlinked; only its owning guard releases the flock.
#[cfg(feature = "ui")]
pub(crate) struct RecoveryLease {
    lock: File,
}

#[cfg(feature = "ui")]
impl RecoveryLease {
    pub(crate) fn acquire(
        socket: &Path,
        timeout: std::time::Duration,
    ) -> Result<Self, PlatformError> {
        let directory_path = socket.parent().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "daemon socket has no runtime directory",
            )
        })?;
        let directory = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | O_NOFOLLOW | libc::O_CLOEXEC)
            .open(directory_path)?;
        let metadata = directory.metadata()?;
        validate_owner(directory_path, &metadata, current_uid())?;
        if metadata.mode() & 0o022 != 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "daemon recovery directory is writable by another user",
            )
            .into());
        }
        let lock_path = directory_path.join("cagentsd.recovery.lock");
        // SAFETY: directory is a live directory descriptor and the fixed name
        // is NUL-terminated. Opening relative to it cannot follow a swapped path.
        let fd = unsafe {
            libc::openat(
                directory.as_raw_fd(),
                c"cagentsd.recovery.lock".as_ptr(),
                libc::O_CREAT | libc::O_RDWR | libc::O_CLOEXEC | LOCK_OPEN_FLAGS,
                0o600 as libc::mode_t,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error().into());
        }
        // SAFETY: openat returned a new descriptor, transferred exactly once.
        let lock = unsafe { File::from_raw_fd(fd) };
        let metadata = lock.metadata()?;
        validate_lock_file(&lock_path, &metadata, current_uid())?;
        if metadata.nlink() != 1 || metadata.mode() & 0o077 != 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "daemon recovery lock is not a private, singly linked file",
            )
            .into());
        }
        let started = std::time::Instant::now();
        loop {
            // SAFETY: flock receives a live descriptor and scalar flags.
            if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
                return Ok(Self { lock });
            }
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::WouldBlock
                && error.kind() != io::ErrorKind::Interrupted
            {
                return Err(error.into());
            }
            let remaining = timeout.saturating_sub(started.elapsed());
            if remaining.is_zero() {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "another terminal recovery is still awaiting attachment; retry after it completes",
                ).into());
            }
            std::thread::sleep(remaining.min(std::time::Duration::from_millis(20)));
        }
    }
}

#[cfg(feature = "ui")]
impl Drop for RecoveryLease {
    fn drop(&mut self) {
        // A concurrently spawned child can inherit this open-file description
        // until exec closes CLOEXEC descriptors. Explicit unlock prevents that
        // unrelated child from extending the recovery critical section.
        // SAFETY: this guard owns the live locked descriptor.
        unsafe { libc::flock(self.lock.as_raw_fd(), libc::LOCK_UN) };
    }
}

pub(crate) struct HostLease {
    socket_path: PathBuf,
    lock: File,
}

impl HostLease {
    #[cfg(feature = "host")]
    pub(crate) fn acquire(
        paths: &RuntimePaths,
        proof: &HostSessionProof,
    ) -> Result<Self, PlatformError> {
        prepare_runtime_directory(&paths.directory)?;

        let mut lock = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            // Do not erase a live daemon's metadata before the lock is acquired.
            // `write_lock_metadata` truncates after exclusive ownership is proven.
            .truncate(false)
            .mode(PRIVATE_FILE_MODE)
            .custom_flags(LOCK_OPEN_FLAGS)
            .open(&paths.lock)?;
        validate_lock_file(&paths.lock, &lock.metadata()?, current_uid())?;
        lock.set_permissions(fs::Permissions::from_mode(PRIVATE_FILE_MODE))?;
        // SAFETY: flock receives a live file descriptor and no pointer data.
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err(PlatformError::AlreadyRunning {
                path: paths.socket.clone(),
            });
        }

        recover_stale_socket(&paths.socket)?;
        write_lock_metadata(&mut lock, proof)?;
        Ok(Self {
            socket_path: paths.socket.clone(),
            lock,
        })
    }

    /// A closed listening socket is not proof that process shutdown completed.
    /// The inventory lease remains held until writers and the checkpoint drain.
    pub(crate) fn is_held(paths: &RuntimePaths) -> Result<bool, PlatformError> {
        Self::is_lock_held(&paths.lock)
    }

    pub(crate) fn is_held_at(socket: &Path) -> Result<bool, PlatformError> {
        Self::is_lock_held(&socket.with_file_name("cagentsd.lock"))
    }

    fn is_lock_held(path: &Path) -> Result<bool, PlatformError> {
        let lock = match OpenOptions::new()
            .read(true)
            .custom_flags(LOCK_OPEN_FLAGS)
            .open(path)
        {
            Ok(lock) => lock,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error.into()),
        };
        validate_lock_file(path, &lock.metadata()?, current_uid())?;
        // SAFETY: flock receives a live, validated descriptor and scalar flags.
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            let error = io::Error::last_os_error();
            return if error.kind() == io::ErrorKind::WouldBlock {
                Ok(true)
            } else {
                Err(error.into())
            };
        }
        // Explicit release also covers copies inherited by a concurrent fork.
        // SAFETY: this descriptor owns the exclusive lock acquired above.
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_UN) } != 0 {
            return Err(io::Error::last_os_error().into());
        }
        Ok(false)
    }

    /// Holds the daemon's exclusive inventory lease without claiming a serving
    /// incarnation. Used only by explicit checkpoint maintenance while stopped.
    pub(crate) fn acquire_for_maintenance(paths: &RuntimePaths) -> Result<Self, PlatformError> {
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(LOCK_OPEN_FLAGS)
            .open(&paths.lock)?;
        validate_lock_file(&paths.lock, &lock.metadata()?, current_uid())?;
        // SAFETY: the validated descriptor is owned through the guard lifetime.
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err(PlatformError::AlreadyRunning {
                path: paths.socket.clone(),
            });
        }
        recover_stale_socket(&paths.socket)?;
        Ok(Self {
            socket_path: paths.socket.clone(),
            lock,
        })
    }
}

impl Drop for HostLease {
    fn drop(&mut self) {
        match fs::remove_file(&self.socket_path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(_) => {}
        }
        // SAFETY: the file descriptor remains live for the duration of Drop.
        let _ = unsafe { libc::flock(self.lock.as_raw_fd(), libc::LOCK_UN) };
    }
}

#[cfg(feature = "host")]
#[derive(Serialize)]
struct LockMetadata<'a> {
    pid: u32,
    proof: &'a HostSessionProof,
}

#[cfg(feature = "host")]
fn write_lock_metadata(lock: &mut File, proof: &HostSessionProof) -> Result<(), PlatformError> {
    lock.set_len(0)?;
    lock.seek(SeekFrom::Start(0))?;
    serde_json::to_writer(
        &mut *lock,
        &LockMetadata {
            pid: std::process::id(),
            proof,
        },
    )?;
    lock.write_all(b"\n")?;
    lock.sync_data()?;
    Ok(())
}

#[derive(Deserialize)]
struct StoredLockMetadata {
    pid: u32,
}

/// Locate the executable of this instance's live, kernel-verified lock owner.
/// Update coordination uses its sibling CLI to speak the owner's own protocol.
pub(crate) fn running_executable() -> Result<Option<PathBuf>, PlatformError> {
    let paths = RuntimePaths::discover()?;
    let mut lock = match OpenOptions::new()
        .read(true)
        .custom_flags(LOCK_OPEN_FLAGS)
        .open(&paths.lock)
    {
        Ok(lock) => lock,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let Some(_pidfd) = daemon_pidfd_from_lock(&mut lock, &paths)? else {
        return Ok(None);
    };
    lock.seek(SeekFrom::Start(0))?;
    let stored: StoredLockMetadata = serde_json::from_reader(&mut lock)?;
    let executable = fs::read_link(format!("/proc/{}/exe", stored.pid))?;
    let _validated = open_validated_pidfd(&lock, stored.pid, Path::new("/proc"))?;
    Ok(Some(executable))
}

/// Authenticate the current socket against the daemon's exclusive lease before
/// using a new incarnation as evidence that an old owner has been replaced.
/// Merely reaching a socket (or receiving a proof mismatch) is not authority.
pub(crate) fn replacement_owner(
    socket: &Path,
    proof: &HostSessionProof,
) -> Result<u32, PlatformError> {
    let pid = lease_owner(&socket.with_file_name("cagentsd.lock"), proof)?;
    let stream = UnixStream::connect(socket)?;
    let mut credential: libc::ucred = unsafe { std::mem::zeroed() };
    let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: credential and length are writable buffers of the declared size.
    if unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut credential as *mut libc::ucred).cast(),
            &mut length,
        )
    } != 0
    {
        return Err(io::Error::last_os_error().into());
    }
    if credential.uid != current_uid() || u32::try_from(credential.pid).ok() != Some(pid) {
        return Err(io::Error::other("daemon socket does not belong to its lease owner").into());
    }
    Ok(pid)
}

/// Validate the exclusive owner before its public listener exists. Holders use
/// this during startup adoption, and compare it to the connecting peer's PID.
pub(crate) fn lease_owner(
    lock_path: &Path,
    proof: &HostSessionProof,
) -> Result<u32, PlatformError> {
    #[derive(Deserialize)]
    struct RecoveryMetadata {
        pid: u32,
        proof: HostSessionProof,
    }

    let lock = OpenOptions::new()
        .read(true)
        .custom_flags(LOCK_OPEN_FLAGS)
        .open(lock_path)?;
    validate_lock_file(lock_path, &lock.metadata()?, current_uid())?;
    // SAFETY: flock receives a live descriptor and no pointers.
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        // SAFETY: this descriptor owns the lock just acquired.
        let _ = unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_UN) };
        return Err(io::Error::other("daemon ownership is not held").into());
    }
    let error = io::Error::last_os_error();
    if error.kind() != io::ErrorKind::WouldBlock {
        return Err(error.into());
    }
    let metadata: RecoveryMetadata = serde_json::from_reader(&lock)?;
    if !proof.is_complete() || metadata.proof != *proof || read_boot_id()? != proof.boot_id {
        return Err(io::Error::other("daemon lease does not match its discovery proof").into());
    }
    let _pidfd = open_validated_pidfd(&lock, metadata.pid, Path::new("/proc"))?;
    Ok(metadata.pid)
}

/// Stops the daemon identified by the current instance's lock and waits for
/// that exact process, lease and listener to finish normal shutdown.
///
/// No protocol response is needed, so this remains usable across versions. The
/// original pidfd alone authorizes the one signal; replacement or uncertain
/// ownership is an error, never permission to signal another process.
pub(crate) fn terminate_daemon_from_lock() -> Result<bool, PlatformError> {
    terminate_daemon_at(
        &RuntimePaths::discover()?,
        std::time::Duration::from_secs(5),
    )
}

fn terminate_daemon_at(
    paths: &RuntimePaths,
    timeout: std::time::Duration,
) -> Result<bool, PlatformError> {
    let deadline = std::time::Instant::now() + timeout;
    let mut lock = match OpenOptions::new()
        .read(true)
        .custom_flags(LOCK_OPEN_FLAGS)
        .open(&paths.lock)
    {
        Ok(lock) => lock,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            recover_stale_socket(&paths.socket)?;
            return Ok(false);
        }
        Err(error) => return Err(error.into()),
    };
    validate_lock_file(&paths.lock, &lock.metadata()?, current_uid())?;
    // Preserve the existing stopped/stale case without parsing an unheld file.
    // SAFETY: this exclusively owned descriptor and scalar flags are valid.
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        let recovered = recover_unheld_stop_socket(&lock, paths);
        // SAFETY: this descriptor owns the lock acquired above.
        let released = unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_UN) };
        recovered?;
        if released != 0 {
            return Err(io::Error::last_os_error().into());
        }
        return Ok(false);
    }
    let error = io::Error::last_os_error();
    if error.kind() != io::ErrorKind::WouldBlock {
        return Err(error.into());
    }
    let metadata = lock.metadata()?;
    let identity = (metadata.dev(), metadata.ino());
    let original = stop_lock_contents(&mut lock)?;
    let stored: StoredLockMetadata = serde_json::from_slice(&original)?;
    let pidfd = open_validated_pidfd(&lock, stored.pid, Path::new("/proc"))?;
    verify_stop_lock(&mut lock, paths, identity, &original)?;
    // Acquiring this lock would mean the observed owner stopped owning it
    // before authorization completed. Do not send even the original signal.
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        let _ = unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_UN) };
        return Err(io::Error::other("daemon ownership changed before stop").into());
    }
    let error = io::Error::last_os_error();
    if error.kind() != io::ErrorKind::WouldBlock {
        return Err(error.into());
    }
    if std::time::Instant::now() >= deadline {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "daemon stop authorization timed out",
        )
        .into());
    }
    // SAFETY: the original pidfd was opened before ownership validation. This
    // is the only signal and never retargets a PID or a replacement lock owner.
    let result = unsafe {
        libc::syscall(
            libc::SYS_pidfd_send_signal,
            pidfd.as_raw_fd(),
            libc::SIGTERM,
            std::ptr::null::<libc::siginfo_t>(),
            0,
        )
    };
    if result == -1 {
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::ESRCH) {
            return Err(error.into());
        }
    }
    if !wait_process(
        &pidfd,
        deadline.saturating_duration_since(std::time::Instant::now()),
    )? {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "the original daemon is still stopping",
        )
        .into());
    }
    // Process exit does not authorize recovering another listener or reporting
    // a replacement daemon stopped. Keep the exact original lock throughout.
    loop {
        if std::time::Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "daemon shutdown observation timed out",
            )
            .into());
        }
        verify_stop_lock(&mut lock, paths, identity, &original)?;
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            let completed = (|| {
                verify_stop_lock(&mut lock, paths, identity, &original)?;
                match fs::symlink_metadata(&paths.socket) {
                    Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
                    Err(error) => Err(PlatformError::Io(error)),
                    Ok(_) => Err(io::Error::other(
                        "the original daemon exited but a listener path remains",
                    )
                    .into()),
                }
            })();
            // Explicit unlock also covers a copy inherited by concurrent fork.
            let released = unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_UN) };
            completed?;
            if released != 0 {
                return Err(io::Error::last_os_error().into());
            }
            if std::time::Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "daemon shutdown observation timed out",
                )
                .into());
            }
            return Ok(true);
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::WouldBlock {
            return Err(error.into());
        }
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "the original daemon's lease is still held",
            )
            .into());
        }
        std::thread::sleep(remaining.min(std::time::Duration::from_millis(5)));
    }
}

// Called only while this descriptor owns the exclusive lease. An unheld corrupt
// file remains recoverable, but a replacement pathname never inherits that lease.
fn recover_unheld_stop_socket(lock: &File, paths: &RuntimePaths) -> Result<(), PlatformError> {
    let held = lock.metadata()?;
    verify_stop_lock_path(lock, paths, (held.dev(), held.ino()))?;
    recover_stale_socket(&paths.socket)
}

fn verify_stop_lock_path(
    lock: &File,
    paths: &RuntimePaths,
    identity: (u64, u64),
) -> Result<(), PlatformError> {
    let held = lock.metadata()?;
    let named = fs::symlink_metadata(&paths.lock)?;
    validate_lock_file(&paths.lock, &held, current_uid())?;
    validate_lock_file(&paths.lock, &named, current_uid())?;
    if (held.dev(), held.ino()) != identity || (named.dev(), named.ino()) != identity {
        return Err(io::Error::other(
            "daemon ownership changed while stopping; no replacement was stopped",
        )
        .into());
    }
    Ok(())
}

fn stop_lock_contents(lock: &mut File) -> Result<Vec<u8>, PlatformError> {
    lock.seek(SeekFrom::Start(0))?;
    let mut contents = Vec::new();
    // Lock metadata is a small PID/proof record. Bound the observation before
    // granting process authority, including malformed or concurrently grown data.
    std::io::Read::read_to_end(&mut std::io::Read::take(&mut *lock, 65_537), &mut contents)?;
    if contents.len() > 65_536 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "daemon lock metadata exceeds 64 KiB",
        )
        .into());
    }
    Ok(contents)
}

fn verify_stop_lock(
    lock: &mut File,
    paths: &RuntimePaths,
    identity: (u64, u64),
    original: &[u8],
) -> Result<(), PlatformError> {
    verify_stop_lock_path(lock, paths, identity)?;
    if stop_lock_contents(lock)? != original {
        return Err(io::Error::other(
            "daemon ownership changed while stopping; no replacement was stopped",
        )
        .into());
    }
    Ok(())
}

/// A confirmation pins the process before it is displayed. A successor cannot
/// inherit the person's approval merely by acquiring the same lock pathname.
#[cfg(feature = "ui")]
#[derive(Debug)]
pub(crate) struct RestartTarget {
    process: File,
    proof: HostSessionProof,
}

#[cfg(feature = "ui")]
pub(crate) fn restart_target() -> Result<RestartTarget, PlatformError> {
    restart_target_at(&RuntimePaths::discover()?)
}

#[cfg(feature = "ui")]
fn restart_target_at(paths: &RuntimePaths) -> Result<RestartTarget, PlatformError> {
    #[derive(Deserialize)]
    struct Metadata {
        pid: u32,
        proof: HostSessionProof,
    }
    let mut lock = OpenOptions::new()
        .read(true)
        .custom_flags(LOCK_OPEN_FLAGS)
        .open(&paths.lock)?;
    let _discovered = daemon_pidfd_from_lock(&mut lock, paths)?
        .ok_or_else(|| io::Error::other("the daemon has already stopped"))?;
    lock.seek(SeekFrom::Start(0))?;
    let metadata: Metadata = serde_json::from_reader(&mut lock)?;
    let process = open_validated_pidfd(&lock, metadata.pid, Path::new("/proc"))?;
    if lease_owner(&paths.lock, &metadata.proof)? != metadata.pid {
        return Err(io::Error::other("daemon ownership changed while opening confirmation").into());
    }
    Ok(RestartTarget {
        process,
        proof: metadata.proof,
    })
}

#[cfg(feature = "ui")]
impl RestartTarget {
    /// Try graceful termination, then finish the same confirmed process if it
    /// cannot respond. The kernel descriptor fences PID reuse throughout.
    pub(crate) fn terminate(&self) -> Result<(), PlatformError> {
        self.terminate_at(
            &RuntimePaths::discover()?,
            std::time::Duration::from_secs(5),
        )
    }
    fn terminate_at(
        &self,
        paths: &RuntimePaths,
        grace: std::time::Duration,
    ) -> Result<(), PlatformError> {
        lease_owner(&paths.lock, &self.proof)?;
        signal_process(&self.process, libc::SIGTERM)?;
        if !wait_process(&self.process, grace)? {
            // If ownership changed during the wait, do not kill anything on
            // behalf of an obsolete confirmation.
            lease_owner(&paths.lock, &self.proof)?;
            signal_process(&self.process, libc::SIGKILL)?;
            if !wait_process(&self.process, std::time::Duration::from_secs(5))? {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "the confirmed daemon did not exit",
                )
                .into());
            }
        }
        Ok(())
    }
}

#[cfg(feature = "ui")]
fn signal_process(process: &File, signal: c_int) -> io::Result<()> {
    // SAFETY: the owned descriptor is a validated pidfd; the caller supplies
    // SIGTERM or SIGKILL. No PID lookup occurs after authentication.
    if unsafe {
        libc::syscall(
            libc::SYS_pidfd_send_signal,
            process.as_raw_fd(),
            signal,
            std::ptr::null::<libc::siginfo_t>(),
            0,
        )
    } == -1
    {
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::ESRCH) {
            return Err(error);
        }
    }
    Ok(())
}

fn wait_process(process: &File, duration: std::time::Duration) -> io::Result<bool> {
    let deadline = std::time::Instant::now() + duration;
    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        let mut poll = libc::pollfd {
            fd: process.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: poll is a live single-element writable array; a pidfd becomes
        // readable after its pinned process exits.
        let result = unsafe {
            libc::poll(
                &mut poll,
                1,
                remaining.as_millis().min(i32::MAX as u128) as i32,
            )
        };
        if result > 0 {
            return Ok(poll.revents & libc::POLLIN != 0);
        }
        if result == 0 {
            return Ok(false);
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

fn daemon_pidfd_from_lock(
    lock: &mut File,
    paths: &RuntimePaths,
) -> Result<Option<File>, PlatformError> {
    validate_lock_file(&paths.lock, &lock.metadata()?, current_uid())?;
    // Check ownership before trusting metadata: a crashed writer can leave a
    // truncated lock file, but an unheld flock proves no daemon can be using it.
    // SAFETY: lock owns a live descriptor and flock takes scalar values.
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        let recovery = recover_stale_socket(&paths.socket);
        // SAFETY: this descriptor owns the lock acquired above.
        let _ = unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_UN) };
        recovery?;
        return Ok(None);
    }
    let lock_error = io::Error::last_os_error();
    if lock_error.kind() != io::ErrorKind::WouldBlock {
        return Err(lock_error.into());
    }
    lock.seek(SeekFrom::Start(0))?;
    let stored: StoredLockMetadata = serde_json::from_reader(&mut *lock)?;
    open_daemon_pidfd_or_recover(lock, paths, stored.pid)
}

fn open_daemon_pidfd_or_recover(
    lock: &File,
    paths: &RuntimePaths,
    pid: u32,
) -> Result<Option<File>, PlatformError> {
    match open_validated_pidfd(lock, pid, Path::new("/proc")) {
        Ok(pidfd) => Ok(Some(pidfd)),
        Err(validation_error) => {
            // A dead daemon can leave both metadata files behind. Only remove
            // its socket after taking the same exclusive lock a live daemon
            // must hold; otherwise preserve the validation failure.
            // SAFETY: lock owns a live descriptor and flock takes scalar values.
            if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
                return Err(validation_error.into());
            }
            recover_stale_socket(&paths.socket)?;
            // SAFETY: this descriptor still owns the lock acquired above.
            let _ = unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_UN) };
            Ok(None)
        }
    }
}

fn open_validated_pidfd(lock: &File, pid: u32, proc_root: &Path) -> io::Result<File> {
    let pid = i32::try_from(pid)
        .ok()
        .filter(|pid| *pid > 1)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid daemon PID"))?;
    // SAFETY: pidfd_open takes scalar arguments and returns a new owned file
    // descriptor or -1 without writing through any pointer.
    let descriptor = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
    if descriptor == -1 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a successful pidfd_open returns a fresh descriptor owned here.
    let pidfd = unsafe { File::from_raw_fd(descriptor as c_int) };

    let process_path = proc_root.join(pid.to_string());
    validate_owner(
        &process_path,
        &fs::symlink_metadata(&process_path)?,
        current_uid(),
    )
    .map_err(io::Error::other)?;
    let expected = lock.metadata()?;
    let owns_lock = fs::read_dir(process_path.join("fd"))?
        .filter_map(Result::ok)
        .filter_map(|entry| fs::metadata(entry.path()).ok())
        .any(|metadata| metadata.dev() == expected.dev() && metadata.ino() == expected.ino());
    if !owns_lock {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "daemon PID does not own the validated lock file",
        ));
    }
    Ok(pidfd)
}

#[cfg(feature = "host")]
fn prepare_runtime_directory(path: &Path) -> Result<(), PlatformError> {
    match fs::symlink_metadata(path) {
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let mut builder = DirBuilder::new();
            builder.mode(PRIVATE_DIRECTORY_MODE).create(path)?;
        }
        Err(error) => return Err(error.into()),
    }

    let metadata = fs::symlink_metadata(path)?;
    let effective_uid = current_uid();
    validate_runtime_directory(path, &metadata, effective_uid)?;
    fs::set_permissions(path, fs::Permissions::from_mode(PRIVATE_DIRECTORY_MODE))?;
    validate_runtime_directory(path, &fs::symlink_metadata(path)?, effective_uid)
}

#[cfg(feature = "host")]
fn validate_runtime_directory(
    path: &Path,
    metadata: &fs::Metadata,
    effective_uid: u32,
) -> Result<(), PlatformError> {
    if !metadata.is_dir() {
        return Err(PlatformError::UnsafeRuntimeDirectory {
            path: path.to_owned(),
        });
    }
    validate_owner(path, metadata, effective_uid)
}

fn validate_lock_file(
    path: &Path,
    metadata: &fs::Metadata,
    effective_uid: u32,
) -> Result<(), PlatformError> {
    if !metadata.is_file() {
        return Err(PlatformError::UnsafeLockPath {
            path: path.to_owned(),
        });
    }
    validate_owner(path, metadata, effective_uid)?;
    if metadata.nlink() != 1 {
        return Err(PlatformError::UnsafeLockLinkCount {
            path: path.to_owned(),
            links: metadata.nlink(),
        });
    }
    Ok(())
}

fn validate_owner(
    path: &Path,
    metadata: &fs::Metadata,
    effective_uid: u32,
) -> Result<(), PlatformError> {
    if metadata.uid() != effective_uid {
        return Err(PlatformError::WrongRuntimeOwner {
            path: path.to_owned(),
        });
    }
    Ok(())
}

fn recover_stale_socket(path: &Path) -> Result<(), PlatformError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    if !metadata.file_type().is_socket() {
        return Err(PlatformError::UnsafeSocketPath {
            path: path.to_owned(),
        });
    }
    if UnixStream::connect(path).is_ok() {
        return Err(PlatformError::AlreadyRunning {
            path: path.to_owned(),
        });
    }
    fs::remove_file(path)?;
    Ok(())
}

pub(crate) fn current_uid() -> u32 {
    // SAFETY: geteuid has no preconditions and no failure state.
    unsafe { libc::geteuid() }
}

/// Called only in a child's pre-exec hook. Long-lived sibling processes must
/// not retain installer assets, update leases, or other inherited descriptors.
pub(crate) fn seal_inherited_descriptors() -> io::Result<()> {
    // SAFETY: close_range takes scalar bounds and only changes descriptor flags.
    if unsafe { libc::close_range(3, u32::MAX, libc::CLOSE_RANGE_CLOEXEC as libc::c_int) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(feature = "host")]
pub(crate) fn peer_uid(stream: &UnixStream) -> io::Result<u32> {
    // SAFETY: zero is valid for every scalar field of the kernel credential buffer.
    let mut credentials: libc::ucred = unsafe { std::mem::zeroed() };
    let mut length = libc::socklen_t::try_from(std::mem::size_of::<libc::ucred>())
        .expect("Linux ucred size must fit socklen_t");
    // SAFETY: credentials and length are valid writable objects of the sizes
    // passed to getsockopt, and stream owns a live socket descriptor.
    let result = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut credentials as *mut libc::ucred).cast(),
            &mut length,
        )
    };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    if usize::try_from(length).ok() != Some(std::mem::size_of::<libc::ucred>()) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "kernel returned an invalid SO_PEERCRED length",
        ));
    }
    Ok(credentials.uid)
}

pub(crate) fn read_boot_id() -> io::Result<BootId> {
    let value = fs::read_to_string("/proc/sys/kernel/random/boot_id")?;
    let value = value.trim();
    if value.is_empty() || value.len() > 128 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "kernel boot ID is empty or implausibly long",
        ));
    }
    Ok(BootId::new(value))
}

#[cfg(feature = "host")]
extern "C" fn request_termination(_signal: c_int) {
    TERMINATION_REQUESTED.store(true, Ordering::Release);
    let fd = WAKEUP_FD.load(Ordering::Acquire);
    if fd < 0 {
        return;
    }
    let byte = 0_u8;
    // SAFETY: write is async-signal-safe, the buffer is one live byte, and the
    // descriptor belongs to a static that is never dropped, so it can neither be
    // closed nor recycled. Writing here rather than relying on EINTR is what
    // closes the race where the signal arrives after the accept loop has read
    // the termination flag but before it blocks in poll.
    unsafe {
        let _ = libc::write(fd, (&raw const byte).cast(), 1);
    }
}

#[cfg(feature = "host")]
pub(crate) fn install_signal_handlers() -> io::Result<()> {
    // The handler writes to the wakeup pipe, so the pipe must exist first.
    wakeup()?;
    // SAFETY: request_termination is async-signal-safe because it only stores
    // into lock-free atomics and writes a byte to a descriptor that outlives the
    // process, and both constants are valid POSIX signals.
    unsafe {
        libc::signal(
            libc::SIGINT,
            request_termination as *const () as libc::sighandler_t,
        );
        libc::signal(
            libc::SIGTERM,
            request_termination as *const () as libc::sighandler_t,
        );
    }
    Ok(())
}

#[cfg(feature = "host")]
pub(crate) fn termination_requested() -> bool {
    TERMINATION_REQUESTED.load(Ordering::Acquire)
}

#[cfg(test)]
mod tests {
    #[cfg(feature = "host")]
    use std::os::unix::fs::symlink;
    use std::os::unix::{fs::PermissionsExt, net::UnixListener};

    use tempfile::tempdir;

    use super::*;
    use crate::model::{HostIncarnation, HostSessionId};

    #[cfg(feature = "host")]
    #[test]
    fn socket_peer_identity_uses_the_kernel_effective_uid() {
        let (client, server) = UnixStream::pair().unwrap();
        // SAFETY: geteuid has no preconditions. Both sockets are real owned
        // descriptors, so this exercises the credential buffer and length ABI.
        let expected = unsafe { libc::geteuid() };
        assert_eq!(peer_uid(&client).unwrap(), expected);
        assert_eq!(peer_uid(&server).unwrap(), expected);
    }

    #[cfg(feature = "host")]
    #[test]
    fn local_wakeup_interrupts_idle_poll_drains_and_accepts_socket_activity() {
        use std::{
            sync::{Arc, mpsc},
            time::Duration,
        };
        let directory = tempdir().unwrap();
        let path = directory.path().join("activity.sock");
        let listener = UnixListener::bind(&path).unwrap();
        // Unlike the global signal channel, this local test channel never
        // publishes a descriptor to a handler or mutates process signal state.
        let channel = Arc::new(Wakeup::channel().unwrap());
        let owned_channel = channel.clone();
        let (started_tx, started_rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            started_tx.send(()).unwrap();
            let result = wait_for_activity_with(&listener, &owned_channel);
            done_tx.send(result).unwrap();
            listener
        });
        started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(
            matches!(
                done_rx.recv_timeout(Duration::from_millis(20)),
                Err(mpsc::RecvTimeoutError::Timeout)
            ),
            "an idle listener must wait for a real wakeup or socket activity"
        );
        channel.wake();
        done_rx
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .unwrap();
        let listener = worker.join().unwrap();
        let mut descriptor = libc::pollfd {
            fd: channel.reader.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: this live descriptor is owned by channel throughout poll.
        assert_eq!(
            unsafe { libc::poll(&mut descriptor, 1, 0) },
            0,
            "the delivered wakeup must be drained rather than causing idle spin"
        );
        let _client = UnixStream::connect(&path).unwrap();
        wait_for_activity_with(&listener, &channel).unwrap();
        let _accepted = listener.accept().unwrap();
    }

    fn proof() -> HostSessionProof {
        HostSessionProof {
            host_session_id: HostSessionId::new("session"),
            boot_id: BootId::new("boot"),
            host_incarnation: HostIncarnation::new("incarnation"),
        }
    }

    fn paths(directory: PathBuf) -> RuntimePaths {
        RuntimePaths {
            socket: directory.join(SOCKET_FILE_NAME),
            lock: directory.join("cagentsd.lock"),
            directory,
        }
    }

    #[test]
    fn special_daemon_locks_are_rejected_before_blocking_open() {
        use std::os::unix::{ffi::OsStrExt, fs::FileTypeExt};
        use std::process::{Command, Stdio};
        use std::time::{Duration, Instant};

        const CASE: &str = "CAGENTS_DAEMON_FIFO_CHECK";
        const TEST: &str = "daemon::tests::special_daemon_locks_are_rejected_before_blocking_open";
        if let Ok(case) = std::env::var(CASE) {
            let paths = RuntimePaths::discover().unwrap();
            assert!(
                fs::symlink_metadata(&paths.lock)
                    .unwrap()
                    .file_type()
                    .is_fifo()
            );
            eprintln!("owned daemon FIFO validation case={case}");
            let result = match case.as_str() {
                "held" => HostLease::is_held(&paths).map(|_| ()),
                "executable" => running_executable().map(|_| ()),
                "owner" => lease_owner(&paths.lock, &proof()).map(|_| ()),
                "terminate" => terminate_daemon_from_lock().map(|_| ()),
                #[cfg(feature = "host")]
                "acquire" => HostLease::acquire(&paths, &proof()).map(|_| ()),
                #[cfg(feature = "host")]
                "maintenance" => HostLease::acquire_for_maintenance(&paths).map(|_| ()),
                #[cfg(feature = "ui")]
                "recovery" => {
                    RecoveryLease::acquire(&paths.socket, Duration::from_millis(50)).map(|_| ())
                }
                _ => panic!("unknown daemon lock fixture case"),
            };
            let expected = if case == "recovery" {
                paths.lock.with_file_name("cagentsd.recovery.lock")
            } else {
                paths.lock
            };
            assert!(
                matches!(result, Err(PlatformError::UnsafeLockPath { path }) if path == expected)
            );
            return;
        }

        // XDG_RUNTIME_DIR is process-global. Each real API runs in an exact
        // subprocess, where a blocking regression is bounded and cleaned up
        // through its original pidfd rather than stranding a libtest thread.
        let runtime = tempdir().unwrap();
        let directory = runtime.path().join(instance::RUNTIME_SUBDIRECTORY);
        fs::create_dir(&directory).unwrap();
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
        for name in ["cagentsd.lock", "cagentsd.recovery.lock"] {
            let path = directory.join(name);
            let name = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
            assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        }
        for case in [
            "held",
            "executable",
            "owner",
            "terminate",
            "acquire",
            "maintenance",
            "recovery",
        ]
        .into_iter()
        .filter(|case| match *case {
            "acquire" | "maintenance" => cfg!(feature = "host"),
            "recovery" => cfg!(feature = "ui"),
            _ => true,
        }) {
            let mut child = Command::new(std::env::current_exe().unwrap())
                .args(["--exact", TEST, "--nocapture", "--test-threads=1"])
                .env(CASE, case)
                .env("XDG_RUNTIME_DIR", runtime.path())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, child.id(), 0) };
            if raw < 0 {
                let error = io::Error::last_os_error();
                // Still the sole owner of this fresh, unreaped fixture child.
                let _ = child.kill();
                let _ = child.wait();
                panic!("cannot capture fixture process identity: {error}");
            }
            let identity = unsafe { File::from_raw_fd(raw as i32) };
            let deadline = Instant::now() + Duration::from_secs(2);
            let mut timed_out = false;
            while child.try_wait().unwrap().is_none() {
                if Instant::now() >= deadline {
                    timed_out = true;
                    assert_eq!(
                        unsafe {
                            libc::syscall(
                                libc::SYS_pidfd_send_signal,
                                identity.as_raw_fd(),
                                libc::SIGKILL,
                                std::ptr::null::<libc::siginfo_t>(),
                                0,
                            )
                        },
                        0
                    );
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            let output = child.wait_with_output().unwrap();
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(
                stderr.contains(&format!("owned daemon FIFO validation case={case}")),
                "exact fixture must run: {stdout}; {stderr}"
            );
            assert!(
                !timed_out && output.status.success(),
                "{case}: {stdout}; {stderr}"
            );
            assert!(
                fs::symlink_metadata(directory.join("cagentsd.lock"))
                    .unwrap()
                    .file_type()
                    .is_fifo()
            );
            assert!(
                fs::symlink_metadata(directory.join("cagentsd.recovery.lock"))
                    .unwrap()
                    .file_type()
                    .is_fifo()
            );
        }
    }

    #[test]
    fn child_exec_seals_inherited_assets_without_closing_standard_output() {
        use std::os::unix::process::CommandExt;

        let asset = tempfile::tempfile().unwrap();
        let fd = asset.as_raw_fd();
        let mut child = std::process::Command::new("/bin/sh");
        child.args([
            "-c",
            "if [ -e \"/proc/self/fd/$1\" ]; then exit 42; fi; test -z \"${CAGENTS_UI_UPDATE_GUARD_FD+x}${CAGENTS_INSTALL_RESULT_FD+x}${CAGENTS_INSTALL_OWNER_CLI+x}${CAGENTS_ARCHIVE+x}${CAGENTS_CHECKSUMS+x}${CAGENTS_REPOSITORY+x}\" || exit 43; printf ready",
            "descriptor-probe",
            &fd.to_string(),
        ]);
        for name in [
            crate::instance::INSTALL_GUARD_ENV,
            "CAGENTS_INSTALL_RESULT_FD",
            "CAGENTS_INSTALL_OWNER_CLI",
            "CAGENTS_ARCHIVE",
            "CAGENTS_CHECKSUMS",
            "CAGENTS_REPOSITORY",
        ] {
            child.env(name, "installer-only");
        }
        crate::install_guard::sanitize_child_environment(&mut child);
        // SAFETY: this child-only hook calls async-signal-safe descriptor syscalls.
        // Clearing CLOEXEC models installer assets intentionally passed to a worker.
        unsafe {
            child.pre_exec(move || {
                if libc::fcntl(fd, libc::F_SETFD, 0) == -1 {
                    return Err(io::Error::last_os_error());
                }
                seal_inherited_descriptors()
            });
        }
        let output = child.output().unwrap();
        assert!(output.status.success(), "{output:?}");
        assert_eq!(output.stdout, b"ready");
    }

    #[cfg(feature = "host")]
    fn mode(path: &Path) -> u32 {
        fs::symlink_metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[cfg(feature = "host")]
    #[test]
    fn runtime_directory_symlink_fails_closed_without_touching_target() {
        let root = tempdir().unwrap();
        let target = root.path().join("target");
        fs::create_dir(&target).unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o755)).unwrap();
        let marker = target.join("marker");
        fs::write(&marker, b"untouched").unwrap();
        let directory = root.path().join("runtime");
        symlink(&target, &directory).unwrap();

        let result = HostLease::acquire(&paths(directory.clone()), &proof());

        assert!(matches!(
            result,
            Err(PlatformError::UnsafeRuntimeDirectory { path }) if path == directory
        ));
        assert_eq!(fs::read(&marker).unwrap(), b"untouched");
        assert_eq!(mode(&target), 0o755);
        assert!(!target.join("cagentsd.lock").exists());
    }

    #[cfg(feature = "host")]
    #[test]
    fn lock_symlink_fails_closed_without_touching_target() {
        let root = tempdir().unwrap();
        let directory = root.path().join("runtime");
        fs::create_dir(&directory).unwrap();
        let target = root.path().join("target");
        fs::write(&target, b"untouched").unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o644)).unwrap();
        symlink(&target, directory.join("cagentsd.lock")).unwrap();

        let result = HostLease::acquire(&paths(directory), &proof());

        assert!(result.is_err());
        assert_eq!(fs::read(&target).unwrap(), b"untouched");
        assert_eq!(mode(&target), 0o644);
    }

    fn hardlinked_lock_fixture() -> (tempfile::TempDir, RuntimePaths, PathBuf) {
        let root = tempdir().unwrap();
        let directory = root.path().join("runtime");
        fs::create_dir(&directory).unwrap();
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
        let target = root.path().join("owned-existing-file.txt");
        fs::write(&target, b"existing owned content must remain intact\n").unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o640)).unwrap();
        let paths = paths(directory);
        fs::hard_link(&target, &paths.lock).unwrap();
        assert_eq!(fs::metadata(&target).unwrap().nlink(), 2);
        assert_eq!(
            fs::metadata(&target).unwrap().ino(),
            fs::metadata(&paths.lock).unwrap().ino()
        );
        (root, paths, target)
    }

    #[cfg(feature = "host")]
    #[test]
    fn hardlinked_daemon_lock_is_refused_before_rewriting_the_other_name() {
        let (_root, paths, target) = hardlinked_lock_fixture();
        let before = fs::metadata(&target).unwrap();
        let contents = fs::read(&target).unwrap();

        assert!(matches!(
            HostLease::acquire(&paths, &proof()),
            Err(PlatformError::UnsafeLockLinkCount { path, links: 2 }) if path == paths.lock
        ));

        let after = fs::metadata(&target).unwrap();
        assert_eq!(fs::read(&target).unwrap(), contents);
        assert_eq!((after.dev(), after.ino()), (before.dev(), before.ino()));
        assert_eq!(after.mode() & 0o777, 0o640);
        assert_eq!(after.nlink(), 2);
    }

    #[test]
    fn hardlinked_lock_read_paths_refuse_without_recovering_the_socket() {
        let (_root, paths, target) = hardlinked_lock_fixture();
        let before = fs::metadata(&target).unwrap();
        let contents = fs::read(&target).unwrap();
        leave_stale_socket(&paths.socket);

        assert!(matches!(
            HostLease::is_held(&paths),
            Err(PlatformError::UnsafeLockLinkCount { path, links: 2 }) if path == paths.lock
        ));
        assert!(matches!(
            lease_owner(&paths.lock, &proof()),
            Err(PlatformError::UnsafeLockLinkCount { path, links: 2 }) if path == paths.lock
        ));
        let mut lock = File::open(&paths.lock).unwrap();
        assert!(matches!(
            daemon_pidfd_from_lock(&mut lock, &paths),
            Err(PlatformError::UnsafeLockLinkCount { path, links: 2 }) if path == paths.lock
        ));

        assert!(paths.socket.exists());
        let after = fs::metadata(&target).unwrap();
        assert_eq!(fs::read(&target).unwrap(), contents);
        assert_eq!((after.dev(), after.ino()), (before.dev(), before.ino()));
        assert_eq!(after.mode() & 0o777, 0o640);
        assert_eq!(after.nlink(), 2);
    }

    #[cfg(feature = "host")]
    #[test]
    fn new_runtime_and_lock_paths_are_private() {
        let root = tempdir().unwrap();
        let paths = paths(root.path().join("runtime"));

        let lease = HostLease::acquire(&paths, &proof()).unwrap();

        assert_eq!(mode(&paths.directory), PRIVATE_DIRECTORY_MODE);
        assert_eq!(mode(&paths.lock), PRIVATE_FILE_MODE);
        assert!(fs::symlink_metadata(&paths.lock).unwrap().is_file());
        drop(lease);
    }

    #[cfg(feature = "host")]
    #[test]
    fn shutdown_remains_running_after_the_listening_socket_closes() {
        let root = tempdir().unwrap();
        let paths = paths(root.path().join("runtime"));
        let lease = HostLease::acquire(&paths, &proof()).unwrap();
        let listener = UnixListener::bind(&paths.socket).unwrap();
        drop(listener);
        fs::remove_file(&paths.socket).unwrap();

        assert!(HostLease::is_held(&paths).unwrap());
        drop(lease);
        assert!(!HostLease::is_held(&paths).unwrap());
    }

    #[cfg(feature = "host")]
    #[test]
    fn existing_runtime_and_lock_paths_are_made_private() {
        let root = tempdir().unwrap();
        let directory = root.path().join("runtime");
        fs::create_dir(&directory).unwrap();
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o755)).unwrap();
        let paths = paths(directory);
        fs::write(&paths.lock, b"stale metadata").unwrap();
        fs::set_permissions(&paths.lock, fs::Permissions::from_mode(0o644)).unwrap();

        let lease = HostLease::acquire(&paths, &proof()).unwrap();

        assert_eq!(mode(&paths.directory), PRIVATE_DIRECTORY_MODE);
        assert_eq!(mode(&paths.lock), PRIVATE_FILE_MODE);
        drop(lease);
    }

    #[test]
    fn validated_pidfd_pins_the_process_that_owns_the_lock() {
        let root = tempdir().unwrap();
        let lock = File::create(root.path().join("lock")).unwrap();

        let pidfd = open_validated_pidfd(&lock, std::process::id(), Path::new("/proc")).unwrap();

        assert!(pidfd.metadata().is_ok());
    }

    fn leave_stale_socket(path: &Path) {
        let listener = UnixListener::bind(path).unwrap();
        // Concurrent tests may fork before CLOEXEC closes an inherited copy.
        // Shut down the shared socket so dropping this owner is deterministic.
        assert_eq!(
            unsafe { libc::shutdown(listener.as_raw_fd(), libc::SHUT_RDWR) },
            0
        );
    }

    #[test]
    fn dead_daemon_lock_recovers_its_stale_socket() {
        let root = tempdir().unwrap();
        let paths = paths(root.path().to_path_buf());
        leave_stale_socket(&paths.socket);
        let lock = File::create(&paths.lock).unwrap();

        let pidfd = open_daemon_pidfd_or_recover(&lock, &paths, u32::MAX).unwrap();

        assert!(pidfd.is_none());
        assert!(!paths.socket.exists());
    }

    #[test]
    fn unheld_corrupt_lock_recovers_without_parsing_metadata() {
        let root = tempdir().unwrap();
        let paths = paths(root.path().to_path_buf());
        leave_stale_socket(&paths.socket);
        fs::write(&paths.lock, b"truncated").unwrap();
        fs::set_permissions(&paths.lock, fs::Permissions::from_mode(PRIVATE_FILE_MODE)).unwrap();
        let mut lock = File::open(&paths.lock).unwrap();

        let pidfd = daemon_pidfd_from_lock(&mut lock, &paths).unwrap();

        assert!(pidfd.is_none());
        assert!(!paths.socket.exists());
    }

    #[cfg(feature = "host")]
    #[test]
    fn non_directory_runtime_path_and_mismatched_owners_are_rejected() {
        let root = tempdir().unwrap();
        let not_directory = root.path().join("runtime");
        fs::write(&not_directory, b"untouched").unwrap();
        let metadata = fs::symlink_metadata(&not_directory).unwrap();

        assert!(matches!(
            validate_runtime_directory(&not_directory, &metadata, current_uid()),
            Err(PlatformError::UnsafeRuntimeDirectory { .. })
        ));

        let directory = root.path().join("directory");
        fs::create_dir(&directory).unwrap();
        let directory_metadata = fs::symlink_metadata(&directory).unwrap();
        let wrong_uid = current_uid().wrapping_add(1);
        assert!(matches!(
            validate_runtime_directory(&directory, &directory_metadata, wrong_uid),
            Err(PlatformError::WrongRuntimeOwner { .. })
        ));

        assert!(matches!(
            validate_lock_file(&directory, &directory_metadata, current_uid()),
            Err(PlatformError::UnsafeLockPath { .. })
        ));

        let lock = root.path().join("lock");
        fs::write(&lock, b"untouched").unwrap();
        let lock_metadata = fs::symlink_metadata(&lock).unwrap();
        assert!(matches!(
            validate_lock_file(&lock, &lock_metadata, wrong_uid),
            Err(PlatformError::WrongRuntimeOwner { .. })
        ));
        assert_eq!(fs::read(&not_directory).unwrap(), b"untouched");
        assert_eq!(fs::read(&lock).unwrap(), b"untouched");
    }

    /// A private kernel lease/listener fixture, not a provider or a substitute
    /// daemon protocol. All children remain direct children of this test.
    struct StopFixture {
        child: std::process::Child,
        input: Option<std::process::ChildStdin>,
        output: Option<std::process::ChildStdout>,
    }

    impl StopFixture {
        fn start(paths: &RuntimePaths, mode: &str) -> Self {
            use std::process::{Command, Stdio};
            let script = r#"import fcntl,json,os,signal,socket,sys
mode=sys.argv[3]
signals=0
f=open(sys.argv[1], 'a+')
os.chmod(sys.argv[1], 0o600)
fcntl.flock(f, fcntl.LOCK_EX)
f.seek(0);f.truncate()
json.dump({'pid':os.getpid(), 'proof':json.loads(sys.argv[4])}, f);f.flush()
listener=socket.socket(socket.AF_UNIX)
listener.bind(sys.argv[2]);listener.listen(4)
def release():
    listener.close()
    if mode!='leftover':
        os.unlink(sys.argv[2])
    f.close()
def stopping(signum, frame):
    global signals
    signals+=1
    print('stopping', flush=True)
    if mode in ('normal','leftover'):
        release()
        sys.exit(0)
signal.signal(signal.SIGTERM, stopping)
print('ready', flush=True)
for line in sys.stdin:
    if line.strip()=='release':
        release()
        if mode=='transfer':
            print('released', flush=True)
        else:
            sys.exit(0)
    elif line.strip()=='park':
        listener.close();os.unlink(sys.argv[2])
        print('parked', flush=True)
    elif line.strip()=='signals':
        print('signals:'+str(signals), flush=True)
    elif line.strip()=='exit':
        sys.exit(0)
"#;
            let child = Command::new("/usr/bin/python3")
                .args(["-I", "-B", "-c", script])
                .arg(&paths.lock)
                .arg(&paths.socket)
                .arg(mode)
                .arg(serde_json::to_string(&proof()).unwrap())
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .unwrap();
            // Establish cleanup before any fallible pipe/identity observation.
            let mut fixture = Self {
                child,
                input: None,
                output: None,
            };
            fixture.input = fixture.child.stdin.take();
            fixture.output = fixture.child.stdout.take();
            assert_eq!(fixture.line(), "ready");
            fixture
        }

        fn line(&mut self) -> String {
            use std::io::Read;
            use std::time::{Duration, Instant};
            let deadline = Instant::now() + Duration::from_secs(10);
            let output = self.output.as_mut().unwrap();
            let mut bytes = Vec::new();
            loop {
                let remaining = deadline.saturating_duration_since(Instant::now());
                assert!(
                    !remaining.is_zero(),
                    "private stop fixture did not publish its event"
                );
                let mut event = libc::pollfd {
                    fd: output.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                };
                // SAFETY: one live pipe descriptor and writable event structure.
                let result = unsafe {
                    libc::poll(
                        &mut event,
                        1,
                        remaining.as_millis().min(i32::MAX as u128) as i32,
                    )
                };
                if result < 0 && io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                assert!(result > 0, "private stop fixture event timed out");
                let mut byte = [0];
                assert_eq!(
                    output.read(&mut byte).unwrap(),
                    1,
                    "private stop fixture exited before its event"
                );
                if byte[0] == b'\n' {
                    return String::from_utf8(bytes).unwrap();
                }
                bytes.push(byte[0]);
                assert!(bytes.len() <= 4096);
            }
        }

        fn send(&mut self, command: &[u8]) {
            use std::io::Write;
            self.input.as_mut().unwrap().write_all(command).unwrap();
        }
    }

    impl Drop for StopFixture {
        fn drop(&mut self) {
            // These are exclusively owned, unreaped direct fixture children;
            // no provider, descendant, numeric-PID discovery or production state.
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    #[test]
    fn rebuild_stop_waits_for_original_exit_and_preserves_live_timeout() {
        use std::time::Duration;
        let root = tempdir().unwrap();
        let paths = paths(root.path().to_path_buf());
        let mut normal = StopFixture::start(&paths, "normal");
        let pin = open_validated_pidfd(
            &File::open(&paths.lock).unwrap(),
            normal.child.id(),
            Path::new("/proc"),
        )
        .unwrap();
        assert!(terminate_daemon_at(&paths, Duration::from_secs(2)).unwrap());
        assert!(
            wait_process(&pin, Duration::ZERO).unwrap(),
            "success requires the ORIGINAL process exit, not just SIGTERM delivery"
        );
        assert!(normal.child.wait().unwrap().success());
        assert!(!HostLease::is_held(&paths).unwrap());
        assert!(!paths.socket.exists());

        let mut retained = StopFixture::start(&paths, "hold");
        let original_contents = fs::read(&paths.lock).unwrap();
        let socket = fs::symlink_metadata(&paths.socket).unwrap();
        let selected = paths.clone();
        let stopping =
            std::thread::spawn(move || terminate_daemon_at(&selected, Duration::from_secs(5)));
        // Observe actual signal delivery before awaiting the bounded refusal.
        // The child cannot exit until this test explicitly releases its gate.
        assert_eq!(retained.line(), "stopping");
        let error = stopping.join().unwrap().unwrap_err();
        assert!(
            matches!(error, PlatformError::Io(ref error) if error.kind() == io::ErrorKind::TimedOut)
        );
        assert!(retained.child.try_wait().unwrap().is_none());
        assert!(HostLease::is_held(&paths).unwrap());
        assert_eq!(fs::read(&paths.lock).unwrap(), original_contents);
        let after = fs::symlink_metadata(&paths.socket).unwrap();
        assert_eq!(
            (after.dev(), after.ino()),
            (socket.dev(), socket.ino()),
            "timeout must not remove the original live endpoint"
        );
        retained.send(b"release\n");
        assert!(retained.child.wait().unwrap().success());
        assert!(!HostLease::is_held(&paths).unwrap());
        assert!(!paths.socket.exists());

        let mut leftover = StopFixture::start(&paths, "leftover");
        let socket = fs::symlink_metadata(&paths.socket).unwrap();
        let error = terminate_daemon_at(&paths, Duration::from_secs(2)).unwrap_err();
        assert!(
            error.to_string().contains("listener path remains"),
            "{error}"
        );
        assert!(leftover.child.wait().unwrap().success());
        assert!(!HostLease::is_held(&paths).unwrap());
        let after = fs::symlink_metadata(&paths.socket).unwrap();
        assert_eq!(
            (after.dev(), after.ino()),
            (socket.dev(), socket.ino()),
            "original process exit does not permit deleting an unproven remaining socket"
        );
        fs::remove_file(&paths.socket).unwrap(); // Only this test's leftover node.
    }

    #[test]
    fn rebuild_stop_recovers_unheld_corrupt_metadata_but_refuses_a_replaced_lock() {
        use std::time::Duration;
        let root = tempdir().unwrap();
        let paths = paths(root.path().to_path_buf());
        fs::write(&paths.lock, b"truncated").unwrap();
        leave_stale_socket(&paths.socket);
        assert!(!terminate_daemon_at(&paths, Duration::from_secs(5)).unwrap());
        assert_eq!(fs::read(&paths.lock).unwrap(), b"truncated");
        assert!(!paths.socket.exists());

        // Pause at the actual recovery boundary with its acquired original
        // lease, then substitute the named file before recovery can inspect a
        // different owner's socket. The old descriptor remains locked.
        let original = File::open(&paths.lock).unwrap();
        assert_eq!(
            unsafe { libc::flock(original.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0
        );
        fs::rename(&paths.lock, root.path().join("original.lock")).unwrap();
        let mut replacement = StopFixture::start(&paths, "hold");
        let contents = fs::read(&paths.lock).unwrap();
        let endpoint = fs::symlink_metadata(&paths.socket).unwrap();
        assert!(recover_unheld_stop_socket(&original, &paths).is_err());
        replacement.send(b"signals\n");
        assert_eq!(replacement.line(), "signals:0");
        assert!(replacement.child.try_wait().unwrap().is_none());
        assert!(HostLease::is_held(&paths).unwrap());
        assert_eq!(fs::read(&paths.lock).unwrap(), contents);
        let after = fs::symlink_metadata(&paths.socket).unwrap();
        assert_eq!((after.dev(), after.ino()), (endpoint.dev(), endpoint.ino()));
        assert!(UnixStream::connect(&paths.socket).is_ok());
        replacement.send(b"release\n");
        assert!(replacement.child.wait().unwrap().success());
        assert_eq!(
            unsafe { libc::flock(original.as_raw_fd(), libc::LOCK_UN) },
            0
        );
    }

    #[test]
    fn rebuild_stop_refuses_a_replacement_without_signalling_or_unlinking_it() {
        use std::time::Duration;
        for different_inode in [false, true] {
            let root = tempdir().unwrap();
            let paths = paths(root.path().to_path_buf());
            let mut original = StopFixture::start(&paths, "transfer");
            let selected = paths.clone();
            let stopping =
                std::thread::spawn(move || terminate_daemon_at(&selected, Duration::from_secs(5)));
            assert_eq!(original.line(), "stopping");
            if different_inode {
                // The original keeps its kernel lease while a distinct inode
                // and listener become current at the same named paths.
                fs::rename(&paths.lock, root.path().join("original.lock")).unwrap();
                original.send(b"park\n");
                assert_eq!(original.line(), "parked");
                assert!(HostLease::is_lock_held(&root.path().join("original.lock")).unwrap());
            } else {
                // Real HostLease::drop ordering permits a successor after the
                // socket/lease release but before original process exit.
                original.send(b"release\n");
                assert_eq!(original.line(), "released");
            }
            let mut replacement = StopFixture::start(&paths, "hold");
            let replacement_contents = fs::read(&paths.lock).unwrap();
            let replacement_socket = fs::symlink_metadata(&paths.socket).unwrap();
            original.send(b"exit\n");
            assert!(original.child.wait().unwrap().success());
            let error = stopping.join().unwrap().unwrap_err();
            assert!(error.to_string().contains("ownership changed"), "{error}");
            replacement.send(b"signals\n");
            assert_eq!(replacement.line(), "signals:0");
            assert!(replacement.child.try_wait().unwrap().is_none());
            assert!(HostLease::is_held(&paths).unwrap());
            assert_eq!(fs::read(&paths.lock).unwrap(), replacement_contents);
            let socket = fs::symlink_metadata(&paths.socket).unwrap();
            assert_eq!(
                (socket.dev(), socket.ino()),
                (replacement_socket.dev(), replacement_socket.ino())
            );
            assert!(
                UnixStream::connect(&paths.socket).is_ok(),
                "replacement listener must remain usable"
            );
            replacement.send(b"release\n");
            assert!(replacement.child.wait().unwrap().success());
        }
    }

    #[cfg(feature = "ui")]
    #[test]
    fn confirmed_restart_refuses_changed_proof_and_terminates_only_its_pinned_process() {
        use std::{
            io::BufRead,
            process::{Command, Stdio},
            time::Duration,
        };
        struct Child(std::process::Child);
        impl Drop for Child {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let root = tempdir().unwrap();
        let paths = paths(root.path().to_path_buf());
        let mut original = proof();
        original.boot_id = read_boot_id().unwrap();
        let script = r#"import fcntl,json,os,signal,sys,time
f=open(sys.argv[1], 'w+')
os.chmod(sys.argv[1], 0o600)
fcntl.flock(f, fcntl.LOCK_EX)
signal.signal(signal.SIGTERM, signal.SIG_IGN)
json.dump({'pid':os.getpid(), 'proof':json.loads(sys.argv[2])}, f)
f.flush()
print('ready', flush=True)
while True: time.sleep(1)
"#;
        let mut child = Child(
            Command::new("python3")
                .args(["-c", script])
                .arg(&paths.lock)
                .arg(serde_json::to_string(&original).unwrap())
                .stdout(Stdio::piped())
                .spawn()
                .unwrap(),
        );
        let mut ready = String::new();
        std::io::BufReader::new(child.0.stdout.take().unwrap())
            .read_line(&mut ready)
            .unwrap();
        assert_eq!(ready.trim(), "ready");
        let target = restart_target_at(&paths).unwrap();
        let mut replacement = original.clone();
        replacement.host_incarnation = crate::model::HostIncarnation::new("different-incarnation");
        fs::write(
            &paths.lock,
            serde_json::to_vec(&serde_json::json!({ "pid": child.0.id(), "proof": replacement }))
                .unwrap(),
        )
        .unwrap();
        assert!(
            target
                .terminate_at(&paths, Duration::from_millis(20))
                .is_err()
        );
        assert!(
            child.0.try_wait().unwrap().is_none(),
            "obsolete consent cannot signal a changed owner"
        );
        fs::write(
            &paths.lock,
            serde_json::to_vec(&serde_json::json!({ "pid": child.0.id(), "proof": original }))
                .unwrap(),
        )
        .unwrap();
        target
            .terminate_at(&paths, Duration::from_millis(20))
            .unwrap();
        assert!(
            !child.0.wait().unwrap().success(),
            "the exact process that ignored graceful stop was terminated"
        );
    }

    #[cfg(feature = "ui")]
    #[test]
    fn recovery_lease_serializes_separate_openers_until_owner_drops() {
        let root = tempdir().unwrap();
        let socket = root.path().join(SOCKET_FILE_NAME);
        let lease = RecoveryLease::acquire(&socket, std::time::Duration::ZERO).unwrap();
        assert!(matches!(
            RecoveryLease::acquire(&socket, std::time::Duration::from_millis(5)),
            Err(PlatformError::Io(error)) if error.kind() == io::ErrorKind::TimedOut
        ));
        drop(lease);
        let replacement = RecoveryLease::acquire(&socket, std::time::Duration::ZERO).unwrap();
        // Releasing an earlier owner must not unlink the shared lock identity.
        assert!(matches!(
            RecoveryLease::acquire(&socket, std::time::Duration::ZERO),
            Err(PlatformError::Io(error)) if error.kind() == io::ErrorKind::TimedOut
        ));
        drop(replacement);
    }

    #[cfg(feature = "ui")]
    #[test]
    fn recovery_lease_rejects_symlink_without_touching_target() {
        let root = tempdir().unwrap();
        let target = root.path().join("target");
        fs::write(&target, b"untouched").unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o600)).unwrap();
        std::os::unix::fs::symlink(&target, root.path().join("cagentsd.recovery.lock")).unwrap();
        assert!(
            RecoveryLease::acquire(
                &root.path().join(SOCKET_FILE_NAME),
                std::time::Duration::ZERO,
            )
            .is_err()
        );
        assert_eq!(fs::read(&target).unwrap(), b"untouched");
    }
}
