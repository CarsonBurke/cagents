//! Where a Codex conversation's own listener lives, and whether it is still
//! being served.
//!
//! Codex does not necessarily bind the path it is told to listen on. Recent
//! releases bind a socket in their own runtime directory and publish the
//! requested path as a symlink to it, and a server that is killed leaves both
//! behind. The entry's type therefore says nothing about whether anything is
//! listening; only connecting does.
//!
//! That published symlink is also why the listener's location matters. The
//! system's temporary-file cleaner ages a symlink and its emptied directory
//! like any other file; it spares only a live socket. Earlier releases
//! allocated listeners in the daemon's temporary directory, where a server
//! that ran for weeks lost its endpoint and could no longer be joined or
//! handed to an update (#2436). New listeners live under the daemon's runtime
//! directory, which nothing ages, and the ones still served from the old
//! location are kept fresh until their conversations end.
use crate::agent::observe::socket;
use std::{
    io,
    os::unix::{
        ffi::OsStrExt,
        fs::{FileTypeExt, MetadataExt},
    },
    path::{Path, PathBuf},
};

/// The directories in which this daemon allocates Codex listeners.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Roots {
    /// Where new listeners are allocated.
    current: PathBuf,
    /// Where earlier releases allocated them. Conversations allocated there
    /// keep their listener, because its path is part of the recipe every
    /// window was issued; this daemon still recognizes it as its own.
    legacy: PathBuf,
}

impl Roots {
    /// The daemon's own layout: new listeners under its runtime directory,
    /// unless a listener there would not fit in a Unix socket address. A
    /// development instance's runtime directory can be that long; its
    /// listeners stay in the temporary directory, where they are refreshed.
    pub(crate) fn in_runtime_directory(runtime: &Path) -> Self {
        let legacy = std::env::temp_dir();
        let current = runtime.join("codex");
        let longest = current
            .join(format!("cagents-codex-{}", crate::model::TerminalId::new()))
            .join("control.sock");
        if longest.as_os_str().len() > SOCKET_PATH_MAX {
            tracing::warn!(
                runtime = %runtime.display(),
                "the runtime directory is too long for Codex listener sockets; allocating them \
                 in the temporary directory instead"
            );
            return Self::at(legacy);
        }
        Self { current, legacy }
    }

    /// Allocates everything at `root`, as releases before #2436 did. Tests
    /// that model no runtime directory use it.
    pub(crate) fn at(root: PathBuf) -> Self {
        Self {
            current: root.clone(),
            legacy: root,
        }
    }

    fn each(&self) -> impl Iterator<Item = &Path> {
        std::iter::once(self.current.as_path())
            .chain((self.legacy != self.current).then_some(self.legacy.as_path()))
    }

    /// Points a Codex thread's `--remote` at its own listener: a new thread
    /// at the current root, and a legacy recipe only at the exact path an
    /// earlier release of this daemon allocated for it.
    pub(crate) fn claim(
        &self,
        session: &mut crate::chat::Session,
        allocator: &crate::model::HostSessionProof,
    ) -> bool {
        self.each()
            .any(|root| session.claim_owned_codex_listener(root, allocator))
    }

    /// Whether `socket` is the listener this daemon allocates for `id`.
    pub(crate) fn allocates(&self, id: crate::model::TerminalId, socket: &Path) -> bool {
        self.each().any(|root| {
            socket
                == root
                    .join(format!("cagents-codex-{id}"))
                    .join("control.sock")
        })
    }
}

/// The longest path a Unix socket address holds, less its terminating NUL.
const SOCKET_PATH_MAX: usize = 107;

/// How often listeners in the legacy location are refreshed. The cleaner's
/// shortest default age is ten days, for /tmp; /var/tmp's is thirty.
pub(crate) const REFRESH_INTERVAL: std::time::Duration =
    std::time::Duration::from_secs(6 * 60 * 60);

/// Marks a served listener's published entry and its allocation directory as
/// just used, so the temporary-file cleaner does not take them from a server
/// that is still running. Only an entry inside this account's private
/// allocation directory is touched, never what a symlink points at.
pub(crate) fn refresh(endpoint: &Path) -> io::Result<()> {
    let Some(directory) = endpoint.parent() else {
        return Ok(());
    };
    if !private_allocation(directory) {
        return Ok(());
    }
    match std::fs::symlink_metadata(endpoint) {
        Ok(entry) if entry.uid() == unsafe { libc::geteuid() } => touch(endpoint)?,
        Ok(_) => return Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    touch(directory)
}

/// Whether `directory` is an allocation only this account can change: a real
/// directory it owns with no access for anyone else.
fn private_allocation(directory: &Path) -> bool {
    std::fs::symlink_metadata(directory).is_ok_and(|metadata| {
        metadata.is_dir()
            && metadata.uid() == unsafe { libc::geteuid() }
            && metadata.mode() & 0o777 == 0o700
    })
}

/// Refreshes every listener still served from the daemon's temporary
/// directory, now and every [`REFRESH_INTERVAL`]. The thread ends at the first
/// wake after the host is dropped.
///
/// It reads the directory rather than this daemon's records, so it also keeps
/// the listener of a retained server that a restore left unattached, which
/// has no record here but is still the endpoint its next start joins.
pub(crate) fn keep_legacy_listeners_fresh(host: &crate::host::state::SharedHost) -> io::Result<()> {
    let host = std::sync::Arc::downgrade(host);
    std::thread::Builder::new()
        .name("cagents-codex-listeners".into())
        .spawn(move || {
            while let Some(host) = host.upgrade() {
                let root = host
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .codex_listeners
                    .legacy
                    .clone();
                drop(host);
                if let Err(error) = refresh_served(&root) {
                    tracing::warn!(
                        %error,
                        root = %root.display(),
                        "Codex listeners in the temporary directory could not be kept fresh"
                    );
                }
                std::thread::sleep(REFRESH_INTERVAL);
            }
        })
        .map(drop)
}

/// Refreshes each listener allocation in `root` whose server still accepts
/// connections. One that stopped is left to age; its next start retires it.
fn refresh_served(root: &Path) -> io::Result<()> {
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                tracing::warn!(
                    %error,
                    root = %root.display(),
                    "a temporary directory entry could not be read"
                );
                continue;
            }
        };
        if !entry.file_name().as_bytes().starts_with(b"cagents-codex-") {
            continue;
        }
        // Only this account's private allocations are ever connected to: an
        // endpoint anyone else could place must not choose what is dialed.
        let directory = entry.path();
        let endpoint = directory.join("control.sock");
        if private_allocation(&directory) && listening(&endpoint) {
            if let Err(error) = refresh(&endpoint) {
                tracing::warn!(
                    %error,
                    endpoint = %endpoint.display(),
                    "a Codex listener in the temporary directory could not be kept fresh"
                );
            }
        }
    }
    Ok(())
}

/// Whether a server accepts connections at `endpoint`, or might: only a
/// refused connection proves it stopped.
fn listening(endpoint: &Path) -> bool {
    match std::fs::metadata(endpoint) {
        Ok(bound) if bound.file_type().is_socket() => {
            !socket::connect(endpoint, bound.dev(), bound.ino())
                .is_err_and(|failure| failure.not_listening)
        }
        _ => false,
    }
}

/// Sets an entry's access and modification times to now without following it.
fn touch(path: &Path) -> io::Result<()> {
    let path = std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
    // SAFETY: `path` is a NUL-terminated string that outlives the call, and a
    // null `times` sets both timestamps to the current time.
    let result = unsafe {
        libc::utimensat(
            libc::AT_FDCWD,
            path.as_ptr(),
            std::ptr::null(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Whether a server is accepting connections at this daemon-allocated endpoint.
///
/// An entry nothing listens behind is retired, so the conversation's server
/// can be launched again at the same path. Only the entry itself is unlinked,
/// and only inside this account's private allocation directory: a symlink's
/// target belongs to Codex, which reclaims it when it next starts there. An
/// endpoint that cannot be proven stopped is reported as served and is never
/// probed, leaving the join to accept or refuse it by its exact identity.
pub(super) fn served(endpoint: &Path) -> io::Result<bool> {
    served_before_retire(endpoint, || Ok(()))
}

/// Check any additional execution evidence before retiring a stopped endpoint.
/// In particular, a published symlink must remain available to that proof.
pub(super) fn served_before_retire(
    endpoint: &Path,
    before_retire: impl FnOnce() -> io::Result<()>,
) -> io::Result<bool> {
    let Ok(entry) = std::fs::symlink_metadata(endpoint) else {
        before_retire()?;
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
    before_retire()?;
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

    fn codex(arguments: &[&str], session_id: Option<&str>) -> crate::chat::Session {
        crate::chat::Session {
            native_continuity: Default::default(),
            owned_codex_listener: None,
            id: crate::model::TerminalId::new(),
            launch: crate::chat::Launch {
                provider: crate::agent::Provider::Codex,
                recipe: crate::model::LaunchRecipe {
                    unset_environment: Default::default(),
                    program: "codex".into(),
                    arguments: arguments.iter().map(|&argument| argument.into()).collect(),
                    working_directory: "/home/user".into(),
                    environment: Default::default(),
                },
                session_id: session_id.map(str::to_owned),
            },
            proof: None,
            terminal: None,
        }
    }

    fn allocator() -> crate::model::HostSessionProof {
        crate::model::HostSessionProof {
            host_session_id: "daemon".into(),
            boot_id: "boot".into(),
            host_incarnation: "incarnation".into(),
        }
    }

    fn layout() -> Roots {
        Roots {
            current: "/run/user/1000/cagents/codex".into(),
            legacy: "/var/tmp".into(),
        }
    }

    #[test]
    fn a_new_listener_is_allocated_under_the_runtime_directory() {
        let roots = Roots::in_runtime_directory(Path::new("/run/user/1000/cagents"));
        assert_eq!(roots.current, Path::new("/run/user/1000/cagents/codex"));
        assert_eq!(roots.legacy, std::env::temp_dir());

        let roots = layout();
        let mut new = codex(&["--remote", "unix://"], None);
        assert!(roots.claim(&mut new, &allocator()));
        let socket = Path::new("/run/user/1000/cagents/codex")
            .join(format!("cagents-codex-{}", new.id))
            .join("control.sock");
        assert_eq!(new.owned_codex_listener.as_ref().unwrap().socket, socket);
        assert_eq!(
            new.launch.recipe.arguments[1],
            format!("unix://{}", socket.display())
        );
        assert!(roots.allocates(new.id, &socket));
    }

    #[test]
    fn a_conversation_allocated_in_the_temporary_directory_keeps_its_listener() {
        let roots = layout();
        let mut legacy = codex(&[], Some("thread"));
        let socket = Path::new("/var/tmp")
            .join(format!("cagents-codex-{}", legacy.id))
            .join("control.sock");
        legacy.launch.recipe.arguments =
            vec!["--remote".into(), format!("unix://{}", socket.display())];
        let recipe = legacy.launch.recipe.clone();
        assert!(roots.claim(&mut legacy, &allocator()));
        assert_eq!(legacy.owned_codex_listener.as_ref().unwrap().socket, socket);
        assert_eq!(
            legacy.launch.recipe, recipe,
            "its issued recipe is unchanged"
        );
        assert!(roots.allocates(legacy.id, &socket));

        // Neither root lends its authority to a path outside it.
        let mut unrelated = codex(&[], Some("thread"));
        unrelated.launch.recipe.arguments = vec![
            "--remote".into(),
            format!(
                "unix:///elsewhere/cagents-codex-{}/control.sock",
                unrelated.id
            ),
        ];
        assert!(!roots.claim(&mut unrelated, &allocator()));
        assert!(unrelated.owned_codex_listener.is_none());
        assert!(
            !roots.allocates(
                unrelated.id,
                &Path::new("/elsewhere")
                    .join(format!("cagents-codex-{}", unrelated.id))
                    .join("control.sock")
            )
        );
    }

    #[test]
    fn a_runtime_directory_too_long_for_a_socket_keeps_the_temporary_directory() {
        let fits = Path::new("/run/user/1000/cagents-development-ab");
        assert_eq!(
            Roots::in_runtime_directory(fits).current,
            fits.join("codex")
        );
        // <runtime>/codex/cagents-codex-<36-byte id>/control.sock
        let below = "/codex/cagents-codex-".len() + 36 + "/control.sock".len();
        let longest = "/".to_owned() + &"r".repeat(SOCKET_PATH_MAX - below - 1);
        assert_eq!(
            Roots::in_runtime_directory(Path::new(&longest)).current,
            Path::new(&longest).join("codex")
        );
        let too_long = longest + "x";
        assert_eq!(
            Roots::in_runtime_directory(Path::new(&too_long)),
            Roots::at(std::env::temp_dir())
        );
    }

    #[test]
    fn only_served_allocations_in_the_temporary_directory_are_refreshed() {
        let (root, served_allocation, runtime) = directories();
        let target = runtime.join("bound");
        let _server = UnixListener::bind(&target).unwrap();
        let served_endpoint = served_allocation.join("control.sock");
        symlink(&target, &served_endpoint).unwrap();

        let stopped_allocation = root.path().join("cagents-codex-stopped");
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&stopped_allocation)
            .unwrap();
        let stopped_target = runtime.join("stopped");
        drop(UnixListener::bind(&stopped_target).unwrap());
        symlink(&stopped_target, stopped_allocation.join("control.sock")).unwrap();

        let unrelated = root.path().join("unrelated");
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&unrelated)
            .unwrap();
        let unrelated_target = runtime.join("unrelated");
        let _unrelated_server = UnixListener::bind(&unrelated_target).unwrap();
        symlink(&unrelated_target, unrelated.join("control.sock")).unwrap();

        // Anyone could place an allocation-named directory that is not
        // private; the scan neither touches it nor dials what it names.
        let open = root.path().join("cagents-codex-open");
        std::fs::DirBuilder::new()
            .mode(0o755)
            .create(&open)
            .unwrap();
        std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o755)).unwrap();
        let open_target = runtime.join("open");
        let open_server = UnixListener::bind(&open_target).unwrap();
        open_server.set_nonblocking(true).unwrap();
        symlink(&open_target, open.join("control.sock")).unwrap();

        for directory in [&served_allocation, &stopped_allocation, &unrelated, &open] {
            age(directory);
        }
        refresh_served(root.path()).unwrap();
        assert_eq!(modified(&open), std::time::SystemTime::UNIX_EPOCH);
        assert_eq!(
            open_server.accept().unwrap_err().kind(),
            io::ErrorKind::WouldBlock,
            "an endpoint outside a private allocation is never connected to"
        );
        let recent = std::time::SystemTime::now() - std::time::Duration::from_secs(60);
        assert!(modified(&served_allocation) > recent);
        assert_eq!(
            modified(&stopped_allocation),
            std::time::SystemTime::UNIX_EPOCH
        );
        assert_eq!(modified(&unrelated), std::time::SystemTime::UNIX_EPOCH);
        assert!(
            std::fs::symlink_metadata(stopped_allocation.join("control.sock")).is_ok(),
            "refreshing retires nothing"
        );
        refresh_served(&root.path().join("missing")).unwrap();
    }

    fn modified(path: &Path) -> std::time::SystemTime {
        std::fs::symlink_metadata(path).unwrap().modified().unwrap()
    }

    fn age(path: &Path) {
        let old = std::fs::FileTimes::new()
            .set_accessed(std::time::SystemTime::UNIX_EPOCH)
            .set_modified(std::time::SystemTime::UNIX_EPOCH);
        std::fs::File::open(path).unwrap().set_times(old).unwrap();
    }

    #[test]
    fn refreshing_a_published_listener_touches_its_entry_but_not_its_target() {
        let (_root, allocation, runtime) = directories();
        let target = runtime.join("bound");
        let _server = UnixListener::bind(&target).unwrap();
        let published = allocation.join("control.sock");
        symlink(&target, &published).unwrap();
        // A symlink's own times can only be set without following it.
        let epoch = std::ffi::CString::new(published.as_os_str().as_bytes()).unwrap();
        let times = [libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        }; 2];
        assert_eq!(
            unsafe {
                libc::utimensat(
                    libc::AT_FDCWD,
                    epoch.as_ptr(),
                    times.as_ptr(),
                    libc::AT_SYMLINK_NOFOLLOW,
                )
            },
            0
        );
        age(&allocation);
        let target_before = std::fs::metadata(&target).unwrap().modified().unwrap();
        assert_eq!(modified(&published), std::time::SystemTime::UNIX_EPOCH);

        refresh(&published).unwrap();
        let recent = std::time::SystemTime::now() - std::time::Duration::from_secs(60);
        assert!(modified(&published) > recent);
        assert!(modified(&allocation) > recent);
        assert_eq!(
            std::fs::metadata(&target).unwrap().modified().unwrap(),
            target_before
        );
        assert!(served(&published).unwrap(), "the server is still reachable");
    }

    #[test]
    fn refreshing_leaves_anything_outside_a_private_allocation_alone() {
        let (_root, allocation, _) = directories();
        let socket = allocation.join("control.sock");
        std::fs::write(&socket, b"").unwrap();
        age(&socket);
        std::fs::set_permissions(&allocation, std::fs::Permissions::from_mode(0o755)).unwrap();
        age(&allocation);
        refresh(&socket).unwrap();
        assert_eq!(modified(&socket), std::time::SystemTime::UNIX_EPOCH);
        assert_eq!(modified(&allocation), std::time::SystemTime::UNIX_EPOCH);
        // A conversation whose server already retired its entry still keeps
        // its allocation directory from being aged.
        std::fs::set_permissions(&allocation, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::remove_file(&socket).unwrap();
        age(&allocation);
        refresh(&socket).unwrap();
        assert!(
            modified(&allocation)
                > std::time::SystemTime::now() - std::time::Duration::from_secs(60)
        );
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
