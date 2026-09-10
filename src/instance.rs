use std::{
    env,
    fs::{self, File, OpenOptions},
    io::{self, BufRead, BufReader, Read, Write},
    os::{
        fd::AsRawFd,
        unix::{
            fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
            net::UnixStream,
        },
    },
    path::{Path, PathBuf},
    process,
    time::Duration,
};
#[cfg(feature = "ui")]
use std::{
    os::unix::{fs::FileTypeExt, net::UnixListener},
    thread::{self, JoinHandle},
};

use directories::ProjectDirs;
use thiserror::Error;

const APPLICATION_LOCK_FILE: &str = if IS_DEVELOPMENT {
    "cagents-development-ui.lock"
} else {
    "cagents-ui.lock"
};
const ACTIVATION_SOCKET_FILE: &str = if IS_DEVELOPMENT {
    "cagents-development-ui.activate"
} else {
    "cagents-ui.activate"
};
const ACTIVATION_IO_TIMEOUT: Duration = Duration::from_secs(1);
const MAX_ACTIVATION_LINE_BYTES: usize = 4096;
const PRIVATE_FILE_MODE: u32 = 0o600;
const LEASE_CAPABLE_PROCESS_NAME: &str = "cagents-ui-v2";
const LEGACY_PROCESS_NAME: &str = "cagents-ui";
const LEASE_MARKER_FILE: &str = ".cagents-ui-lease-v1";
// `full` names the product that ships the embedded browser: it claims that
// package's application id, window class, and peer directory. A UI built with
// `full` but without `integrated-browser` would claim them while running the
// browserless code paths, so refuse the combination instead of shipping an
// identity the binary cannot honour.
#[cfg(all(feature = "ui", feature = "full", not(feature = "integrated-browser")))]
compile_error!(
    "a `full` UI must select `integrated-browser`; build the browserless UI with \
     --no-default-features --features ui"
);
#[cfg(feature = "full")]
const PEER_PACKAGE_DIRECTORY: &str = "/usr/libexec/cagents-browserless";
#[cfg(not(feature = "full"))]
const PEER_PACKAGE_DIRECTORY: &str = "/usr/libexec/cagents";
pub const UI_LAUNCH_HANDSHAKE_ENV: &str = "CAGENTS_UI_LAUNCH_HANDSHAKE";
pub const UI_LAUNCH_READY: &str = "ready";
pub const UI_LAUNCH_ERROR_PREFIX: &str = "error: ";

#[derive(Debug, Error)]
pub enum ApplicationLeaseError {
    #[error("XDG_RUNTIME_DIR is not set to an absolute path")]
    MissingRuntimeDirectory,
    #[error("runtime path {path:?} is not an owned directory")]
    UnsafeRuntimeDirectory { path: PathBuf },
    #[error("application lock path {path:?} is not an owned regular file")]
    UnsafeLockPath { path: PathBuf },
    #[error("another cagents UI is already using the shared session store")]
    AlreadyRunning,
    #[error(
        "installed peer UI at {path:?} predates the shared-session lease; upgrade that package first"
    )]
    IncompatiblePeerPackage { path: PathBuf },
    #[error("failed to acquire cagents application lock: {0}")]
    Io(#[from] io::Error),
}

/// Exclusive process lease protecting this instance's durable session store.
pub struct ApplicationLease {
    _lock: File,
}

impl Drop for ApplicationLease {
    fn drop(&mut self) {
        // A concurrently forked child can retain this description until exec.
        // SAFETY: the lease owns the live locked descriptor throughout Drop.
        let _ = unsafe { libc::flock(self._lock.as_raw_fd(), libc::LOCK_UN) };
    }
}

impl ApplicationLease {
    pub fn acquire() -> Result<Self, ApplicationLeaseError> {
        if !IS_DEVELOPMENT {
            let peer_directory = Path::new(PEER_PACKAGE_DIRECTORY);
            if peer_ui_predates_lease(peer_directory) {
                return Err(ApplicationLeaseError::IncompatiblePeerPackage {
                    path: peer_directory.join("cagents-ui"),
                });
            }
        }
        mark_lease_capable_process()?;
        let runtime_directory = env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
            .ok_or(ApplicationLeaseError::MissingRuntimeDirectory)?;
        let lease = Self::acquire_in(&runtime_directory)?;
        if !IS_DEVELOPMENT && legacy_ui_process_running(Path::new("/proc"))? {
            return Err(ApplicationLeaseError::AlreadyRunning);
        }
        Ok(lease)
    }

    /// Acquires the lease and reports the result to the short-lived CLI
    /// launcher when this process was spawned through `cagents ui`.
    pub fn acquire_for_ui() -> Result<Self, ApplicationLeaseError> {
        let outcome = Self::acquire();
        if env::var_os(UI_LAUNCH_HANDSHAKE_ENV).as_deref() == Some("1".as_ref()) {
            let message = match &outcome {
                Ok(_) => UI_LAUNCH_READY.to_owned(),
                Err(error) => format!("{UI_LAUNCH_ERROR_PREFIX}{error}").replace('\n', " "),
            };
            let mut stdout = io::stdout().lock();
            writeln!(stdout, "{message}")?;
            stdout.flush()?;
        }
        outcome
    }

    fn acquire_in(runtime_directory: &Path) -> Result<Self, ApplicationLeaseError> {
        let metadata =
            fs::symlink_metadata(runtime_directory).map_err(ApplicationLeaseError::Io)?;
        if !metadata.is_dir() || metadata.uid() != current_uid() {
            return Err(ApplicationLeaseError::UnsafeRuntimeDirectory {
                path: runtime_directory.to_path_buf(),
            });
        }

        let lock_path = runtime_directory.join(APPLICATION_LOCK_FILE);
        let lock = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .mode(PRIVATE_FILE_MODE)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&lock_path)?;
        let metadata = lock.metadata()?;
        if !metadata.is_file() || metadata.uid() != current_uid() {
            return Err(ApplicationLeaseError::UnsafeLockPath { path: lock_path });
        }
        lock.set_permissions(fs::Permissions::from_mode(PRIVATE_FILE_MODE))?;
        // SAFETY: flock receives a live file descriptor and no pointer data.
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::WouldBlock {
                return Err(ApplicationLeaseError::AlreadyRunning);
            }
            return Err(ApplicationLeaseError::Io(error));
        }

        Ok(Self { _lock: lock })
    }
}

/// A later `cagents ui` invocation asking the running window to come forward.
#[cfg(feature = "ui")]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UiActivation {
    pub token: Option<String>,
}

/// Owns the per-instance activation socket used by `cagents ui` to focus an
/// already-running window instead of failing the exclusive session lease.
#[cfg(feature = "ui")]
pub struct UiActivationListener {
    socket_path: PathBuf,
    thread: Option<JoinHandle<()>>,
}

#[cfg(feature = "ui")]
impl UiActivationListener {
    pub fn bind(events: async_channel::Sender<UiActivation>) -> io::Result<Self> {
        let runtime_directory = env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    "XDG_RUNTIME_DIR is not set to an absolute path",
                )
            })?;
        Self::bind_in(&runtime_directory, events)
    }

    fn bind_in(
        runtime_directory: &Path,
        events: async_channel::Sender<UiActivation>,
    ) -> io::Result<Self> {
        require_owned_runtime_directory(runtime_directory)?;

        let socket_path = runtime_directory.join(ACTIVATION_SOCKET_FILE);
        prepare_activation_socket_path(&socket_path)?;
        let listener = UnixListener::bind(&socket_path)?;
        // SAFETY: fchmod receives the bound listener's live descriptor.
        if unsafe { libc::fchmod(listener.as_raw_fd(), PRIVATE_FILE_MODE) } != 0 {
            let error = io::Error::last_os_error();
            let _ = fs::remove_file(&socket_path);
            return Err(error);
        }
        let thread = match thread::Builder::new()
            .name("cagents-ui-activate".into())
            .spawn(move || serve_activation_socket(listener, events))
        {
            Ok(thread) => thread,
            Err(error) => {
                let _ = fs::remove_file(&socket_path);
                return Err(error);
            }
        };
        Ok(Self {
            socket_path,
            thread: Some(thread),
        })
    }
}

#[cfg(feature = "ui")]
impl Drop for UiActivationListener {
    fn drop(&mut self) {
        let woke = UnixStream::connect(&self.socket_path)
            .and_then(|mut stream| {
                stream.set_write_timeout(Some(ACTIVATION_IO_TIMEOUT))?;
                writeln!(stream, "shutdown")
            })
            .is_ok();
        if let Some(thread) = self.thread.take() {
            if woke {
                let _ = thread.join();
            }
        }
        let _ = fs::remove_file(&self.socket_path);
    }
}

/// Asks a running UI to focus its window. Returns `true` when that instance
/// acknowledged the request.
pub fn activate_running_ui() -> io::Result<bool> {
    let Some(runtime_directory) = env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
    else {
        return Ok(false);
    };
    activate_running_ui_in(&runtime_directory)
}

fn activate_running_ui_in(runtime_directory: &Path) -> io::Result<bool> {
    require_owned_runtime_directory(runtime_directory)?;
    let socket_path = runtime_directory.join(ACTIVATION_SOCKET_FILE);
    let mut stream = match UnixStream::connect(&socket_path) {
        Ok(stream) => stream,
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
            ) =>
        {
            return Ok(false);
        }
        Err(error) => return Err(error),
    };
    stream.set_write_timeout(Some(ACTIVATION_IO_TIMEOUT))?;
    stream.set_read_timeout(Some(ACTIVATION_IO_TIMEOUT))?;
    let token = env::var("XDG_ACTIVATION_TOKEN").ok().filter(|token| {
        !token.is_empty()
            && !token.contains('\n')
            && !token.contains('\r')
            && token.len() < MAX_ACTIVATION_LINE_BYTES
    });
    writeln!(
        stream,
        "{}",
        match token.as_deref() {
            Some(token) => format!("activate {token}"),
            None => "activate".to_owned(),
        }
    )?;
    Ok(read_capped_line(&stream)? == "ok")
}

fn require_owned_runtime_directory(runtime_directory: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(runtime_directory)?;
    if !metadata.is_dir() || metadata.uid() != current_uid() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "runtime path is not an owned directory",
        ));
    }
    Ok(())
}

#[cfg(feature = "ui")]
fn prepare_activation_socket_path(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
        Ok(metadata) => {
            if metadata.file_type().is_symlink() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "activation socket path is a symlink",
                ));
            }
            if metadata.file_type().is_socket() {
                fs::remove_file(path)
            } else {
                Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "activation path exists and is not a socket",
                ))
            }
        }
    }
}

#[cfg(feature = "ui")]
fn serve_activation_socket(listener: UnixListener, events: async_channel::Sender<UiActivation>) {
    loop {
        let Ok((stream, _)) = listener.accept() else {
            break;
        };
        let _ = stream.set_read_timeout(Some(ACTIVATION_IO_TIMEOUT));
        let _ = stream.set_write_timeout(Some(ACTIVATION_IO_TIMEOUT));
        match read_activation_request(&stream) {
            Ok(ActivationRequest::Shutdown) => break,
            Ok(ActivationRequest::Activate(token)) => {
                let _ = writeln!(&stream, "ok");
                if events.send_blocking(UiActivation { token }).is_err() {
                    break;
                }
            }
            Err(_) => {
                let _ = writeln!(&stream, "error");
            }
        }
    }
}

#[cfg(feature = "ui")]
enum ActivationRequest {
    Activate(Option<String>),
    Shutdown,
}

#[cfg(feature = "ui")]
fn read_activation_request(stream: &UnixStream) -> io::Result<ActivationRequest> {
    parse_activation_request(&read_capped_line(stream)?)
}

fn read_capped_line(stream: &UnixStream) -> io::Result<String> {
    let mut line = String::new();
    let read =
        BufReader::new(stream.take(MAX_ACTIVATION_LINE_BYTES as u64 + 1)).read_line(&mut line)?;
    if read == 0 || line.len() > MAX_ACTIVATION_LINE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "activation request was empty or too large",
        ));
    }
    if line.ends_with('\n') {
        line.pop();
        if line.ends_with('\r') {
            line.pop();
        }
    }
    Ok(line)
}

#[cfg(feature = "ui")]
fn parse_activation_request(line: &str) -> io::Result<ActivationRequest> {
    match line.split_once(' ') {
        None if line == "activate" => Ok(ActivationRequest::Activate(None)),
        None if line == "shutdown" => Ok(ActivationRequest::Shutdown),
        Some(("activate", token)) if !token.is_empty() => {
            Ok(ActivationRequest::Activate(Some(token.to_owned())))
        }
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "activation request was not recognized",
        )),
    }
}

fn mark_lease_capable_process() -> io::Result<()> {
    fs::write("/proc/self/comm", format!("{LEASE_CAPABLE_PROCESS_NAME}\n"))
}
fn peer_ui_predates_lease(peer_directory: &Path) -> bool {
    peer_directory.join("cagents-ui").exists() && !peer_directory.join(LEASE_MARKER_FILE).is_file()
}

fn legacy_ui_process_running(proc_root: &Path) -> io::Result<bool> {
    for entry in fs::read_dir(proc_root)? {
        let Ok(entry) = entry else {
            continue;
        };
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        if pid == process::id() {
            continue;
        }
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if metadata.uid() != current_uid() {
            continue;
        }
        let Ok(name) = fs::read_to_string(entry.path().join("comm")) else {
            continue;
        };
        if name.trim_end() == LEGACY_PROCESS_NAME {
            return Ok(true);
        }
    }
    Ok(false)
}

fn current_uid() -> u32 {
    // SAFETY: geteuid has no preconditions and no failure state.
    unsafe { libc::geteuid() }
}

// Source-tree debug builds are development instances by default. The explicit
// feature keeps the same isolation available to optimized development builds.
const IS_DEVELOPMENT: bool = cfg!(any(debug_assertions, feature = "development"));

pub const NAME: &str = if IS_DEVELOPMENT {
    "development"
} else {
    "production"
};

pub const APPLICATION_ID: &str = if IS_DEVELOPMENT {
    "dev.cagents.Cagents.Development"
} else if cfg!(feature = "full") {
    "dev.cagents.Cagents"
} else {
    "dev.cagents.Cagents.Browserless"
};

pub const APPLICATION_NAME: &str = if IS_DEVELOPMENT {
    "cagents (Development)"
} else {
    "cagents"
};

pub const STORAGE_NAME: &str = if IS_DEVELOPMENT {
    "cagents-development"
} else {
    "cagents"
};

pub const RUNTIME_SUBDIRECTORY: &str = if IS_DEVELOPMENT {
    "cagents-development"
} else {
    "cagents"
};

pub fn project_dirs() -> Option<ProjectDirs> {
    ProjectDirs::from("dev", "cagents", STORAGE_NAME)
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::symlink;

    use tempfile::tempdir;

    use super::*;

    #[test]
    fn one_process_lease_guards_the_shared_session_store() {
        let runtime = tempdir().unwrap();
        let first = ApplicationLease::acquire_in(runtime.path()).unwrap();

        assert!(matches!(
            ApplicationLease::acquire_in(runtime.path()),
            Err(ApplicationLeaseError::AlreadyRunning)
        ));

        drop(first);
        ApplicationLease::acquire_in(runtime.path()).unwrap();
    }

    #[test]
    fn application_lease_rejects_a_symlinked_lock() {
        let runtime = tempdir().unwrap();
        let target = runtime.path().join("target");
        File::create(&target).unwrap();
        symlink(&target, runtime.path().join(APPLICATION_LOCK_FILE)).unwrap();

        assert!(ApplicationLease::acquire_in(runtime.path()).is_err());
    }

    #[test]
    fn legacy_ui_detection_ignores_unrelated_process_names() {
        let proc_root = tempdir().unwrap();
        let legacy = proc_root.path().join("4294967294");
        fs::create_dir(&legacy).unwrap();
        fs::write(legacy.join("comm"), "unrelated\n").unwrap();
        assert!(!legacy_ui_process_running(proc_root.path()).unwrap());

        fs::write(legacy.join("comm"), "cagents-ui\n").unwrap();
        assert!(legacy_ui_process_running(proc_root.path()).unwrap());
    }

    #[test]
    fn installed_peer_ui_requires_the_shared_lease_marker() {
        let peer = tempdir().unwrap();
        assert!(!peer_ui_predates_lease(peer.path()));

        File::create(peer.path().join("cagents-ui")).unwrap();
        assert!(peer_ui_predates_lease(peer.path()));

        File::create(peer.path().join(LEASE_MARKER_FILE)).unwrap();
        assert!(!peer_ui_predates_lease(peer.path()));
    }

    #[cfg(any(debug_assertions, feature = "development"))]
    #[test]
    fn debug_and_explicit_development_builds_use_development_namespaces() {
        assert_eq!(
            (
                NAME,
                APPLICATION_ID,
                APPLICATION_NAME,
                STORAGE_NAME,
                RUNTIME_SUBDIRECTORY,
                APPLICATION_LOCK_FILE,
                ACTIVATION_SOCKET_FILE,
            ),
            (
                "development",
                "dev.cagents.Cagents.Development",
                "cagents (Development)",
                "cagents-development",
                "cagents-development",
                "cagents-development-ui.lock",
                "cagents-development-ui.activate",
            )
        );
    }

    #[cfg(feature = "ui")]
    #[test]
    fn activation_request_parses_focus_tokens_and_rejects_noise() {
        assert!(matches!(
            parse_activation_request("activate").unwrap(),
            ActivationRequest::Activate(None)
        ));
        assert!(matches!(
            parse_activation_request("activate wayland-token").unwrap(),
            ActivationRequest::Activate(Some(token)) if token == "wayland-token"
        ));
        assert!(matches!(
            parse_activation_request("shutdown").unwrap(),
            ActivationRequest::Shutdown
        ));
        assert!(parse_activation_request("open").is_err());
        assert!(parse_activation_request("activate ").is_err());
    }

    #[cfg(feature = "ui")]
    #[test]
    fn activation_socket_focuses_a_running_listener() {
        let runtime = tempdir().unwrap();
        let (sender, receiver) = async_channel::unbounded();
        let _listener = UiActivationListener::bind_in(runtime.path(), sender).unwrap();

        assert!(activate_running_ui_in(runtime.path()).unwrap());
        let activation = receiver.recv_blocking().unwrap();
        assert_eq!(activation.token, None);

        let mut stream = UnixStream::connect(runtime.path().join(ACTIVATION_SOCKET_FILE)).unwrap();
        writeln!(stream, "activate wayland-token").unwrap();
        assert_eq!(read_capped_line(&stream).unwrap(), "ok");
        let activation = receiver.recv_blocking().unwrap();
        assert_eq!(activation.token.as_deref(), Some("wayland-token"));
    }

    #[cfg(feature = "ui")]
    #[test]
    fn activation_replaces_a_stale_socket_and_rejects_a_symlink() {
        let runtime = tempdir().unwrap();
        let leftover = UnixListener::bind(runtime.path().join(ACTIVATION_SOCKET_FILE)).unwrap();
        drop(leftover);
        let (sender, receiver) = async_channel::unbounded();
        let _listener = UiActivationListener::bind_in(runtime.path(), sender).unwrap();
        assert!(activate_running_ui_in(runtime.path()).unwrap());
        assert_eq!(receiver.recv_blocking().unwrap().token, None);

        let runtime = tempdir().unwrap();
        let target = runtime.path().join("target");
        File::create(&target).unwrap();
        symlink(&target, runtime.path().join(ACTIVATION_SOCKET_FILE)).unwrap();
        let (sender, _receiver) = async_channel::unbounded();
        assert!(UiActivationListener::bind_in(runtime.path(), sender).is_err());
    }

    #[test]
    fn activation_is_absent_when_no_ui_is_listening() {
        let runtime = tempdir().unwrap();
        assert!(!activate_running_ui_in(runtime.path()).unwrap());
    }

    #[cfg(not(any(debug_assertions, feature = "development")))]
    #[test]
    fn optimized_builds_without_development_feature_use_production_namespaces() {
        let application_id = if cfg!(feature = "full") {
            "dev.cagents.Cagents"
        } else {
            "dev.cagents.Cagents.Browserless"
        };
        assert_eq!(
            (
                NAME,
                APPLICATION_ID,
                APPLICATION_NAME,
                STORAGE_NAME,
                RUNTIME_SUBDIRECTORY,
                APPLICATION_LOCK_FILE,
                ACTIVATION_SOCKET_FILE,
            ),
            (
                "production",
                application_id,
                "cagents",
                "cagents",
                "cagents",
                "cagents-ui.lock",
                "cagents-ui.activate",
            )
        );
    }
}
