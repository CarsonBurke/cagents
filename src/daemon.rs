#[cfg(feature = "host")]
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
    ffi::{c_short, c_ulong, c_void},
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
const LOCK_EX: c_int = 2;
const LOCK_NB: c_int = 4;
const LOCK_UN: c_int = 8;
#[cfg(feature = "host")]
const SOL_SOCKET: c_int = 1;
#[cfg(feature = "host")]
const SO_PEERCRED: c_int = 17;
#[cfg(feature = "host")]
const SIGINT: c_int = 2;
const SIGTERM: c_int = 15;
#[cfg(feature = "host")]
const POLLIN: c_short = 0x001;
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

unsafe extern "C" {
    fn flock(fd: c_int, operation: c_int) -> c_int;
    #[cfg(feature = "host")]
    fn getsockopt(
        socket: c_int,
        level: c_int,
        option_name: c_int,
        option_value: *mut c_void,
        option_len: *mut u32,
    ) -> c_int;
    #[cfg(feature = "host")]
    fn signal(signal: c_int, handler: usize) -> usize;
    #[cfg(feature = "host")]
    fn poll(fds: *mut PollFd, count: c_ulong, timeout: c_int) -> c_int;
    #[cfg(feature = "host")]
    #[link_name = "write"]
    fn write_byte(fd: c_int, buffer: *const c_void, count: usize) -> isize;
}

#[cfg(feature = "host")]
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct UCred {
    pid: i32,
    uid: u32,
    gid: u32,
}

#[cfg(feature = "host")]
#[repr(C)]
#[derive(Clone, Copy)]
struct PollFd {
    fd: c_int,
    events: c_short,
    revents: c_short,
}

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
        let (reader, writer) = UnixStream::pair()?;
        reader.set_nonblocking(true)?;
        // A full pipe is itself a pending wakeup, so no writer may block on one.
        writer.set_nonblocking(true)?;
        // Published here rather than by the caller, so the descriptor is visible
        // to the signal handler as soon as it exists.
        WAKEUP_FD.store(writer.as_raw_fd(), Ordering::Release);
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
    let wakeup = wakeup()?;
    let mut fds = [
        PollFd {
            fd: listener.as_raw_fd(),
            events: POLLIN,
            revents: 0,
        },
        PollFd {
            fd: wakeup.reader.as_raw_fd(),
            events: POLLIN,
            revents: 0,
        },
    ];
    // SAFETY: fds is a live, fully initialised array of the length passed to
    // poll, and both descriptors are owned for the duration of the call.
    let ready = unsafe { poll(fds.as_mut_ptr(), fds.len() as c_ulong, POLL_FOREVER) };
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
    #[cfg(feature = "host")]
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
            #[cfg(feature = "host")]
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
                libc::O_CREAT | libc::O_RDWR | libc::O_CLOEXEC | libc::O_NONBLOCK | O_NOFOLLOW,
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
            if unsafe { flock(lock.as_raw_fd(), LOCK_EX | LOCK_NB) } == 0 {
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
        unsafe { flock(self.lock.as_raw_fd(), libc::LOCK_UN) };
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
            .custom_flags(O_NOFOLLOW)
            .open(&paths.lock)?;
        validate_lock_file(&paths.lock, &lock.metadata()?, current_uid())?;
        lock.set_permissions(fs::Permissions::from_mode(PRIVATE_FILE_MODE))?;
        // SAFETY: flock receives a live file descriptor and no pointer data.
        if unsafe { flock(lock.as_raw_fd(), LOCK_EX | LOCK_NB) } != 0 {
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
            .custom_flags(O_NOFOLLOW)
            .open(path)
        {
            Ok(lock) => lock,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error.into()),
        };
        validate_lock_file(path, &lock.metadata()?, current_uid())?;
        // SAFETY: flock receives a live, validated descriptor and scalar flags.
        if unsafe { flock(lock.as_raw_fd(), LOCK_EX | LOCK_NB) } != 0 {
            let error = io::Error::last_os_error();
            return if error.kind() == io::ErrorKind::WouldBlock {
                Ok(true)
            } else {
                Err(error.into())
            };
        }
        // Explicit release also covers copies inherited by a concurrent fork.
        // SAFETY: this descriptor owns the exclusive lock acquired above.
        if unsafe { flock(lock.as_raw_fd(), LOCK_UN) } != 0 {
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
            .custom_flags(O_NOFOLLOW)
            .open(&paths.lock)?;
        validate_lock_file(&paths.lock, &lock.metadata()?, current_uid())?;
        // SAFETY: the validated descriptor is owned through the guard lifetime.
        if unsafe { flock(lock.as_raw_fd(), LOCK_EX | LOCK_NB) } != 0 {
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
        let _ = unsafe { flock(self.lock.as_raw_fd(), LOCK_UN) };
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
        .custom_flags(O_NOFOLLOW)
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
        .custom_flags(O_NOFOLLOW)
        .open(lock_path)?;
    validate_lock_file(lock_path, &lock.metadata()?, current_uid())?;
    // SAFETY: flock receives a live descriptor and no pointers.
    if unsafe { flock(lock.as_raw_fd(), LOCK_EX | LOCK_NB) } == 0 {
        // SAFETY: this descriptor owns the lock just acquired.
        let _ = unsafe { flock(lock.as_raw_fd(), LOCK_UN) };
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

/// Sends SIGTERM to the daemon identified by the current instance's lock.
///
/// The pidfd is opened before `/proc` validation, so PID reuse cannot redirect
/// the signal after the lock owner has been authenticated.
pub(crate) fn terminate_daemon_from_lock() -> Result<bool, PlatformError> {
    let paths = RuntimePaths::discover()?;
    let mut lock = match OpenOptions::new()
        .read(true)
        .custom_flags(O_NOFOLLOW)
        .open(&paths.lock)
    {
        Ok(lock) => lock,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            recover_stale_socket(&paths.socket)?;
            return Ok(false);
        }
        Err(error) => return Err(error.into()),
    };
    let Some(pidfd) = daemon_pidfd_from_lock(&mut lock, &paths)? else {
        return Ok(false);
    };
    // SAFETY: pidfd is a live pidfd for the validated lock owner, SIGTERM is a
    // valid signal, and a null siginfo pointer with flags zero is supported.
    let result = unsafe {
        libc::syscall(
            libc::SYS_pidfd_send_signal,
            pidfd.as_raw_fd(),
            SIGTERM,
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
    Ok(true)
}

fn daemon_pidfd_from_lock(
    lock: &mut File,
    paths: &RuntimePaths,
) -> Result<Option<File>, PlatformError> {
    validate_lock_file(&paths.lock, &lock.metadata()?, current_uid())?;
    // Check ownership before trusting metadata: a crashed writer can leave a
    // truncated lock file, but an unheld flock proves no daemon can be using it.
    // SAFETY: lock owns a live descriptor and flock takes scalar values.
    if unsafe { flock(lock.as_raw_fd(), LOCK_EX | LOCK_NB) } == 0 {
        let recovery = recover_stale_socket(&paths.socket);
        // SAFETY: this descriptor owns the lock acquired above.
        let _ = unsafe { flock(lock.as_raw_fd(), LOCK_UN) };
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
            if unsafe { flock(lock.as_raw_fd(), LOCK_EX | LOCK_NB) } != 0 {
                return Err(validation_error.into());
            }
            recover_stale_socket(&paths.socket)?;
            // SAFETY: this descriptor still owns the lock acquired above.
            let _ = unsafe { flock(lock.as_raw_fd(), LOCK_UN) };
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
    validate_owner(path, metadata, effective_uid)
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
    let mut credentials = UCred::default();
    let mut length =
        u32::try_from(std::mem::size_of::<UCred>()).expect("Linux ucred size must fit socklen_t");
    // SAFETY: credentials and length are valid writable objects of the sizes
    // passed to getsockopt, and stream owns a live socket descriptor.
    let result = unsafe {
        getsockopt(
            stream.as_raw_fd(),
            SOL_SOCKET,
            SO_PEERCRED,
            (&mut credentials as *mut UCred).cast(),
            &mut length,
        )
    };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    if usize::try_from(length).ok() != Some(std::mem::size_of::<UCred>()) {
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
        let _ = write_byte(fd, (&raw const byte).cast(), 1);
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
        signal(SIGINT, request_termination as *const () as usize);
        signal(SIGTERM, request_termination as *const () as usize);
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
    #[cfg(feature = "host")]
    use crate::model::{HostIncarnation, HostSessionId};

    #[cfg(feature = "host")]
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
            #[cfg(feature = "host")]
            directory,
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
