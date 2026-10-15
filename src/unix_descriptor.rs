//! Darwin descriptor setup using its native socket, pipe and fcntl APIs.
//!
//! Darwin has no pipe2 or SOCK_CLOEXEC/SOCK_NONBLOCK creation flags. These
//! descriptors are fully configured before publication to callers, and every
//! configuration error closes both owned ends. Creation and fcntl are NOT an
//! atomic operation: qualifying concurrent fork/exec inheritance requires the
//! platform spawn policy, separately from these post-return flag guarantees.
use std::{
    fs::File,
    io,
    os::{fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd}, unix::net::UnixStream},
};

fn configure(descriptor: BorrowedFd<'_>) -> io::Result<()> {
    let raw = descriptor.as_raw_fd();
    // SAFETY: the borrowed descriptor stays owned throughout these fcntl calls.
    let descriptor_flags = unsafe { libc::fcntl(raw, libc::F_GETFD) };
    if descriptor_flags == -1 {
        return Err(io::Error::last_os_error());
    }
    if unsafe { libc::fcntl(raw, libc::F_SETFD, descriptor_flags | libc::FD_CLOEXEC) } == -1 {
        return Err(io::Error::last_os_error());
    }
    let status_flags = unsafe { libc::fcntl(raw, libc::F_GETFL) };
    if status_flags == -1 {
        return Err(io::Error::last_os_error());
    }
    if unsafe { libc::fcntl(raw, libc::F_SETFL, status_flags | libc::O_NONBLOCK) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub(crate) fn nonblocking_socket() -> io::Result<UnixStream> {
    // SAFETY: socket creates one fresh descriptor, owned immediately on success.
    let raw = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0) };
    if raw == -1 {
        return Err(io::Error::last_os_error());
    }
    let stream = unsafe { UnixStream::from_raw_fd(raw) };
    configure(stream.as_fd())?;
    Ok(stream)
}

pub(crate) fn nonblocking_pipe() -> io::Result<(File, File)> {
    let mut descriptors = [-1; 2];
    // SAFETY: pipe initializes both entries on success. Both become owned
    // before any fallible configuration, so partial setup cannot leak an end.
    if unsafe { libc::pipe(descriptors.as_mut_ptr()) } == -1 {
        return Err(io::Error::last_os_error());
    }
    let reader = unsafe { File::from_raw_fd(descriptors[0]) };
    let writer = unsafe { File::from_raw_fd(descriptors[1]) };
    configure(reader.as_fd())?;
    configure(writer.as_fd())?;
    Ok((reader, writer))
}
