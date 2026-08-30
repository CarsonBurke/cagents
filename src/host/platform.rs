use std::{
    env,
    ffi::{c_int, c_short, c_ulong, c_void},
    fs::{self, DirBuilder, File, OpenOptions},
    io::{self, Read, Seek, SeekFrom, Write},
    os::{
        fd::AsRawFd,
        unix::{
            fs::{DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt},
            net::{UnixListener, UnixStream},
        },
    },
    path::{Path, PathBuf},
    sync::{
        LazyLock,
        atomic::{AtomicBool, AtomicI32, Ordering},
    },
};

use serde::Serialize;
use thiserror::Error;

use crate::{
    instance,
    model::{BootId, HostSessionProof},
    terminal_ipc::SOCKET_FILE_NAME,
};

const PRIVATE_DIRECTORY_MODE: u32 = 0o700;
const PRIVATE_FILE_MODE: u32 = 0o600;
const O_NOFOLLOW: c_int = 0o400000;
const LOCK_EX: c_int = 2;
const LOCK_NB: c_int = 4;
const LOCK_UN: c_int = 8;
const SOL_SOCKET: c_int = 1;
const SO_PEERCRED: c_int = 17;
const SIGINT: c_int = 2;
const SIGTERM: c_int = 15;
const POLLIN: c_short = 0x001;
const POLL_FOREVER: c_int = -1;

static TERMINATION_REQUESTED: AtomicBool = AtomicBool::new(false);

/// The daemon's wakeup pipe. It is created once and never dropped, because the
/// signal handler writes to its sending descriptor: that descriptor must never
/// be closed, and its number must never be recycled by an unrelated `open`.
static WAKEUP: LazyLock<io::Result<Wakeup>> = LazyLock::new(Wakeup::create);
/// The sending descriptor of [`WAKEUP`], readable from a signal handler, which
/// must touch nothing but lock-free atomics.
static WAKEUP_FD: AtomicI32 = AtomicI32::new(-1);

unsafe extern "C" {
    fn flock(fd: c_int, operation: c_int) -> c_int;
    fn geteuid() -> u32;
    fn getsockopt(
        socket: c_int,
        level: c_int,
        option_name: c_int,
        option_value: *mut c_void,
        option_len: *mut u32,
    ) -> c_int;
    fn signal(signal: c_int, handler: usize) -> usize;
    fn poll(fds: *mut PollFd, count: c_ulong, timeout: c_int) -> c_int;
    #[link_name = "write"]
    fn write_byte(fd: c_int, buffer: *const c_void, count: usize) -> isize;
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct UCred {
    pid: i32,
    uid: u32,
    gid: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct PollFd {
    fd: c_int,
    events: c_short,
    revents: c_short,
}

/// A socket pair used purely as a wakeup channel: any byte written to `writer`
/// interrupts a [`wait_for_activity`] blocked on `reader`.
struct Wakeup {
    reader: UnixStream,
    writer: UnixStream,
}

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

fn wakeup() -> io::Result<&'static Wakeup> {
    // `io::Error` is not `Clone`, so the stored failure is reproduced rather
    // than handed out; kind and message are what callers report.
    WAKEUP
        .as_ref()
        .map_err(|error| io::Error::new(error.kind(), error.to_string()))
}

/// Interrupts a blocking [`wait_for_activity`] from any thread. Losing a byte to
/// a full pipe is harmless, because a full pipe is a pending wakeup.
pub(crate) fn wake() {
    if let Ok(wakeup) = &*WAKEUP {
        wakeup.wake();
    }
}

/// Blocks until `listener` has a connection pending or something calls [`wake`].
/// There is no timeout: an idle daemon causes no wakeups at all.
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
    UnsafeRuntimeDirectory { path: PathBuf },
    #[error("daemon lock path {path:?} is not a regular file")]
    UnsafeLockPath { path: PathBuf },
    #[error("another cagents daemon owns {path:?}")]
    AlreadyRunning { path: PathBuf },
    #[error("refusing to replace non-socket runtime path {path:?}")]
    UnsafeSocketPath { path: PathBuf },
    #[error("failed to prepare cagents daemon runtime: {0}")]
    Io(#[from] io::Error),
    #[error("failed to serialize cagents daemon lock metadata: {0}")]
    Serialize(#[from] serde_json::Error),
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

pub(crate) struct HostLease {
    socket_path: PathBuf,
    lock: File,
}

impl HostLease {
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

#[derive(Serialize)]
struct LockMetadata<'a> {
    pid: u32,
    proof: &'a HostSessionProof,
}

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
    // SAFETY: geteuid has no preconditions.
    unsafe { geteuid() }
}

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

pub(crate) fn termination_requested() -> bool {
    TERMINATION_REQUESTED.load(Ordering::Acquire)
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::symlink;

    use tempfile::tempdir;

    use super::*;
    use crate::model::{HostIncarnation, HostSessionId};

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

    fn mode(path: &Path) -> u32 {
        fs::symlink_metadata(path).unwrap().permissions().mode() & 0o777
    }

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
}
