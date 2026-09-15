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
    time::{Duration, SystemTime},
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

pub const PRIMARY_WINDOW: &str = "primary";
static WINDOW_SLOT: std::sync::OnceLock<String> = std::sync::OnceLock::new();

pub fn window_slot() -> &'static str {
    WINDOW_SLOT
        .get()
        .map(String::as_str)
        .unwrap_or(PRIMARY_WINDOW)
}

pub fn validate_window_slot(slot: &str) -> io::Result<()> {
    if slot == "update"
        || slot.is_empty()
        || slot.len() > 40
        || !slot
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "window name must contain 1–40 ASCII letters, digits, hyphens or underscores",
        ));
    }
    Ok(())
}

fn runtime_directory() -> io::Result<PathBuf> {
    let path = env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "XDG_RUNTIME_DIR is not absolute")
        })?;
    require_owned_runtime_directory(&path)?;
    Ok(path)
}

fn slot_lock_name(slot: &str) -> String {
    format!("{APPLICATION_LOCK_FILE}.{slot}")
}

fn slot_socket_name(slot: &str) -> String {
    if slot == PRIMARY_WINDOW {
        ACTIVATION_SOCKET_FILE.into()
    } else {
        format!("{ACTIVATION_SOCKET_FILE}.{slot}")
    }
}

fn owned_lock(path: &Path, operation: i32) -> io::Result<File> {
    let lock = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .mode(PRIVATE_FILE_MODE)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)?;
    let metadata = lock.metadata()?;
    if !metadata.is_file() || metadata.uid() != current_uid() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "lock is not an owned regular file",
        ));
    }
    lock.set_permissions(fs::Permissions::from_mode(PRIVATE_FILE_MODE))?;
    // SAFETY: the descriptor remains owned by the returned file.
    if unsafe { libc::flock(lock.as_raw_fd(), operation) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(lock)
}

/// A slot owns its full workspace snapshot; terminal identifiers still refer to
/// the one shared daemon. The old global lock remains held shared, so older
/// exclusive-lease binaries cannot open any of these stores concurrently.
pub struct WindowLease {
    _legacy_fence: File,
    _slot: File,
}

impl Drop for WindowLease {
    fn drop(&mut self) {
        // A concurrent fork may still hold the description before exec. Release
        // ownership explicitly, as ApplicationLease does, rather than waiting
        // for every inherited descriptor to close.
        for lock in [&self._slot, &self._legacy_fence] {
            // SAFETY: both owned descriptors remain live throughout Drop.
            let _ = unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_UN) };
        }
    }
}

impl WindowLease {
    pub fn acquire(new_window: bool, requested: Option<&str>) -> io::Result<Self> {
        let (lease, slot) = Self::acquire_in(&runtime_directory()?, new_window, requested)?;
        WINDOW_SLOT
            .set(slot)
            .map_err(|_| io::Error::other("window slot already initialized"))?;
        Ok(lease)
    }

    fn acquire_in(
        runtime: &Path,
        new_window: bool,
        requested: Option<&str>,
    ) -> io::Result<(Self, String)> {
        require_owned_runtime_directory(runtime)?;
        let _launch = owned_lock(
            &runtime.join(format!("{APPLICATION_LOCK_FILE}.update")),
            libc::LOCK_SH | libc::LOCK_NB,
        )
        .map_err(|error| {
            io::Error::new(error.kind(), format!("an update is in progress: {error}"))
        })?;
        if !IS_DEVELOPMENT && peer_ui_predates_lease(Path::new(PEER_PACKAGE_DIRECTORY)) {
            return Err(io::Error::other(
                "installed peer UI predates the shared-session lease; upgrade that package first",
            ));
        }
        mark_lease_capable_process()?;
        let legacy = owned_lock(
            &runtime.join(APPLICATION_LOCK_FILE),
            libc::LOCK_SH | libc::LOCK_NB,
        )?;
        if !IS_DEVELOPMENT && legacy_ui_process_running(Path::new("/proc"))? {
            return Err(io::Error::other(ApplicationLeaseError::AlreadyRunning));
        }
        if let Some(slot) = requested {
            validate_window_slot(slot)?;
        }
        for index in 1..=1024 {
            let slot = requested.map(str::to_owned).unwrap_or_else(|| {
                if !new_window {
                    PRIMARY_WINDOW.into()
                } else {
                    format!("window-{}", index + 1)
                }
            });
            match owned_lock(
                &runtime.join(slot_lock_name(&slot)),
                libc::LOCK_EX | libc::LOCK_NB,
            ) {
                Ok(lock) => {
                    return Ok((
                        Self {
                            _legacy_fence: legacy,
                            _slot: lock,
                        },
                        slot,
                    ));
                }
                Err(error)
                    if error.kind() == io::ErrorKind::WouldBlock
                        && new_window
                        && requested.is_none() => {}
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    return Err(io::Error::other(ApplicationLeaseError::AlreadyRunning));
                }
                Err(error) => return Err(error),
            }
        }
        Err(io::Error::other("all window slots are occupied"))
    }
}

pub fn ui_launch_handshake_requested() -> bool {
    env::var_os(UI_LAUNCH_HANDSHAKE_ENV).as_deref() == Some("1".as_ref())
}

pub fn report_ui_startup(result: &Result<(), String>) -> io::Result<()> {
    static REPORTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if ui_launch_handshake_requested() && !REPORTED.swap(true, std::sync::atomic::Ordering::AcqRel)
    {
        let message = match result {
            Ok(()) => UI_LAUNCH_READY.to_owned(),
            Err(error) => format!("{UI_LAUNCH_ERROR_PREFIX}{error}").replace('\n', " "),
        };
        let mut stdout = io::stdout().lock();
        writeln!(stdout, "{message}")?;
        stdout.flush()?;
    }
    Ok(())
}

/// Persist an uncaught UI panic with enough context to diagnose a native crash
/// after the compositor has closed the process's stderr.
pub fn report_ui_panic(
    message: &str,
    thread_name: Option<&str>,
    location: Option<&std::panic::Location<'static>>,
    backtrace: &std::backtrace::Backtrace,
) -> io::Result<()> {
    let dirs = project_dirs().ok_or_else(|| io::Error::other("data directory unavailable"))?;
    fs::create_dir_all(dirs.data_dir())?;
    let path = dirs.data_dir().join("ui-crash.log");
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(PRIVATE_FILE_MODE)
        .open(path)?;
    writeln!(
        file,
        "--- cagents UI panic pid={} at {:?} thread={} ---",
        process::id(),
        SystemTime::now(),
        thread_name.unwrap_or("<unnamed>"),
    )?;
    if let Some(location) = location {
        writeln!(
            file,
            "location: {}:{}:{}",
            location.file(),
            location.line(),
            location.column()
        )?;
    }
    writeln!(file, "message: {message}")?;
    writeln!(file, "backtrace:\n{backtrace}")?;
    file.flush()
}

pub fn window_database_path(slot: &str) -> io::Result<PathBuf> {
    validate_window_slot(slot)?;
    let dirs = project_dirs().ok_or_else(|| io::Error::other("data directory unavailable"))?;
    Ok(if slot == PRIMARY_WINDOW {
        dirs.data_dir().join("sessions.sqlite3")
    } else {
        dirs.data_dir()
            .join("windows")
            .join(slot)
            .join("sessions.sqlite3")
    })
}

pub fn window_database_paths() -> io::Result<Vec<PathBuf>> {
    let primary = window_database_path(PRIMARY_WINDOW)?;
    let mut paths = vec![primary.clone()];
    let windows = primary.parent().unwrap().join("windows");
    match fs::read_dir(windows) {
        Ok(entries) => {
            for entry in entries {
                let entry = entry?;
                if entry.file_type()?.is_dir()
                    && let Some(slot) = entry.file_name().to_str()
                    && validate_window_slot(slot).is_ok()
                {
                    let path = entry.path().join("sessions.sqlite3");
                    if path.is_file() {
                        paths.push(path);
                    }
                }
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    paths.sort();
    Ok(paths)
}

fn running_window_slots(runtime: &Path) -> io::Result<Vec<String>> {
    require_owned_runtime_directory(runtime)?;
    let prefix = format!("{APPLICATION_LOCK_FILE}.");
    let mut slots = Vec::new();
    for entry in fs::read_dir(runtime)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(slot) = name.to_str().and_then(|name| name.strip_prefix(&prefix)) else {
            continue;
        };
        if slot == "update" || validate_window_slot(slot).is_err() {
            continue;
        }
        match owned_lock(&entry.path(), libc::LOCK_EX | libc::LOCK_NB) {
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => slots.push(slot.to_owned()),
            Err(error) => return Err(error),
        }
    }
    slots.sort();
    Ok(slots)
}

pub fn update_in_progress() -> bool {
    runtime_directory()
        .and_then(|runtime| {
            owned_lock(
                &runtime.join(format!("{APPLICATION_LOCK_FILE}.update")),
                libc::LOCK_SH | libc::LOCK_NB,
            )
        })
        .is_err()
}

fn update_window(runtime: &Path, slot: &str, action: &str, timeout: Duration) -> io::Result<()> {
    let mut stream = UnixStream::connect(runtime.join(slot_socket_name(slot)))?;
    stream.set_read_timeout(Some(timeout.max(Duration::from_millis(1))))?;
    stream.set_write_timeout(Some(ACTIVATION_IO_TIMEOUT))?;
    writeln!(stream, "{action}")?;
    let response = read_capped_line(&stream)?;
    if response == "ok" {
        Ok(())
    } else {
        Err(io::Error::other(format!("window {slot}: {response}")))
    }
}

/// Holds a launch fence while a detached updater cooperatively drains windows.
/// No signal is sent to UI processes. Preparation is reversible; a dirty editor
/// or failed durable save cancels the entire update before any commit is sent.
pub struct UiUpdateGuard {
    lock: Option<File>,
    runtime: PathBuf,
    _legacy_fence: File,
    prepared: Vec<String>,
    closed: Vec<String>,
}

impl UiUpdateGuard {
    pub fn acquire() -> io::Result<Self> {
        let runtime = runtime_directory()?;
        let lock = owned_lock(
            &runtime.join(format!("{APPLICATION_LOCK_FILE}.update")),
            libc::LOCK_EX | libc::LOCK_NB,
        )?;
        let legacy = owned_lock(
            &runtime.join(APPLICATION_LOCK_FILE),
            libc::LOCK_SH | libc::LOCK_NB,
        )
        .map_err(|error| {
            io::Error::new(
                error.kind(),
                "an older UI is running; close it before updating",
            )
        })?;
        Ok(Self {
            lock: Some(lock),
            _legacy_fence: legacy,
            runtime,
            prepared: Vec::new(),
            closed: Vec::new(),
        })
    }

    pub fn prepare_and_close_windows(&mut self, timeout: Duration) -> io::Result<Vec<String>> {
        let deadline = std::time::Instant::now() + timeout;
        let slots = running_window_slots(&self.runtime)?;
        for slot in &slots {
            // Include a possibly timed-out participant in cancellation.
            self.prepared.push(slot.clone());
            update_window(
                &self.runtime,
                slot,
                "update-prepare",
                deadline.saturating_duration_since(std::time::Instant::now()),
            )?;
        }
        for slot in &slots {
            // A lost ACK cannot prove that the UI did not accept an irreversible close.
            self.closed.push(slot.clone());
            update_window(
                &self.runtime,
                slot,
                "update-commit",
                deadline.saturating_duration_since(std::time::Instant::now()),
            )?;
        }
        while !running_window_slots(&self.runtime)?.is_empty() {
            if std::time::Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "windows did not finish saving and closing; update cancelled",
                ));
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        self.prepared.clear();
        Ok(slots)
    }

    pub fn relaunch_windows(mut self, slots: &[String], executable: &Path) -> io::Result<()> {
        self.prepared.clear();
        self.release_launch_fence()?;
        self.closed.clear();
        let mut errors = Vec::new();
        for slot in slots {
            if let Err(error) = launch_window_at(executable, false, Some(slot), None) {
                errors.push(format!("{slot}: {error}"));
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(io::Error::other(errors.join("; ")))
        }
    }

    fn release_launch_fence(&mut self) -> io::Result<()> {
        if let Some(lock) = &self.lock {
            // A child between fork and exec must not extend this transaction.
            lock.unlock()?;
        }
        self.lock.take();
        Ok(())
    }
}

impl Drop for UiUpdateGuard {
    fn drop(&mut self) {
        for slot in &self.prepared {
            if update_window(&self.runtime, slot, "update-cancel", Duration::from_secs(2)).is_ok() {
                // A UI that already accepted Commit must refuse cancellation.
                self.closed.retain(|candidate| candidate != slot);
            }
        }
        // A committed close is already on its bounded LSP teardown path.
        // Wait for that lease before reopening, rather than racing a still-live
        // process and silently losing its relaunch.
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        while !self.closed.is_empty()
            && running_window_slots(&self.runtime)
                .is_ok_and(|live| self.closed.iter().any(|slot| live.contains(slot)))
            && std::time::Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(25));
        }
        let _ = self.release_launch_fence();
        for slot in &self.closed {
            let _ = launch_window(false, Some(slot), None);
        }
        let _ = self._legacy_fence.unlock();
    }
}

/// Starts another independent process using the same installed UI executable.
pub fn launch_window(new_window: bool, slot: Option<&str>, source: Option<&str>) -> io::Result<()> {
    let executable = env::current_exe()?.with_file_name("cagents-ui");
    launch_window_at(&executable, new_window, slot, source)
}

fn launch_window_at(
    executable: &Path,
    new_window: bool,
    slot: Option<&str>,
    source: Option<&str>,
) -> io::Result<()> {
    if let Some(slot) = slot {
        validate_window_slot(slot)?;
    }
    if let Some(source) = source {
        validate_window_slot(source)?;
    }
    let mut command = process::Command::new(executable);
    if new_window {
        command.arg("--new-window");
    }
    if let Some(slot) = slot {
        command.args(["--window", slot]);
    }
    if let Some(source) = source {
        command.args(["--from-window", source]);
    }
    crate::control::spawn_ui_process(&mut command)
}

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
#[derive(Debug, Clone)]
pub struct UiActivation {
    pub token: Option<String>,
    pub update: Option<UiUpdateRequest>,
}

#[cfg(feature = "ui")]
#[derive(Debug, Clone)]
pub struct UiUpdateRequest {
    pub action: UiUpdateAction,
    pub response: std::sync::mpsc::SyncSender<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UiUpdateAction {
    Prepare,
    Cancel,
    Commit,
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

        let socket_path = runtime_directory.join(slot_socket_name(window_slot()));
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
    if activate_running_ui_in(&runtime_directory)? {
        return Ok(true);
    }
    for slot in running_window_slots(&runtime_directory)? {
        if activate_window_in(&runtime_directory, &slot)? {
            return Ok(true);
        }
    }
    Ok(false)
}

fn activate_running_ui_in(runtime_directory: &Path) -> io::Result<bool> {
    activate_window_in(runtime_directory, PRIMARY_WINDOW)
}

pub fn activate_window(slot: &str) -> io::Result<bool> {
    validate_window_slot(slot)?;
    activate_window_in(&runtime_directory()?, slot)
}

fn activate_window_in(runtime_directory: &Path, slot: &str) -> io::Result<bool> {
    require_owned_runtime_directory(runtime_directory)?;
    let socket_path = runtime_directory.join(slot_socket_name(slot));
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
                if events
                    .send_blocking(UiActivation {
                        token,
                        update: None,
                    })
                    .is_err()
                {
                    break;
                }
            }
            Ok(ActivationRequest::Update(action)) => {
                let (response, receiver) = std::sync::mpsc::sync_channel(1);
                if events
                    .send_blocking(UiActivation {
                        token: None,
                        update: Some(UiUpdateRequest { action, response }),
                    })
                    .is_err()
                {
                    break;
                }
                let response = receiver
                    .recv_timeout(Duration::from_secs(30))
                    .unwrap_or_else(|_| "error: window did not respond".into());
                let _ = writeln!(&stream, "{response}");
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
    Update(UiUpdateAction),
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
        None if line == "update-prepare" => Ok(ActivationRequest::Update(UiUpdateAction::Prepare)),
        None if line == "update-cancel" => Ok(ActivationRequest::Update(UiUpdateAction::Cancel)),
        None if line == "update-commit" => Ok(ActivationRequest::Update(UiUpdateAction::Commit)),
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
    fn window_leases_allow_multiple_slots_and_reuse_only_closed_slots() {
        let runtime = tempdir().unwrap();
        let (primary, name) = WindowLease::acquire_in(runtime.path(), false, None).unwrap();
        assert_eq!(name, PRIMARY_WINDOW);
        let (second, name) = WindowLease::acquire_in(runtime.path(), true, None).unwrap();
        assert_eq!(name, "window-2");
        assert!(WindowLease::acquire_in(runtime.path(), false, Some("window-2")).is_err());
        let (third, name) = WindowLease::acquire_in(runtime.path(), true, None).unwrap();
        assert_eq!(name, "window-3");
        assert!(ApplicationLease::acquire_in(runtime.path()).is_err());
        // Duplicate descriptions model a child between fork and exec.
        let _inherited_slot = second._slot.try_clone().unwrap();
        let _inherited_legacy_fence = second._legacy_fence.try_clone().unwrap();
        drop(second);
        let (reopened, name) = WindowLease::acquire_in(runtime.path(), true, None).unwrap();
        assert_eq!(name, "window-2");
        drop(primary);
        drop(third);
        drop(reopened);
        assert!(ApplicationLease::acquire_in(runtime.path()).is_ok());
    }

    #[test]
    fn update_launch_fence_blocks_new_windows_and_retains_old_version_fence() {
        let runtime = tempdir().unwrap();
        let lock = owned_lock(
            &runtime
                .path()
                .join(format!("{APPLICATION_LOCK_FILE}.update")),
            libc::LOCK_EX | libc::LOCK_NB,
        )
        .unwrap();
        let legacy =
            owned_lock(&runtime.path().join(APPLICATION_LOCK_FILE), libc::LOCK_SH).unwrap();
        let _inherited_update_fence = lock.try_clone().unwrap();
        let _inherited_legacy_fence = legacy.try_clone().unwrap();
        let guard = UiUpdateGuard {
            lock: Some(lock),
            runtime: runtime.path().into(),
            _legacy_fence: legacy,
            prepared: Vec::new(),
            closed: Vec::new(),
        };
        assert!(WindowLease::acquire_in(runtime.path(), true, None).is_err());
        drop(guard);
        let older = ApplicationLease::acquire_in(runtime.path()).unwrap();
        assert!(WindowLease::acquire_in(runtime.path(), true, None).is_err());
        drop(older);
        assert!(WindowLease::acquire_in(runtime.path(), true, None).is_ok());
    }

    #[test]
    fn one_window_veto_cancels_every_prepared_window_before_any_close() {
        let runtime = tempdir().unwrap();
        let lock = owned_lock(
            &runtime
                .path()
                .join(format!("{APPLICATION_LOCK_FILE}.update")),
            libc::LOCK_EX | libc::LOCK_NB,
        )
        .unwrap();
        let legacy =
            owned_lock(&runtime.path().join(APPLICATION_LOCK_FILE), libc::LOCK_SH).unwrap();
        let mut workers = Vec::new();
        for (slot, veto) in [(PRIMARY_WINDOW, false), ("window-2", true)] {
            let lease =
                owned_lock(&runtime.path().join(slot_lock_name(slot)), libc::LOCK_EX).unwrap();
            let listener =
                std::os::unix::net::UnixListener::bind(runtime.path().join(slot_socket_name(slot)))
                    .unwrap();
            workers.push(std::thread::spawn(move || {
                let _lease = lease;
                let mut actions = Vec::new();
                for _ in 0..2 {
                    let (stream, _) = listener.accept().unwrap();
                    let action = read_capped_line(&stream).unwrap();
                    let response = if veto && action == "update-prepare" {
                        "error: unsaved editor"
                    } else {
                        "ok"
                    };
                    writeln!(&stream, "{response}").unwrap();
                    actions.push(action);
                }
                actions
            }));
        }
        let mut guard = UiUpdateGuard {
            lock: Some(lock),
            _legacy_fence: legacy,
            runtime: runtime.path().into(),
            prepared: Vec::new(),
            closed: Vec::new(),
        };
        assert!(
            guard
                .prepare_and_close_windows(Duration::from_secs(5))
                .unwrap_err()
                .to_string()
                .contains("unsaved editor")
        );
        drop(guard);
        for worker in workers {
            assert_eq!(worker.join().unwrap(), ["update-prepare", "update-cancel"]);
        }
    }

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
