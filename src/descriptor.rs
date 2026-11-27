//! Close-on-exec descriptor and socket creation with Linux's semantics on
//! every supported kernel.
//!
//! Linux creates these descriptors with their flags in one call. Darwin lacks
//! `pipe2`, `dup3` and `SOCK_CLOEXEC`, so it sets the flags immediately after
//! creation, as the standard library does there. A process spawned on another
//! thread inside that window can inherit the descriptor across exec, as with
//! the standard library's own descriptors; only spawns that seal inherited
//! descriptors before exec, as the daemon's do, are closed to it.

use std::{
    io,
    os::{
        fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd},
        unix::net::{SocketAddr, UnixListener, UnixStream},
    },
    time::Instant,
};

/// Wait for descriptor activity without changing the file description. Report
/// hangup or pending errors as activity so the caller can read the actual result.
pub(crate) fn wait_for_activity(
    descriptor: BorrowedFd<'_>,
    events: libc::c_short,
    deadline: Option<Instant>,
) -> io::Result<bool> {
    loop {
        let timeout = if let Some(deadline) = deadline {
            let now = Instant::now();
            if now >= deadline {
                return Ok(false);
            }
            let milliseconds = (deadline - now).as_nanos().div_ceil(1_000_000);
            milliseconds.min(libc::c_int::MAX as u128) as libc::c_int
        } else {
            -1
        };
        let mut watched = libc::pollfd {
            fd: descriptor.as_raw_fd(),
            events,
            revents: 0,
        };
        // SAFETY: the descriptor is borrowed, and poll sees one live pollfd.
        let ready = unsafe { libc::poll(&mut watched, 1, timeout) };
        if ready < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        if watched.revents & libc::POLLNVAL != 0 {
            return Err(io::Error::from_raw_os_error(libc::EBADF));
        }
        if watched.revents & (events | libc::POLLHUP | libc::POLLERR) != 0 {
            return Ok(true);
        }
    }
}

/// `openat` relative to `directory`, with creation races settled as Linux
/// settles them.
///
/// When several openers create one name beneath a directory descriptor at
/// once, Darwin answers every one but the winner `ENOENT`; a path-based `open`
/// does not. A creating open that reads `ENOENT` therefore tries again: the
/// retry opens the winner's file, or reports `EEXIST` under `O_EXCL`. A
/// removed directory keeps answering `ENOENT`, which is returned once the
/// bounded retries are spent.
pub(crate) fn openat(
    directory: RawFd,
    name: &std::ffi::CStr,
    flags: libc::c_int,
    mode: libc::mode_t,
) -> io::Result<OwnedFd> {
    const CREATION_ATTEMPTS: usize = if cfg!(target_os = "macos") { 8 } else { 1 };
    let mut attempt = 1;
    loop {
        // SAFETY: `name` is NUL-terminated and outlives the call; the mode
        // argument is the promoted variadic `O_CREAT` reads, ignored otherwise.
        let descriptor =
            unsafe { libc::openat(directory, name.as_ptr(), flags, libc::c_uint::from(mode)) };
        if descriptor >= 0 {
            // SAFETY: `openat` just returned this descriptor; nothing else owns it.
            return Ok(unsafe { OwnedFd::from_raw_fd(descriptor) });
        }
        let error = io::Error::last_os_error();
        if flags & libc::O_CREAT == 0
            || error.raw_os_error() != Some(libc::ENOENT)
            || attempt == CREATION_ATTEMPTS
        {
            return Err(error);
        }
        attempt += 1;
    }
}

/// Marks `descriptor` close-on-exec, which Linux sets at creation instead.
#[cfg(target_os = "macos")]
pub(crate) fn set_close_on_exec(descriptor: BorrowedFd<'_>) -> io::Result<()> {
    // SAFETY: the descriptor is borrowed for the call; FD_CLOEXEC is the only flag.
    if unsafe { libc::fcntl(descriptor.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Puts `descriptor` into nonblocking mode.
pub(crate) fn set_nonblocking(descriptor: BorrowedFd<'_>) -> io::Result<()> {
    let raw = descriptor.as_raw_fd();
    // SAFETY: the descriptor is borrowed for the call; F_GETFL takes no argument.
    let flags = unsafe { libc::fcntl(raw, libc::F_GETFL) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: as above, with the `int` argument F_SETFL requires.
    if unsafe { libc::fcntl(raw, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// A close-on-exec pipe, `(reader, writer)`, optionally nonblocking at both ends.
pub(crate) fn pipe(nonblocking: bool) -> io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [-1; 2];
    #[cfg(target_os = "linux")]
    {
        let flags = libc::O_CLOEXEC | if nonblocking { libc::O_NONBLOCK } else { 0 };
        // SAFETY: pipe2 initializes both descriptors, owned at once below.
        if unsafe { libc::pipe2(fds.as_mut_ptr(), flags) } == -1 {
            return Err(io::Error::last_os_error());
        }
    }
    #[cfg(target_os = "macos")]
    // SAFETY: pipe initializes both descriptors, owned at once below.
    if unsafe { libc::pipe(fds.as_mut_ptr()) } == -1 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: both descriptors were just created and are owned only here.
    let (reader, writer) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
    #[cfg(target_os = "macos")]
    for end in [&reader, &writer] {
        set_close_on_exec(std::os::fd::AsFd::as_fd(end))?;
        if nonblocking {
            set_nonblocking(std::os::fd::AsFd::as_fd(end))?;
        }
    }
    Ok((reader, writer))
}

/// A close-on-exec Unix-domain socket of `kind`, optionally nonblocking.
pub(crate) fn unix_socket(kind: libc::c_int, nonblocking: bool) -> io::Result<OwnedFd> {
    #[cfg(target_os = "linux")]
    let kind = kind | libc::SOCK_CLOEXEC | if nonblocking { libc::SOCK_NONBLOCK } else { 0 };
    // SAFETY: socket takes scalar arguments and returns a new descriptor.
    let raw = unsafe { libc::socket(libc::AF_UNIX, kind, 0) };
    if raw < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the descriptor was just created and is owned only here.
    let socket = unsafe { OwnedFd::from_raw_fd(raw) };
    #[cfg(target_os = "macos")]
    {
        set_close_on_exec(std::os::fd::AsFd::as_fd(&socket))?;
        if nonblocking {
            set_nonblocking(std::os::fd::AsFd::as_fd(&socket))?;
        }
    }
    keep_options_after_shutdown(std::os::fd::AsFd::as_fd(&socket))?;
    Ok(socket)
}

/// Whether a refused Unix-domain `connect` should be tried again before it
/// counts as proof that nothing listens there. `refusing_since` is when the
/// first refusal of this endpoint was seen.
///
/// Linux refuses only a socket nobody listens on, and answers a full backlog
/// with `EAGAIN`, so a refusal is final there. Darwin refuses a full backlog
/// with the same `ECONNREFUSED`. A live listener drains its backlog as it
/// accepts, so there a refusal counts only once it has persisted for
/// [`REFUSAL_WINDOW`]. That is weaker than Linux: a listener whose backlog
/// stays full for the whole window is taken for one that stopped. Retrying
/// until a caller's own deadline instead, as Linux retries `EAGAIN`, would
/// make every request to a crashed server, whose socket file stays behind,
/// wait out that whole deadline. A deadline that ends inside the window
/// leaves the refusal unproved, so the caller reports a timeout, not absence.
pub(crate) fn retry_refusal(error: &io::Error, refusing_since: std::time::Instant) -> bool {
    #[cfg(target_os = "linux")]
    {
        let _ = (error, refusing_since);
        false
    }
    #[cfg(target_os = "macos")]
    {
        error.kind() == io::ErrorKind::ConnectionRefused
            && refusing_since.elapsed() < REFUSAL_WINDOW
    }
}

/// How long Darwin must keep refusing; see [`retry_refusal`].
#[cfg(target_os = "macos")]
pub(crate) const REFUSAL_WINDOW: std::time::Duration = std::time::Duration::from_millis(100);

/// A listen backlog that admits exactly one pending connection, so a test can
/// fill it with one. Linux admits one more than the backlog; XNU admits the
/// backlog itself, and replaces a backlog of 0 with its default.
#[cfg(test)]
pub(crate) const ONE_PENDING_BACKLOG: libc::c_int = if cfg!(target_os = "macos") { 1 } else { 0 };

/// Connects a Unix-domain stream to `path`; see [`keep_options_after_shutdown`].
pub(crate) fn connect(path: &std::path::Path) -> io::Result<UnixStream> {
    let stream = UnixStream::connect(path)?;
    keep_options_after_shutdown(std::os::fd::AsFd::as_fd(&stream))?;
    Ok(stream)
}

/// [`connect`] for a caller that takes a refusal to mean nothing listens at
/// `path`. Linux makes one attempt, since its refusal is final. Darwin also
/// refuses a full backlog, so a refusal is tried again until it has lasted
/// [`REFUSAL_WINDOW`] and only then returned; see [`retry_refusal`].
pub(crate) fn connect_settled(path: impl AsRef<std::path::Path>) -> io::Result<UnixStream> {
    let path = path.as_ref();
    let refusing_since = std::time::Instant::now();
    let mut delay = std::time::Duration::from_millis(1);
    loop {
        match connect(path) {
            Err(error) if retry_refusal(&error, refusing_since) => {
                std::thread::sleep(delay);
                delay = (delay * 2).min(std::time::Duration::from_millis(20));
            }
            result => return result,
        }
    }
}

/// Binds a Unix-domain listener at `path` whose connections keep their
/// socket options after shutdown; see [`keep_options_after_shutdown`]. XNU
/// copies the setting from the listener to each connection as it queues, so
/// it is in place before a fast peer can write, close, and leave the accepted
/// stream refusing it.
pub fn bind(path: impl AsRef<std::path::Path>) -> io::Result<UnixListener> {
    let listener = UnixListener::bind(path)?;
    keep_options_after_shutdown(std::os::fd::AsFd::as_fd(&listener))?;
    Ok(listener)
}

/// A Unix address includes its terminating NUL. Filesystem paths have no such
/// small limit; [`with_socket_address`] reaches a longer endpoint through its
/// held directory.
pub fn socket_fits(path: &std::path::Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    // SAFETY: sockaddr_un is plain data and only its field length is read.
    let capacity = unsafe { std::mem::zeroed::<libc::sockaddr_un>() }
        .sun_path
        .len();
    path.as_os_str().as_bytes().len() < capacity
}

/// Runs `operation` with an address that reaches the endpoint at `path`, for
/// `bind`, `connect`, or a `stat`, `chmod` or unlink of the socket; it must
/// resolve that address and nothing else. A path that fits `sun_path` is
/// passed as is. A longer one is reached through its parent directory, opened
/// once and checked to be private, so the operation acts in exactly the
/// directory that was checked:
///
/// * Linux passes `/proc/self/fd/N/<name>` for the held directory.
/// * Darwin has neither `/proc` nor `bindat`/`connectat`, but each thread can
///   have its own working directory. `operation` runs on a thread of its own
///   whose working directory is the held one, and is passed the bare name. No
///   other code runs on that thread, so nothing else resolves paths there.
///
/// Either way the endpoint stays at its full path, so whatever finds,
/// authenticates or retires it by that path keeps working.
pub fn with_socket_address<T: Send>(
    path: &std::path::Path,
    operation: impl FnOnce(&std::path::Path) -> io::Result<T> + Send,
) -> io::Result<T> {
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    if socket_fits(path) {
        return operation(path);
    }
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "socket endpoint has no directory",
            )
        })?;
    let name = path.file_name().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "socket endpoint has no filename",
        )
    })?;
    crate::secure_file::reject_symlink_components(parent)?;
    let directory = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(parent)?;
    let metadata = directory.metadata()?;
    if !metadata.is_dir()
        || metadata.uid() != crate::daemon::current_uid()
        || metadata.mode() & 0o077 != 0
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "unsafe socket directory",
        ));
    }
    // Hold the directory until the operation completes.
    #[cfg(target_os = "linux")]
    {
        let address =
            std::path::PathBuf::from(format!("/proc/self/fd/{}", directory.as_raw_fd())).join(name);
        operation(&address)
    }
    #[cfg(target_os = "macos")]
    {
        use std::os::fd::AsFd;
        let name = std::path::Path::new(name);
        std::thread::scope(|scope| {
            std::thread::Builder::new()
                .name("socket-address".into())
                .spawn_scoped(scope, || {
                    enter_directory(directory.as_fd())?;
                    operation(name)
                })?
                .join()
                .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
        })
    }
}

/// Makes `directory` the calling thread's working directory, leaving every
/// other thread's alone.
///
/// `pthread_fchdir_np` is exported by `libsystem_pthread`, whose own `fts`
/// uses it, but no SDK header declares it. It is looked up rather than linked
/// so that a system without it refuses long endpoints instead of refusing to
/// start the program.
#[cfg(target_os = "macos")]
fn enter_directory(directory: std::os::fd::BorrowedFd<'_>) -> io::Result<()> {
    type ThreadFchdir = unsafe extern "C" fn(libc::c_int) -> libc::c_int;
    static THREAD_FCHDIR: std::sync::OnceLock<Option<ThreadFchdir>> = std::sync::OnceLock::new();
    let thread_fchdir = THREAD_FCHDIR
        .get_or_init(|| {
            // SAFETY: the name is NUL-terminated, and RTLD_DEFAULT only searches
            // the images already loaded.
            let symbol = unsafe { libc::dlsym(libc::RTLD_DEFAULT, c"pthread_fchdir_np".as_ptr()) };
            // SAFETY: `pthread_fchdir_np` is `int pthread_fchdir_np(int fd)`.
            (!symbol.is_null())
                .then(|| unsafe { std::mem::transmute::<*mut libc::c_void, ThreadFchdir>(symbol) })
        })
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::Unsupported,
                "this system cannot reach a socket endpoint longer than a Unix socket address",
            )
        })?;
    // SAFETY: it only changes this thread's working directory to an open
    // directory descriptor.
    if unsafe { thread_fchdir(directory.as_raw_fd()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Accepts a connection with Linux's semantics everywhere: a BSD socket
/// inherits its listener's `O_NONBLOCK` across `accept`, which Linux never
/// does, so the accepted stream is made blocking there. A listener from
/// [`bind`] also hands it Linux's socket options after shutdown.
pub fn accept(listener: &UnixListener) -> io::Result<(UnixStream, SocketAddr)> {
    let (stream, address) = listener.accept()?;
    #[cfg(target_os = "macos")]
    stream.set_nonblocking(false)?;
    Ok((stream, address))
}

/// Darwin refuses every socket option, timeouts included, with `EINVAL` once
/// both directions of a stream are shut down, even while the peer's last
/// reply is still buffered. A daemon that answers and then closes would make
/// the reader fail before it reads the answer. `SONPX_SETOPTSHUT` restores
/// Linux's behavior, where the read returns that reply and then end of file.
#[cfg(target_os = "macos")]
pub(crate) fn keep_options_after_shutdown(socket: BorrowedFd<'_>) -> io::Result<()> {
    /// `struct so_np_extensions` from `<sys/socket.h>`, which libc lacks.
    #[repr(C)]
    struct Extensions {
        flags: u32,
        mask: u32,
    }
    const SONPX_SETOPTSHUT: u32 = 0x0000_0001;
    let extensions = Extensions {
        flags: SONPX_SETOPTSHUT,
        mask: SONPX_SETOPTSHUT,
    };
    // SAFETY: the descriptor is borrowed for the call, and the option value
    // is a live `so_np_extensions` of the size passed.
    let status = unsafe {
        libc::setsockopt(
            socket.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_NP_EXTENSIONS,
            std::ptr::from_ref(&extensions).cast(),
            std::mem::size_of::<Extensions>() as libc::socklen_t,
        )
    };
    if status != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(target_os = "linux")]
pub(crate) fn keep_options_after_shutdown(_socket: BorrowedFd<'_>) -> io::Result<()> {
    Ok(())
}

/// Duplicates `source` onto exactly `target`, close-on-exec. Whatever `target`
/// named before is closed; the caller must own that slot. `source` cannot be
/// `target`: Linux's `dup3` refuses that with `EINVAL`, and Darwin's `dup2`
/// would return the borrowed descriptor itself as a second owner.
pub(crate) fn duplicate_to(source: BorrowedFd<'_>, target: RawFd) -> io::Result<OwnedFd> {
    if source.as_raw_fd() == target {
        return Err(io::Error::from_raw_os_error(libc::EINVAL));
    }
    #[cfg(target_os = "linux")]
    // SAFETY: dup3 takes scalar arguments; the caller owns the target slot.
    let raw = unsafe { libc::dup3(source.as_raw_fd(), target, libc::O_CLOEXEC) };
    #[cfg(target_os = "macos")]
    // SAFETY: dup2 takes scalar arguments; the caller owns the target slot.
    let raw = unsafe { libc::dup2(source.as_raw_fd(), target) };
    if raw < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the target slot now holds a fresh duplicate owned only here.
    let duplicate = unsafe { OwnedFd::from_raw_fd(raw) };
    #[cfg(target_os = "macos")]
    set_close_on_exec(std::os::fd::AsFd::as_fd(&duplicate))?;
    Ok(duplicate)
}

/// Opens `file`'s inode again as a new, independent open file description, so
/// a lock probe through it cannot reuse a lock held through `file`. Linux
/// reopens `/proc/self/fd`. Darwin's `/dev/fd` would only duplicate the same
/// description, so it opens the descriptor's current path without following a
/// final link and refuses anything but the same inode.
pub(crate) fn reopen(file: &std::fs::File, flags: libc::c_int) -> io::Result<std::fs::File> {
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    let path = path_of(file)?;
    #[cfg(target_os = "macos")]
    let flags = flags | libc::O_NOFOLLOW;
    let reopened = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(flags | libc::O_CLOEXEC)
        .open(path)?;
    let (expected, actual) = (file.metadata()?, reopened.metadata()?);
    if expected.dev() != actual.dev() || expected.ino() != actual.ino() {
        return Err(io::Error::other(
            "the file was replaced while it was being reopened",
        ));
    }
    Ok(reopened)
}

/// A path an invoked program can open for an inherited file descriptor.
/// Darwin duplicates the open description, including its current offset.
pub(crate) fn inherited_file_path(fd: RawFd) -> std::path::PathBuf {
    #[cfg(target_os = "linux")]
    let prefix = "/proc/self/fd";
    #[cfg(target_os = "macos")]
    let prefix = "/dev/fd";
    format!("{prefix}/{fd}").into()
}

#[cfg(target_os = "linux")]
pub(crate) fn path_of(file: &std::fs::File) -> io::Result<std::path::PathBuf> {
    Ok(inherited_file_path(file.as_raw_fd()))
}

/// The kernel's current path for an open file, which follows renames.
#[cfg(target_os = "macos")]
pub(crate) fn path_of(file: &std::fs::File) -> io::Result<std::path::PathBuf> {
    crate::fs_safety::descriptor_path(file.as_raw_fd())
}

/// A wake one thread can raise for another that polls: `notify` is written,
/// `watch` is polled. Both are nonblocking, so repeated notifications
/// coalesce and never block. Linux uses one eventfd for both; Darwin, which
/// has none, a pipe.
pub(crate) struct Wake {
    pub(crate) notify: std::sync::Arc<OwnedFd>,
    pub(crate) watch: std::sync::Arc<OwnedFd>,
}

#[cfg(target_os = "linux")]
pub(crate) fn wake() -> io::Result<Wake> {
    // SAFETY: eventfd returns a fresh, close-on-exec owned descriptor. Its
    // nonblocking counter coalesces notifications and cannot stall cancel().
    let raw = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
    if raw < 0 {
        return Err(io::Error::last_os_error());
    }
    let descriptor = std::sync::Arc::new(unsafe { OwnedFd::from_raw_fd(raw) });
    Ok(Wake {
        notify: std::sync::Arc::clone(&descriptor),
        watch: descriptor,
    })
}

#[cfg(target_os = "macos")]
pub(crate) fn wake() -> io::Result<Wake> {
    let (watch, notify) = pipe(true)?;
    Ok(Wake {
        notify: std::sync::Arc::new(notify),
        watch: std::sync::Arc::new(watch),
    })
}

#[cfg(any(feature = "host", feature = "ui"))]
pub(crate) fn set_read_timeout_or_hangup(
    stream: &std::os::unix::net::UnixStream,
    timeout: Option<std::time::Duration>,
) -> io::Result<()> {
    let result = stream.set_read_timeout(timeout);
    #[cfg(target_os = "macos")]
    if matches!(&result, Err(error) if error.raw_os_error() == Some(libc::EINVAL)) {
        // XNU refuses new socket options after a full shutdown. The peer
        // might have closed after its final reply. Let the reader consume
        // any remaining bytes and EOF without changing the prior timeout.
        let mut descriptor = libc::pollfd {
            fd: stream.as_raw_fd(),
            // XNU only registers the read filter when a read or hangup
            // event is requested.
            events: libc::POLLHUP,
            revents: 0,
        };
        // SAFETY: descriptor is writable and contains a live borrowed fd.
        let ready = unsafe { libc::poll(&raw mut descriptor, 1, 0) };
        if ready > 0 && descriptor.revents & libc::POLLHUP != 0 {
            return Ok(());
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use std::{
        ffi::CString,
        fs::File,
        io,
        os::fd::AsRawFd,
        sync::{Arc, Barrier},
        thread,
    };

    /// Racing creators of one name each settle as Linux settles them: every
    /// plain creator opens the file, and exactly one exclusive creator wins.
    #[test]
    fn racing_creators_beneath_a_directory_settle_as_on_linux() {
        const CREATORS: usize = 4;
        let root = tempfile::tempdir().unwrap();
        let directory = Arc::new(File::open(root.path()).unwrap());
        for round in 0..20 {
            for exclusive in [false, true] {
                let name = Arc::new(CString::new(format!("{round}-{exclusive}")).unwrap());
                let start = Arc::new(Barrier::new(CREATORS));
                let flags = libc::O_RDWR
                    | libc::O_CREAT
                    | libc::O_CLOEXEC
                    | if exclusive { libc::O_EXCL } else { 0 };
                let results: Vec<_> = (0..CREATORS)
                    .map(|_| {
                        let (directory, name, start) = (
                            Arc::clone(&directory),
                            Arc::clone(&name),
                            Arc::clone(&start),
                        );
                        thread::spawn(move || {
                            start.wait();
                            super::openat(directory.as_raw_fd(), &name, flags, 0o600)
                                .map_err(|error| error.kind())
                        })
                    })
                    .collect::<Vec<_>>()
                    .into_iter()
                    .map(|creator| creator.join().unwrap())
                    .collect();
                let opened = results.iter().filter(|result| result.is_ok()).count();
                if exclusive {
                    assert_eq!(opened, 1, "{results:?}");
                    assert!(
                        results.iter().all(|result| matches!(
                            result,
                            Ok(_) | Err(io::ErrorKind::AlreadyExists)
                        )),
                        "{results:?}"
                    );
                } else {
                    assert_eq!(opened, CREATORS, "{results:?}");
                }
            }
        }
    }

    #[test]
    fn a_wake_is_readable_once_notified_and_never_blocks_its_notifier() {
        let wake = super::wake().unwrap();
        let mut watched = libc::pollfd {
            fd: wake.watch.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one owned descriptor, valid for the call.
        assert_eq!(unsafe { libc::poll(&mut watched, 1, 0) }, 0);
        let one = 1_u64;
        for _ in 0..100_000 {
            // SAFETY: eight readable bytes; the descriptor is nonblocking, so
            // a full pipe answers EAGAIN rather than blocking.
            let written = unsafe {
                libc::write(
                    wake.notify.as_raw_fd(),
                    (&one as *const u64).cast(),
                    std::mem::size_of::<u64>(),
                )
            };
            if written < 0 {
                assert_eq!(io::Error::last_os_error().kind(), io::ErrorKind::WouldBlock);
                break;
            }
        }
        // SAFETY: as above.
        assert_eq!(unsafe { libc::poll(&mut watched, 1, 0) }, 1);
    }

    /// A full backlog is waited out, as Linux blocks on it, but a socket that
    /// nobody listens on is still reported refused once the window has passed.
    #[test]
    fn a_settled_connect_waits_out_a_full_backlog_but_not_a_dead_socket() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("listener.sock");
        let listener = super::bind(&path).unwrap();
        // SAFETY: listen takes the live listener descriptor and a scalar.
        assert_eq!(
            unsafe { libc::listen(listener.as_raw_fd(), super::ONE_PENDING_BACKLOG) },
            0
        );
        let _pending = super::connect(&path).unwrap();
        let accepter = thread::spawn(move || {
            thread::sleep(std::time::Duration::from_millis(20));
            let accepted = listener.accept().unwrap();
            (listener, accepted)
        });
        super::connect_settled(&path).unwrap();
        drop(accepter.join().unwrap());

        let started = std::time::Instant::now();
        let error = super::connect_settled(&path).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::ConnectionRefused);
        #[cfg(target_os = "macos")]
        assert!(started.elapsed() >= super::REFUSAL_WINDOW);
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
    }
}
