//! Shared native-chat/observer endpoint and account identity fences.
use serde_json::Value;
use std::{
    io,
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::{
            fs::{FileTypeExt, MetadataExt},
            net::UnixStream,
        },
    },
    path::Path,
};

/// Why an exact Codex endpoint was not joined.
#[derive(Debug)]
pub(crate) struct JoinError {
    message: String,
    /// Nothing is accepting connections there. A server that is still
    /// starting binds its socket before it listens, so this is not final
    /// for a listener whose launch is being awaited.
    pub(crate) not_listening: bool,
}

impl From<String> for JoinError {
    fn from(message: String) -> Self {
        Self {
            message,
            not_listening: false,
        }
    }
}

impl From<&str> for JoinError {
    fn from(message: &str) -> Self {
        message.to_owned().into()
    }
}

impl From<JoinError> for String {
    fn from(error: JoinError) -> Self {
        error.message
    }
}

fn verify_socket(path: &Path, device: u64, inode: u64) -> Result<(), String> {
    let metadata = std::fs::metadata(path).map_err(|error| {
        format!(
            "Cannot verify exact Codex socket {}: {error}",
            path.display()
        )
    })?;
    if !metadata.file_type().is_socket()
        || metadata.dev() != device
        || metadata.ino() != inode
        || metadata.uid() != unsafe { libc::geteuid() }
    {
        return Err("Codex socket identity or owner changed; refusing another daemon".into());
    }
    Ok(())
}

pub(crate) fn connect(path: &Path, device: u64, inode: u64) -> Result<UnixStream, JoinError> {
    verify_socket(path, device, inode)?;
    let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    address.sun_family = libc::AF_UNIX as _;
    let bytes = path.as_os_str().as_encoded_bytes();
    if bytes.is_empty() || bytes.len() >= address.sun_path.len() || bytes.contains(&0) {
        return Err("Invalid Codex Unix socket path".into());
    }
    for (target, byte) in address.sun_path.iter_mut().zip(bytes) {
        *target = *byte as _;
    }
    let fd = unsafe {
        libc::socket(
            libc::AF_UNIX,
            libc::SOCK_STREAM | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
            0,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error().to_string().into());
    }
    let stream = unsafe { UnixStream::from_raw_fd(fd) };
    let result = unsafe {
        libc::connect(
            fd,
            (&address as *const libc::sockaddr_un).cast(),
            (std::mem::offset_of!(libc::sockaddr_un, sun_path) + bytes.len() + 1) as _,
        )
    };
    if result < 0 {
        // No retry on backlog pressure or endpoint replacement.
        let failure = io::Error::last_os_error();
        return Err(JoinError {
            message: format!(
                "Cannot join existing Codex socket {}: {failure}",
                path.display()
            ),
            not_listening: failure.kind() == io::ErrorKind::ConnectionRefused,
        });
    }
    verify_socket(path, device, inode)?;
    let mut peer: libc::ucred = unsafe { std::mem::zeroed() };
    let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let result = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut peer as *mut libc::ucred).cast(),
            &mut length,
        )
    };
    if result < 0
        || length as usize != std::mem::size_of::<libc::ucred>()
        || peer.uid != unsafe { libc::geteuid() }
    {
        return Err("Cannot verify Codex daemon peer account".into());
    }
    Ok(stream)
}

pub(crate) fn verify_account(result: &Value, expected_home: &Path) -> Result<(), String> {
    if !result["userAgent"].is_string() {
        return Err("Unsupported Codex initialize response".into());
    }
    let home = result["codexHome"]
        .as_str()
        .ok_or("Missing Codex account directory")?;
    let server_home = std::fs::canonicalize(home)
        .map_err(|error| format!("Cannot verify connected Codex account directory: {error}"))?;
    if server_home != expected_home {
        return Err("The selected Codex daemon belongs to a different CODEX_HOME".into());
    }
    Ok(())
}
