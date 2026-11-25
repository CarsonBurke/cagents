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
};

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

/// Connects a Unix-domain stream to `path`; see [`keep_options_after_shutdown`].
pub(crate) fn connect(path: &std::path::Path) -> io::Result<UnixStream> {
    let stream = UnixStream::connect(path)?;
    keep_options_after_shutdown(std::os::fd::AsFd::as_fd(&stream))?;
    Ok(stream)
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

#[cfg(target_os = "linux")]
fn path_of(file: &std::fs::File) -> io::Result<std::path::PathBuf> {
    Ok(format!("/proc/self/fd/{}", file.as_raw_fd()).into())
}

#[cfg(target_os = "macos")]
fn path_of(file: &std::fs::File) -> io::Result<std::path::PathBuf> {
    use std::os::unix::ffi::OsStringExt;
    let mut path = vec![0_u8; libc::PATH_MAX as usize];
    // SAFETY: F_GETPATH writes at most PATH_MAX bytes, NUL included, into
    // the buffer, which outlives the call.
    if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETPATH, path.as_mut_ptr()) } == -1 {
        return Err(io::Error::last_os_error());
    }
    let length = path
        .iter()
        .position(|&byte| byte == 0)
        .unwrap_or(path.len());
    path.truncate(length);
    Ok(std::ffi::OsString::from_vec(path).into())
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
}
