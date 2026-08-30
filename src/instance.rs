use std::{
    env,
    fs::{self, File, OpenOptions},
    io::{self, Write},
    os::{
        fd::AsRawFd,
        unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    },
    path::{Path, PathBuf},
    process,
};

use thiserror::Error;

const APPLICATION_LOCK_FILE: &str = "cagents-ui.lock";
const PRIVATE_FILE_MODE: u32 = 0o600;
const LEASE_CAPABLE_PROCESS_NAME: &str = "cagents-ui-v2";
const LEGACY_PROCESS_NAME: &str = "cagents-ui";
const LEASE_MARKER_FILE: &str = ".cagents-ui-lease-v1";
#[cfg(feature = "integrated-browser")]
const PEER_PACKAGE_DIRECTORY: &str = "/usr/libexec/cagents-browserless";
#[cfg(not(feature = "integrated-browser"))]
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

/// Exclusive process lease protecting the session store shared by all variants.
pub struct ApplicationLease {
    _lock: File,
}

impl ApplicationLease {
    pub fn acquire() -> Result<Self, ApplicationLeaseError> {
        let peer_directory = Path::new(PEER_PACKAGE_DIRECTORY);
        if peer_ui_predates_lease(peer_directory) {
            return Err(ApplicationLeaseError::IncompatiblePeerPackage {
                path: peer_directory.join("cagents-ui"),
            });
        }
        mark_lease_capable_process()?;
        let runtime_directory = env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
            .ok_or(ApplicationLeaseError::MissingRuntimeDirectory)?;
        let lease = Self::acquire_in(&runtime_directory)?;
        if legacy_ui_process_running(Path::new("/proc"))? {
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

use directories::ProjectDirs;

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
} else if cfg!(feature = "integrated-browser") {
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

    #[test]
    fn instance_namespaces_are_internally_consistent() {
        if IS_DEVELOPMENT {
            assert_eq!(
                (
                    NAME,
                    APPLICATION_ID,
                    APPLICATION_NAME,
                    STORAGE_NAME,
                    RUNTIME_SUBDIRECTORY
                ),
                (
                    "development",
                    "dev.cagents.Cagents.Development",
                    "cagents (Development)",
                    "cagents-development",
                    "cagents-development"
                )
            );
        } else {
            let application_id = if cfg!(feature = "integrated-browser") {
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
                    RUNTIME_SUBDIRECTORY
                ),
                ("production", application_id, "cagents", "cagents", "cagents")
            );
        }
    }
}
