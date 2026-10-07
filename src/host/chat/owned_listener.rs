//! Whether a Codex conversation's own listener is still being served.
//!
//! Codex does not necessarily bind the path it is told to listen on. Recent
//! releases bind a socket in their own runtime directory and publish the
//! requested path as a symlink to it, and a server that is killed leaves both
//! behind. The entry's type therefore says nothing about whether anything is
//! listening; only connecting does.
use crate::agent::observe::socket;
use std::{
    io,
    os::unix::fs::{FileTypeExt, MetadataExt},
    path::Path,
};

/// Whether a server is accepting connections at this daemon-allocated endpoint.
///
/// An entry nothing listens behind is retired, so the conversation's server
/// can be launched again at the same path. Only the entry itself is unlinked,
/// and only inside this account's private allocation directory: a symlink's
/// target belongs to Codex, which reclaims it when it next starts there. An
/// endpoint that cannot be proven stopped is reported as served and is never
/// probed, leaving the join to accept or refuse it by its exact identity.
pub(super) fn served(endpoint: &Path) -> io::Result<bool> {
    let Ok(entry) = std::fs::symlink_metadata(endpoint) else {
        return Ok(false);
    };
    let account = unsafe { libc::geteuid() };
    let private = endpoint.parent().is_some_and(|directory| {
        std::fs::symlink_metadata(directory).is_ok_and(|metadata| {
            metadata.is_dir() && metadata.uid() == account && metadata.mode() & 0o777 == 0o700
        })
    });
    let kind = entry.file_type();
    if !private || entry.uid() != account || !(kind.is_socket() || kind.is_symlink()) {
        return Ok(true);
    }
    let stopped = match std::fs::metadata(endpoint) {
        Ok(bound) if bound.file_type().is_socket() => {
            socket::connect(endpoint, bound.dev(), bound.ino())
                .is_err_and(|failure| failure.not_listening)
        }
        Ok(_) => false,
        // A symlink whose target is gone refuses nothing, yet is just as stopped.
        Err(failure) => failure.kind() == io::ErrorKind::NotFound,
    };
    if !stopped {
        return Ok(true);
    }
    // Another start of this conversation may have retired the entry and
    // launched a server that published its own since the probe.
    let unchanged = std::fs::symlink_metadata(endpoint)
        .is_ok_and(|current| current.dev() == entry.dev() && current.ino() == entry.ino());
    if !unchanged {
        return Ok(true);
    }
    match std::fs::remove_file(endpoint) {
        Ok(()) => Ok(false),
        Err(failure) if failure.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(failure) => Err(failure),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::{
        fs::{DirBuilderExt, PermissionsExt, symlink},
        net::UnixListener,
    };

    /// A private allocation directory and the runtime directory Codex binds in.
    fn directories() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
        let root = tempfile::tempdir().unwrap();
        let allocation = root.path().join("cagents-codex-test");
        let runtime = root.path().join("codex-daemon");
        for directory in [&allocation, &runtime] {
            std::fs::DirBuilder::new()
                .mode(0o700)
                .create(directory)
                .unwrap();
        }
        (root, allocation, runtime)
    }

    #[test]
    fn an_unallocated_endpoint_is_not_served() {
        let (_root, allocation, _) = directories();
        assert!(!served(&allocation.join("control.sock")).unwrap());
    }

    #[test]
    fn a_stopped_socket_is_retired() {
        let (_root, allocation, _) = directories();
        let socket = allocation.join("control.sock");
        drop(UnixListener::bind(&socket).unwrap());
        assert!(!served(&socket).unwrap());
        assert!(std::fs::symlink_metadata(&socket).is_err());
    }

    #[test]
    fn a_symlink_to_a_stopped_socket_is_retired_without_touching_its_target() {
        let (_root, allocation, runtime) = directories();
        let socket = allocation.join("control.sock");
        let target = runtime.join("bound");
        drop(UnixListener::bind(&target).unwrap());
        symlink(&target, &socket).unwrap();
        assert!(!served(&socket).unwrap());
        assert!(std::fs::symlink_metadata(&socket).is_err());
        assert!(std::fs::symlink_metadata(&target).is_ok());
    }

    #[test]
    fn a_dangling_symlink_is_retired() {
        let (_root, allocation, runtime) = directories();
        let socket = allocation.join("control.sock");
        symlink(runtime.join("cleared"), &socket).unwrap();
        assert!(!served(&socket).unwrap());
        assert!(std::fs::symlink_metadata(&socket).is_err());
    }

    #[test]
    fn a_listening_server_is_kept_through_either_entry() {
        let (_root, allocation, runtime) = directories();
        let direct = allocation.join("control.sock");
        let _direct = UnixListener::bind(&direct).unwrap();
        assert!(served(&direct).unwrap());
        assert!(std::fs::symlink_metadata(&direct).is_ok());

        let target = runtime.join("bound");
        let _published = UnixListener::bind(&target).unwrap();
        let published = allocation.join("published.sock");
        symlink(&target, &published).unwrap();
        assert!(served(&published).unwrap());
        assert!(std::fs::symlink_metadata(&published).is_ok());
    }

    #[test]
    fn nothing_is_unlinked_outside_a_private_allocation() {
        let (_root, allocation, runtime) = directories();
        std::fs::set_permissions(&allocation, std::fs::Permissions::from_mode(0o755)).unwrap();
        let socket = allocation.join("control.sock");
        let target = runtime.join("bound");
        drop(UnixListener::bind(&target).unwrap());
        symlink(&target, &socket).unwrap();
        assert!(served(&socket).unwrap());
        assert!(std::fs::symlink_metadata(&socket).is_ok());
    }

    #[test]
    fn an_entry_that_is_not_an_endpoint_is_left_alone() {
        let (_root, allocation, _) = directories();
        let socket = allocation.join("control.sock");
        std::fs::write(&socket, b"").unwrap();
        assert!(served(&socket).unwrap());
        assert!(socket.is_file());
    }
}
