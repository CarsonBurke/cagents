//! The scope a local socket peer must use when requesting a daemon action.

#[cfg(target_os = "linux")]
use std::{fs, path::Path};
use std::{io, os::unix::net::UnixStream};

use super::{
    PeerCredentials, PinnedProcess,
    hosting::{MAX_ANCESTORS, daemon_executable},
};

/// Whether the local socket peer must present a session credential before
/// using user-scoped requests. Unavailable or changing kernel evidence
/// cannot grant user scope. Ancestry can only restrict a peer.
pub fn peer_requires_session_authority(socket: &UnixStream, peer: PeerCredentials) -> bool {
    !matches!(hosted_peer(socket, peer), Ok(false))
}

enum Ancestor {
    Pinned(PinnedProcess),
    #[cfg(target_os = "macos")]
    Foreign(crate::pinned_process::ForeignAncestor),
}

impl Ancestor {
    fn open(
        pid: u32,
        #[cfg_attr(target_os = "linux", allow(unused_variables))] uid: u32,
    ) -> io::Result<Self> {
        match PinnedProcess::open(pid) {
            Ok(process) => Ok(Self::Pinned(process)),
            #[cfg(target_os = "macos")]
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
                crate::pinned_process::ForeignAncestor::open(pid, uid).map(Self::Foreign)
            }
            Err(error) => Err(error),
        }
    }

    fn parent_pid(&self) -> io::Result<u32> {
        match self {
            Self::Pinned(process) => process.parent_pid(),
            #[cfg(target_os = "macos")]
            Self::Foreign(process) => process.parent_pid(),
        }
    }

    fn is_host(&self, uid: u32) -> io::Result<bool> {
        match self {
            Self::Pinned(process) => Ok(process.uid()? == uid && is_host(process)?),
            #[cfg(target_os = "macos")]
            Self::Foreign(_) => Ok(false),
        }
    }
}

fn hosted_peer(socket: &UnixStream, peer: PeerCredentials) -> io::Result<bool> {
    let uid = super::current_uid();
    if peer.uid != uid || peer.pid <= 1 {
        return Err(io::ErrorKind::PermissionDenied.into());
    }
    #[cfg(target_os = "linux")]
    if fs::read_link("/proc/self")? != Path::new(&std::process::id().to_string()) {
        return Err(io::Error::other(
            "procfs is not in the daemon's PID namespace",
        ));
    }
    let process = PinnedProcess::socket_peer(socket, peer.pid)?;
    if process.exited()? {
        return Err(io::ErrorKind::NotFound.into());
    }
    #[cfg(target_os = "linux")]
    if super::process_scope::in_session_scope(peer.pid)? {
        return Ok(true);
    }

    let mut process = Ancestor::Pinned(process);
    for _ in 0..MAX_ANCESTORS {
        let parent = process.parent_pid()?;
        if parent == std::process::id() {
            return Ok(true);
        }
        if parent == 1 {
            return Ok(false);
        }
        // A peer with invisible ancestry cannot prove that it is external.
        let ancestor = Ancestor::open(parent, uid)?;
        if process.parent_pid()? != parent {
            return Err(io::Error::other("the peer's ancestry changed"));
        }
        if ancestor.is_host(uid)? {
            return Ok(true);
        }
        process = ancestor;
    }
    Err(io::Error::other("the peer's ancestry is too deep"))
}

fn is_host(process: &PinnedProcess) -> io::Result<bool> {
    // The ancestor can be a retained holder launched by an earlier daemon.
    // A normal login ancestor such as sshd can deny executable lookup, so
    // also recognize the dedicated daemon and holder kernel process names.
    match process.executable() {
        Ok(path) if daemon_executable(&path) => return Ok(true),
        Ok(_) => {}
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::PermissionDenied | io::ErrorKind::NotFound
            ) => {}
        Err(error) => return Err(error),
    }
    Ok(matches!(
        process.command_name()?.as_slice(),
        b"cagentsd" | b"cagents-holder" | b"cagents-native"
    ))
}
