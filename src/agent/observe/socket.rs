//! Shared native-chat/observer endpoint and account identity fences.
use serde_json::Value;
use std::{
    io,
    os::{
        fd::AsRawFd,
        unix::{
            fs::{FileTypeExt, MetadataExt},
            net::UnixStream,
        },
    },
    path::Path,
};

/// Why an exact Codex endpoint was not joined.
#[derive(Debug)]
pub struct JoinError {
    message: String,
    /// Nothing is accepting connections there. A server that is still
    /// starting binds its socket before it listens, so this is not final
    /// for a listener whose launch is being awaited.
    ///
    /// On Darwin it is inferred from refusals that persist, which a listener
    /// with a full backlog also gives; see `descriptor::retry_refusal`.
    pub not_listening: bool,
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

pub fn connect(path: &Path, device: u64, inode: u64) -> Result<UnixStream, JoinError> {
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
    let length = (std::mem::offset_of!(libc::sockaddr_un, sun_path) + bytes.len() + 1) as _;
    let refusing_since = std::time::Instant::now();
    let stream = loop {
        let stream = UnixStream::from(
            crate::descriptor::unix_socket(libc::SOCK_STREAM, true)
                .map_err(|error| JoinError::from(error.to_string()))?,
        );
        // SAFETY: the address is initialized up to `length`, and the
        // descriptor is the live socket `stream` owns.
        let result = unsafe {
            libc::connect(
                stream.as_raw_fd(),
                (&address as *const libc::sockaddr_un).cast(),
                length,
            )
        };
        if result == 0 {
            break stream;
        }
        let failure = io::Error::last_os_error();
        // No retry on backlog pressure or endpoint replacement, except for
        // the refusals Darwin also gives a full backlog.
        if crate::descriptor::retry_refusal(&failure, refusing_since) {
            std::thread::sleep(std::time::Duration::from_millis(5));
            continue;
        }
        return Err(JoinError {
            message: format!(
                "Cannot join existing Codex socket {}: {failure}",
                path.display()
            ),
            not_listening: failure.kind() == io::ErrorKind::ConnectionRefused,
        });
    };
    verify_socket(path, device, inode)?;
    if crate::pinned_process::peer_uid(&stream).ok() != Some(crate::daemon::current_uid()) {
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
