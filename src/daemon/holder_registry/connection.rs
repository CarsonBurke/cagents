//! Deadline-bound Unix connection, including a full listener backlog. A socket
//! read timeout does not bound connect(2), so connect itself must be nonblocking.
use std::{
    io,
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd},
        unix::{ffi::OsStrExt, net::UnixStream},
    },
    path::Path,
    time::{Duration, Instant},
};

fn remaining(deadline: Instant) -> io::Result<Duration> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|remaining| !remaining.is_zero())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::TimedOut,
                "Holder connection deadline expired",
            )
        })
}

pub(crate) fn connect_until(path: &Path, deadline: Instant) -> io::Result<UnixStream> {
    super::with_socket_address(path, |address| {
        let bytes = address.as_os_str().as_bytes();
        // with_socket_address also handles long filesystem paths through its
        // retained directory FD; this syscall address must itself fit.
        let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
        if bytes.contains(&0) || bytes.len() >= address.sun_path.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Invalid holder socket address",
            ));
        }
        address.sun_family = libc::AF_UNIX as libc::sa_family_t;
        for (target, byte) in address.sun_path.iter_mut().zip(bytes) {
            *target = *byte as libc::c_char;
        }
        let length = (std::mem::offset_of!(libc::sockaddr_un, sun_path) + bytes.len() + 1)
            as libc::socklen_t;
        loop {
            remaining(deadline)?;
            // SAFETY: a successful socket owns one new descriptor immediately
            // transferred to OwnedFd. CLOEXEC is set atomically.
            let descriptor = unsafe {
                libc::socket(
                    libc::AF_UNIX,
                    libc::SOCK_STREAM | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
                    0,
                )
            };
            if descriptor < 0 {
                return Err(io::Error::last_os_error());
            }
            let descriptor = unsafe { OwnedFd::from_raw_fd(descriptor) };
            let connected = unsafe {
                libc::connect(descriptor.as_raw_fd(), (&raw const address).cast(), length)
            };
            if connected != 0 {
                let error = io::Error::last_os_error();
                match error.raw_os_error() {
                    // Linux Unix sockets report EAGAIN when the listener's
                    // backlog is full without beginning a connection. Retry a
                    // fresh descriptor after a bounded pause, never blocking.
                    Some(libc::EAGAIN) => {
                        drop(descriptor);
                        std::thread::sleep(remaining(deadline)?.min(Duration::from_millis(5)));
                        continue;
                    }
                    Some(libc::EINTR) => continue,
                    Some(libc::EINPROGRESS) => loop {
                        let wait = remaining(deadline)?;
                        let milliseconds =
                            wait.as_millis().saturating_add(1).min(i32::MAX as u128) as i32;
                        let mut event = libc::pollfd {
                            fd: descriptor.as_raw_fd(),
                            events: libc::POLLOUT,
                            revents: 0,
                        };
                        let ready = unsafe { libc::poll(&mut event, 1, milliseconds) };
                        if ready == 0 {
                            continue;
                        }
                        if ready < 0 {
                            let error = io::Error::last_os_error();
                            if error.kind() == io::ErrorKind::Interrupted {
                                continue;
                            }
                            return Err(error);
                        }
                        let mut error: libc::c_int = 0;
                        let mut length = std::mem::size_of_val(&error) as libc::socklen_t;
                        if unsafe {
                            libc::getsockopt(
                                descriptor.as_raw_fd(),
                                libc::SOL_SOCKET,
                                libc::SO_ERROR,
                                (&raw mut error).cast(),
                                &raw mut length,
                            )
                        } != 0
                        {
                            return Err(io::Error::last_os_error());
                        }
                        if error != 0 {
                            return Err(io::Error::from_raw_os_error(error));
                        }
                        break;
                    },
                    _ => return Err(error),
                }
            }
            let stream = UnixStream::from(descriptor);
            stream.peer_addr()?;
            stream.set_nonblocking(false)?;
            return Ok(stream);
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;

    #[test]
    fn full_holder_backlog_obeys_deadline_without_disturbing_original_connection() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("holder");
        let listener = UnixListener::bind(&path).unwrap();
        assert_eq!(unsafe { libc::listen(listener.as_raw_fd(), 0) }, 0);
        let original = UnixStream::connect(&path).unwrap();
        let start = Instant::now();
        let error = connect_until(&path, start + Duration::from_millis(80)).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(start.elapsed() < Duration::from_secs(2));
        let (_accepted, _) = listener.accept().unwrap();
        assert!(original.peer_addr().is_ok());
    }

    #[test]
    fn connected_stream_is_blocking_and_close_on_exec_and_expired_deadline_has_no_connection() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("holder");
        let listener = UnixListener::bind(&path).unwrap();
        let connection = connect_until(&path, Instant::now() + Duration::from_secs(1)).unwrap();
        let (_accepted, _) = listener.accept().unwrap();
        assert_ne!(
            unsafe { libc::fcntl(connection.as_raw_fd(), libc::F_GETFD) } & libc::FD_CLOEXEC,
            0
        );
        assert_eq!(
            unsafe { libc::fcntl(connection.as_raw_fd(), libc::F_GETFL) } & libc::O_NONBLOCK,
            0
        );
        assert_eq!(
            connect_until(&path, Instant::now()).unwrap_err().kind(),
            io::ErrorKind::TimedOut
        );
    }
}
